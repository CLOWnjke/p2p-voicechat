//! Захват микрофона и воспроизведение.
//!
//! Внутри всё живёт на 48 кГц моно — это родной режим Opus. Звуковая карта
//! может работать на другой частоте (на Windows сплошь и рядом 44 100), поэтому
//! на входе и выходе стоит простой линейный ресемплер.

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use decibri_aec::{Aec, AecConfig};
use nnnoiseless::DenoiseState;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

pub const SAMPLE_RATE: u32 = 48_000;
/// 20 мс — стандартный размер кадра для голоса в Opus.
pub const FRAME: usize = 960;
/// 10 мс — кадр RNNoise. Ровно половина нашего, так что делится без остатка.
const DENOISE_FRAME: usize = DenoiseState::FRAME_SIZE;

/// Сколько звука держим в буфере на каждого собеседника, прежде чем начать
/// выбрасывать. 100 мс: больше — заметная задержка, меньше — заикания.
const MAX_TRACK_SAMPLES: usize = SAMPLE_RATE as usize / 10;

/// Очередь того, что уходит в динамики. Эхоподавителю нужен опорный сигнал:
/// без него он не знает, что именно вычитать из микрофона.
pub type Reference = Arc<Mutex<VecDeque<f32>>>;

/// Больше секунды опорного сигнала держать незачем.
const MAX_REFERENCE: usize = SAMPLE_RATE as usize;

/// На сколько кадров ворота заглядывают вперёд, прежде чем выпустить звук.
/// Без этого начало слова срезается: пока детектор поймёт, что началась речь,
/// первые миллисекунды уже ушли наружу закрытыми. Цена — 20 мс задержки.
const GATE_LOOKAHEAD: usize = 2;
/// Сколько кадров держим ворота открытыми после того, как речь пропала.
/// 250 мс: хвосты слов и глухие согласные не должны обрубаться.
const GATE_HANGOVER: u32 = 25;
/// Открываемся за 5 мс, закрываемся за 40 — плавно, иначе слышны щелчки.
const GATE_ATTACK: f32 = 1.0 / 240.0;
const GATE_RELEASE: f32 = 1.0 / 1920.0;

/// Ворота, пропускающие только голос.
///
/// Работают не по громкости, а по вероятности речи: хлопок в ладоши громкий,
/// но на речь не похож, поэтому любой порог по уровню он проходит, а эти
/// ворота — нет. Порог громкости оставлен вторым условием, чтобы отсекать
/// тихие срабатывания детектора.
struct VoiceGate {
    frames: VecDeque<[f32; DENOISE_FRAME]>,
    probs: VecDeque<f32>,
    peaks: VecDeque<f32>,
    hold: u32,
    gain: f32,
}

impl VoiceGate {
    fn new() -> Self {
        Self {
            frames: VecDeque::with_capacity(GATE_LOOKAHEAD + 2),
            probs: VecDeque::with_capacity(GATE_LOOKAHEAD + 2),
            peaks: VecDeque::with_capacity(GATE_LOOKAHEAD + 2),
            hold: 0,
            gain: 0.0,
        }
    }

    /// Кладёт кадр в линию задержки и, когда та наполнилась, дописывает
    /// в `out` самый старый кадр — уже с применённым усилением.
    fn push(&mut self, frame: &[f32], prob: f32, open_thr: f32, floor: f32, out: &mut Vec<f32>) {
        let mut buf = [0f32; DENOISE_FRAME];
        buf.copy_from_slice(frame);
        let peak = frame.iter().fold(0.0f32, |a, s| a.max(s.abs()));

        self.frames.push_back(buf);
        self.probs.push_back(prob);
        self.peaks.push_back(peak);

        if self.frames.len() <= GATE_LOOKAHEAD {
            return;
        }

        // Решение принимается по всему окну, включая кадры, которые ещё не
        // прозвучали: если речь начнётся через кадр, ворота откроются заранее.
        let voiced = self
            .probs
            .iter()
            .zip(self.peaks.iter())
            .any(|(p, pk)| *p >= open_thr && *pk >= floor);

        if voiced {
            self.hold = GATE_HANGOVER;
        } else if self.hold > 0 {
            self.hold -= 1;
        }
        let target = if self.hold > 0 { 1.0 } else { 0.0 };

        let oldest = self.frames.pop_front().unwrap();
        self.probs.pop_front();
        self.peaks.pop_front();

        for s in oldest {
            if self.gain < target {
                self.gain = (self.gain + GATE_ATTACK).min(target);
            } else if self.gain > target {
                self.gain = (self.gain - GATE_RELEASE).max(target);
            }
            out.push(s * self.gain);
        }
    }
}

/// Смешивает дорожки всех собеседников в один поток на выход.
pub struct Mixer {
    tracks: Mutex<HashMap<u16, VecDeque<f32>>>,
}

impl Mixer {
    pub fn new() -> Self {
        Self {
            tracks: Mutex::new(HashMap::new()),
        }
    }

    pub fn push(&self, peer: u16, samples: &[f32]) {
        let mut tracks = self.tracks.lock().unwrap();
        let track = tracks.entry(peer).or_default();
        track.extend(samples.iter().copied());
        // Если приёмник не успевает, лучше потерять старое, чем растить задержку.
        while track.len() > MAX_TRACK_SAMPLES {
            track.pop_front();
        }
    }

    pub fn remove(&self, peer: u16) {
        self.tracks.lock().unwrap().remove(&peer);
    }

    /// Оставляет дорожки только тех, кто ещё в комнате. Иначе после ухода
    /// человека его недоигранный хвост продолжал бы шуметь в микшере.
    pub fn retain(&self, present: &[u16]) {
        self.tracks
            .lock()
            .unwrap()
            .retain(|id, _| present.contains(id));
    }

    /// Забирает `out.len()` смешанных сэмплов. Молчание, если говорить некому.
    fn pull(&self, out: &mut [f32]) {
        out.fill(0.0);
        let mut tracks = self.tracks.lock().unwrap();
        for track in tracks.values_mut() {
            for slot in out.iter_mut() {
                match track.pop_front() {
                    Some(s) => *slot += s,
                    None => break,
                }
            }
        }
        // Мягкое ограничение вместо жёсткого клиппинга при нескольких говорящих.
        for s in out.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
    }
}

impl Default for Mixer {
    fn default() -> Self {
        Self::new()
    }
}

/// Линейный ресемплер с сохранением состояния между вызовами.
struct Resampler {
    ratio: f64,
    pos: f64,
    buf: Vec<f32>,
}

impl Resampler {
    fn new(from_rate: u32, to_rate: u32) -> Self {
        Self {
            ratio: from_rate as f64 / to_rate as f64,
            pos: 0.0,
            buf: Vec::new(),
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.buf.extend_from_slice(input);
        while (self.pos as usize) + 1 < self.buf.len() {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            out.push(self.buf[i] * (1.0 - frac) + self.buf[i + 1] * frac);
            self.pos += self.ratio;
        }
        let consumed = self.pos as usize;
        if consumed > 0 {
            self.buf.drain(..consumed);
            self.pos -= consumed as f64;
        }
    }
}

/// Уровень микрофона для полоски в интерфейсе (0.0 – 1.0), в виде бит f32.
pub type Level = Arc<AtomicU32>;

pub fn level_value(level: &Level) -> f32 {
    f32::from_bits(level.load(Ordering::Relaxed))
}

/// Ручки, за которые дёргает интерфейс, и показания, которые он читает.
#[derive(Clone)]
pub struct Controls {
    pub muted: Arc<AtomicBool>,
    pub denoise: Arc<AtomicBool>,
    pub aec: Arc<AtomicBool>,
    /// Пропускать только голос: хлопки, стук и щелчки не уходят наружу.
    pub gate: Arc<AtomicBool>,
    /// 0 — пропускать только уверенную речь, 1 — почти всё подряд.
    pub gate_sensitivity: Level,
    /// Нижний порог громкости: тише него не пропускаем даже похожее на речь.
    pub gate_floor: Level,
    /// Уровень уже обработанного сигнала — так видно, что шумодав делает.
    pub level: Level,
    /// Оценка «сейчас говорят», которую RNNoise выдаёт заодно с очисткой.
    pub voice: Level,
}

impl Controls {
    pub fn new() -> Self {
        Self {
            muted: Arc::new(AtomicBool::new(false)),
            denoise: Arc::new(AtomicBool::new(true)),
            aec: Arc::new(AtomicBool::new(true)),
            gate: Arc::new(AtomicBool::new(true)),
            gate_sensitivity: Arc::new(AtomicU32::new(0.5f32.to_bits())),
            gate_floor: Arc::new(AtomicU32::new(0.02f32.to_bits())),
            level: Arc::new(AtomicU32::new(0)),
            voice: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl Default for Controls {
    fn default() -> Self {
        Self::new()
    }
}

/// Имя устройства: в cpal 0.18 оно лежит внутри описания.
fn device_name(dev: &cpal::Device, fallback: &str) -> String {
    dev.description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| fallback.to_string())
}

pub struct AudioEngine {
    _input: Stream,
    _output: Stream,
    pub input_name: String,
    pub output_name: String,
}

/// Запускает захват и воспроизведение.
///
/// Готовые кадры по 960 сэмплов уходят в `frames_tx`; всё, что нужно проиграть,
/// кладётся в `mixer`.
pub fn start(
    frames_tx: SyncSender<Vec<i16>>,
    mixer: Arc<Mixer>,
    controls: Controls,
) -> Result<AudioEngine> {
    let host = cpal::default_host();

    let in_dev = host
        .default_input_device()
        .ok_or_else(|| anyhow!("не найден микрофон"))?;
    let out_dev = host
        .default_output_device()
        .ok_or_else(|| anyhow!("не найдено устройство вывода"))?;

    let input_name = device_name(&in_dev, "микрофон");
    let output_name = device_name(&out_dev, "динамики");

    let in_cfg = in_dev.default_input_config()?;
    let out_cfg = out_dev.default_output_config()?;

    let reference: Reference = Arc::new(Mutex::new(VecDeque::new()));

    let input = build_input(&in_dev, &in_cfg, frames_tx, controls, reference.clone())?;
    let output = build_output(&out_dev, &out_cfg, mixer, reference)?;

    input.play()?;
    output.play()?;

    Ok(AudioEngine {
        _input: input,
        _output: output,
        input_name,
        output_name,
    })
}

fn build_input(
    device: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    frames_tx: SyncSender<Vec<i16>>,
    controls: Controls,
    reference: Reference,
) -> Result<Stream> {
    let channels = cfg.channels() as usize;
    let mut resampler = Resampler::new(cfg.sample_rate(), SAMPLE_RATE);
    let mut mono: Vec<f32> = Vec::with_capacity(2048);
    // Свежие сэмплы этого вызова, уже на 48 кГц, до эхоподавления.
    let mut resampled: Vec<f32> = Vec::with_capacity(2048);
    let mut echo_free: Vec<f32> = Vec::with_capacity(2048);
    let mut ref_chunk: Vec<f32> = Vec::with_capacity(2048);

    let mut aec = {
        let mut config = AecConfig::default();
        config.sample_rate = SAMPLE_RATE;
        Aec::new(config).map_err(|e| anyhow!("эхоподавитель не завёлся: {e}"))?
    };
    // Сырой поток на 48 кГц, ещё не прошедший через шумодав.
    let mut raw: Vec<f32> = Vec::with_capacity(DENOISE_FRAME * 4);
    // Готовое к упаковке в Opus.
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME * 4);

    let mut denoiser = DenoiseState::new();
    let mut den_in = [0f32; DENOISE_FRAME];
    let mut den_out = [0f32; DENOISE_FRAME];
    let mut gate = VoiceGate::new();
    let mut norm = [0f32; DENOISE_FRAME];

    let err_fn = |e| eprintln!("ошибка входного потока: {e}");
    let stream_cfg: cpal::StreamConfig = cfg.config();

    let mut handle = move |samples: &[f32]| {
        // Сводим в моно: для голоса разница между каналами не нужна.
        mono.clear();
        for chunk in samples.chunks(channels) {
            mono.push(chunk.iter().sum::<f32>() / channels as f32);
        }

        // Сначала отдаём эхоподавителю всё, что успело уйти в динамики.
        ref_chunk.clear();
        {
            let mut r = reference.lock().unwrap();
            ref_chunk.extend(r.drain(..));
        }
        if !ref_chunk.is_empty() {
            aec.feed_reference(&ref_chunk);
        }

        resampled.clear();
        resampler.process(&mono, &mut resampled);

        // Порядок важен: сначала убираем эхо, потом шум. Эхоподавителю нужен
        // микрофон в том виде, в каком эхо в него пришло.
        echo_free.clear();
        if aec.process(&resampled, &mut echo_free).is_err() {
            echo_free.clear();
            echo_free.extend_from_slice(&resampled);
        }

        if controls.aec.load(Ordering::Relaxed) {
            raw.extend_from_slice(&echo_free);
        } else {
            raw.extend_from_slice(&resampled);
        }

        while raw.len() >= DENOISE_FRAME {
            // RNNoise ждёт сэмплы в шкале i16, а не в привычном диапазоне
            // от -1 до 1 — на этом обычно и спотыкаются при интеграции.
            for (dst, src) in den_in.iter_mut().zip(raw.drain(..DENOISE_FRAME)) {
                *dst = src * i16::MAX as f32;
            }

            // Считаем всегда, даже когда шумодав выключен: это около процента
            // ядра, зато нет артефактов при переключении и всегда под рукой
            // оценка «сейчас говорят».
            let voice = denoiser.process_frame(&mut den_out, &den_in);
            controls.voice.store(voice.to_bits(), Ordering::Relaxed);

            let source: &[f32] = if controls.denoise.load(Ordering::Relaxed) {
                &den_out
            } else {
                &den_in
            };

            // Возвращаемся из шкалы i16 в привычный диапазон.
            for (dst, src) in norm.iter_mut().zip(source) {
                *dst = src / i16::MAX as f32;
            }

            let written_from = pending.len();
            if controls.gate.load(Ordering::Relaxed) {
                let sens = f32::from_bits(controls.gate_sensitivity.load(Ordering::Relaxed));
                // Чувствительность 0 требует почти уверенной речи, 1 — почти ничего.
                let open_thr = 0.85 - 0.70 * sens.clamp(0.0, 1.0);
                let floor = f32::from_bits(controls.gate_floor.load(Ordering::Relaxed));
                gate.push(&norm, voice, open_thr, floor, &mut pending);
            } else {
                pending.extend_from_slice(&norm);
            }

            // Уровень снимается с того, что реально уходит наружу: так на
            // полоске видно и работу шумодава, и работу ворот.
            let peak = pending[written_from..]
                .iter()
                .fold(0.0f32, |a, s| a.max(s.abs()));
            let prev = f32::from_bits(controls.level.load(Ordering::Relaxed));
            controls
                .level
                .store(peak.max(prev * 0.8).to_bits(), Ordering::Relaxed);
        }

        while pending.len() >= FRAME {
            let frame: Vec<i16> = pending
                .drain(..FRAME)
                .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                .collect();
            if !controls.muted.load(Ordering::Relaxed) {
                // Полный канал означает, что сеть не успевает: кадр дешевле потерять.
                let _ = frames_tx.try_send(frame);
            }
        }
    };

    let stream = match cfg.sample_format() {
        SampleFormat::F32 => device.build_input_stream(
            stream_cfg.clone(),
            move |data: &[f32], _| handle(data),
            err_fn,
            None,
        )?,
        SampleFormat::I16 => {
            let mut scratch: Vec<f32> = Vec::new();
            device.build_input_stream(
                stream_cfg.clone(),
                move |data: &[i16], _| {
                    scratch.clear();
                    scratch.extend(data.iter().map(|s| *s as f32 / i16::MAX as f32));
                    handle(&scratch);
                },
                err_fn,
                None,
            )?
        }
        other => return Err(anyhow!("формат микрофона {other:?} пока не поддержан")),
    };

    Ok(stream)
}

fn build_output(
    device: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    mixer: Arc<Mixer>,
    reference: Reference,
) -> Result<Stream> {
    let channels = cfg.channels() as usize;
    let mut resampler = Resampler::new(SAMPLE_RATE, cfg.sample_rate());
    let mut staging: Vec<f32> = Vec::with_capacity(FRAME * 4);
    let mut source = vec![0.0f32; FRAME];
    let err_fn = |e| eprintln!("ошибка выходного потока: {e}");
    let stream_cfg: cpal::StreamConfig = cfg.config();

    let mut handle = move |out: &mut [f32]| {
        let needed = out.len() / channels;
        while staging.len() < needed {
            mixer.pull(&mut source);
            // Ровно то, что сейчас прозвучит, — опорный сигнал для эхоподавителя.
            {
                let mut r = reference.lock().unwrap();
                r.extend(source.iter().copied());
                while r.len() > MAX_REFERENCE {
                    r.pop_front();
                }
            }
            resampler.process(&source, &mut staging);
        }
        for (i, chunk) in out.chunks_mut(channels).enumerate() {
            let s = staging[i];
            chunk.fill(s);
        }
        staging.drain(..needed);
    };

    let stream = match cfg.sample_format() {
        SampleFormat::F32 => device.build_output_stream(
            stream_cfg.clone(),
            move |data: &mut [f32], _| handle(data),
            err_fn,
            None,
        )?,
        SampleFormat::I16 => {
            let mut scratch: Vec<f32> = Vec::new();
            device.build_output_stream(
                stream_cfg.clone(),
                move |data: &mut [i16], _| {
                    scratch.clear();
                    scratch.resize(data.len(), 0.0);
                    handle(&mut scratch);
                    for (d, s) in data.iter_mut().zip(scratch.iter()) {
                        *d = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                },
                err_fn,
                None,
            )?
        }
        other => return Err(anyhow!("формат вывода {other:?} пока не поддержан")),
    };

    Ok(stream)
}
