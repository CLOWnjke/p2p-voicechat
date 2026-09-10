//! Захват микрофона и воспроизведение.
//!
//! Внутри всё живёт на 48 кГц моно — это родной режим Opus. Звуковая карта
//! может работать на другой частоте (на Windows сплошь и рядом 44 100), поэтому
//! на входе и выходе стоит простой линейный ресемплер.

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
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

    let input = build_input(&in_dev, &in_cfg, frames_tx, controls)?;
    let output = build_output(&out_dev, &out_cfg, mixer)?;

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
) -> Result<Stream> {
    let channels = cfg.channels() as usize;
    let mut resampler = Resampler::new(cfg.sample_rate(), SAMPLE_RATE);
    let mut mono: Vec<f32> = Vec::with_capacity(2048);
    // Сырой поток на 48 кГц, ещё не прошедший через шумодав.
    let mut raw: Vec<f32> = Vec::with_capacity(DENOISE_FRAME * 4);
    // Готовое к упаковке в Opus.
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME * 4);

    let mut denoiser = DenoiseState::new();
    let mut den_in = [0f32; DENOISE_FRAME];
    let mut den_out = [0f32; DENOISE_FRAME];

    let err_fn = |e| eprintln!("ошибка входного потока: {e}");
    let stream_cfg: cpal::StreamConfig = cfg.config();

    let mut handle = move |samples: &[f32]| {
        // Сводим в моно: для голоса разница между каналами не нужна.
        mono.clear();
        for chunk in samples.chunks(channels) {
            mono.push(chunk.iter().sum::<f32>() / channels as f32);
        }
        resampler.process(&mono, &mut raw);

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

            let mut peak = 0.0f32;
            for s in source {
                let v = s / i16::MAX as f32;
                peak = peak.max(v.abs());
                pending.push(v);
            }
            // Пиковый уровень с плавным спадом — иначе полоска дёргается.
            // Уровень снимается уже после очистки, чтобы работа шумодава
            // была видна глазами.
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
