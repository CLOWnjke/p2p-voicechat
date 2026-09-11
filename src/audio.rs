//! Захват микрофона и воспроизведение.
//!
//! Внутри всё живёт на 48 кГц моно — это родной режим Opus. Звуковая карта
//! может работать на другой частоте (на Windows сплошь и рядом 44 100), поэтому
//! на входе и выходе стоит простой линейный ресемплер.

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use decibri_aec::{Aec, AecConfig};
use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::Array2;
use nnnoiseless::DenoiseState;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const SAMPLE_RATE: u32 = 48_000;
/// 20 мс — стандартный размер кадра для голоса в Opus.
pub const FRAME: usize = 960;
/// 10 мс — кадр и у DeepFilterNet, и у RNNoise. Ровно половина нашего опусного,
/// так что всё делится без остатка.
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
/// первые миллисекунды уже ушли наружу закрытыми. Цена — 40 мс задержки,
/// и они добавляются только при включённых воротах.
const GATE_LOOKAHEAD: usize = 4;
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
    /// `prob` и `peak` берутся с сигнала **до** шумодава: после него тишина
    /// становится идеальным нулём, и по нему невозможно понять, что речь
    /// вот-вот начнётся.
    fn push(
        &mut self,
        frame: &[f32],
        prob: f32,
        peak: f32,
        open_thr: f32,
        floor: f32,
        out: &mut Vec<f32>,
    ) {
        let mut buf = [0f32; DENOISE_FRAME];
        buf.copy_from_slice(frame);

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

/// Счётчики потраченного времени, по одному на этап.
///
/// Считаем не «сколько занял последний кадр», а сумму наносекунд с начала
/// работы. Интерфейс берёт разность за прошедшее время — и это сразу доля
/// ядра, без всяких усреднений и догадок. Мерить нужно потому, что
/// рассуждения о стоимости этапов уже один раз разошлись с тем, что человек
/// видит в игре, а измерение не спорит.
#[derive(Default)]
pub struct Load {
    /// Обработка микрофона: эхоподавитель, шумодав, ворота.
    pub dsp_ns: AtomicU64,
    /// Упаковка в Opus и отправка.
    pub enc_ns: AtomicU64,
    /// Разбор пришедших пакетов, декодирование, микширование.
    pub rx_ns: AtomicU64,
    /// Отрисовка окна.
    pub ui_ns: AtomicU64,
    /// Сколько кадров окна нарисовано.
    pub ui_frames: AtomicU64,
    /// Сколько звуковых пакетов отправлено и принято.
    pub sent: AtomicU64,
    pub recv: AtomicU64,
}

impl Load {
    pub fn add(counter: &AtomicU64, since: std::time::Instant) {
        counter.fetch_add(since.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
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
    /// Модель шумоподавления загружается ~полсекунды при входе в комнату.
    pub dfn_ready: Arc<AtomicBool>,
    /// Загрузка закончилась — неважно, удачей или нет. Без этого метка в
    /// окне навсегда оставалась «ЗАГРУЗКА…», хотя ничего уже не грузилось
    /// и работал запасной RNNoise.
    pub dfn_done: Arc<AtomicBool>,
    /// Уровень уже обработанного сигнала — так видно, что шумодав делает.
    pub level: Level,
    /// Оценка «сейчас говорят», которую RNNoise выдаёт заодно с очисткой.
    pub voice: Level,
    /// Режим «говорить по кнопке».
    pub ptt: Arc<AtomicBool>,
    /// Кнопка сейчас нажата. Ставится глобальным опросом клавиатуры —
    /// иначе в игре режим бесполезен, там окно не в фокусе.
    pub ptt_down: Arc<AtomicBool>,
    /// Какую клавишу слушать.
    pub ptt_key: Arc<Mutex<String>>,
    /// Куда уходит время. Нужно, чтобы разговор о нагрузке вёлся числами.
    pub load: Arc<Load>,
}

impl Controls {
    /// Микрофон молчит: либо выключен руками, либо кнопка разговора отпущена.
    pub fn mic_off(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
            || (self.ptt.load(Ordering::Relaxed) && !self.ptt_down.load(Ordering::Relaxed))
    }

    pub fn new() -> Self {
        Self {
            muted: Arc::new(AtomicBool::new(false)),
            denoise: Arc::new(AtomicBool::new(true)),
            aec: Arc::new(AtomicBool::new(true)),
            gate: Arc::new(AtomicBool::new(true)),
            gate_sensitivity: Arc::new(AtomicU32::new(0.5f32.to_bits())),
            gate_floor: Arc::new(AtomicU32::new(0.02f32.to_bits())),
            dfn_ready: Arc::new(AtomicBool::new(false)),
            dfn_done: Arc::new(AtomicBool::new(false)),
            level: Arc::new(AtomicU32::new(0)),
            voice: Arc::new(AtomicU32::new(0)),
            ptt: Arc::new(AtomicBool::new(false)),
            ptt_down: Arc::new(AtomicBool::new(false)),
            ptt_key: Arc::new(Mutex::new("F8".to_string())),
            load: Arc::new(Load::default()),
        }
    }
}

impl Default for Controls {
    fn default() -> Self {
        Self::new()
    }
}

/// Глобальный опрос клавиши разговора.
///
/// Именно опрос, а не системный хоткей: нужны и нажатие, и отпускание,
/// причём когда окно не в фокусе. Тридцать раз в секунду — это ничто.
///
/// `DeviceState::new` умеет падать там, где до клавиатуры не дотянуться
/// (нет графической сессии, не выданы права). Ловим это и просто выключаем
/// режим, а не роняем приложение.
pub fn spawn_ptt_watcher(controls: Controls, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    use device_query::{DeviceQuery, DeviceState};

    thread::spawn(move || {
        let device = match std::panic::catch_unwind(DeviceState::new) {
            Ok(d) => d,
            Err(_) => {
                eprintln!("клавиатура недоступна — режим «говорить по кнопке» выключен");
                controls.ptt.store(false, Ordering::Relaxed);
                return;
            }
        };

        while !stop.load(Ordering::Relaxed) {
            if controls.ptt.load(Ordering::Relaxed) {
                let want = controls.ptt_key.lock().unwrap().clone();
                let down = device
                    .get_keys()
                    .iter()
                    .any(|k| format!("{k:?}").eq_ignore_ascii_case(&want));
                controls.ptt_down.store(down, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(30));
        }
    })
}

/// Какие устройства выбрал человек. Пусто — берём системные по умолчанию.
#[derive(Clone, Default)]
pub struct DevicePrefs {
    pub input: Option<String>,
    pub output: Option<String>,
}

/// Списки доступных устройств для выпадающего выбора.
pub fn list_devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let ins = host
        .input_devices()
        .map(|it| it.map(|d| device_name(&d, "микрофон")).collect())
        .unwrap_or_default();
    let outs = host
        .output_devices()
        .map(|it| it.map(|d| device_name(&d, "динамики")).collect())
        .unwrap_or_default();
    (ins, outs)
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
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    pub input_name: String,
    pub output_name: String,
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Запускает захват и воспроизведение.
///
/// Готовые кадры по 960 сэмплов уходят в `frames_tx`; всё, что нужно проиграть,
/// кладётся в `mixer`.
pub fn start(
    frames_tx: SyncSender<Vec<i16>>,
    mixer: Arc<Mixer>,
    controls: Controls,
    prefs: DevicePrefs,
) -> Result<AudioEngine> {
    let host = cpal::default_host();

    // Выбранное по имени, иначе системное по умолчанию. Если названное
    // устройство исчезло (выдернули гарнитуру), молча берём умолчание —
    // это лучше, чем отказаться запускаться.
    let in_dev = prefs
        .input
        .as_ref()
        .and_then(|want| {
            host.input_devices().ok().and_then(|mut it| {
                it.find(|d| device_name(d, "") == *want)
            })
        })
        .or_else(|| host.default_input_device())
        .ok_or_else(|| anyhow!("не найден микрофон"))?;

    let out_dev = prefs
        .output
        .as_ref()
        .and_then(|want| {
            host.output_devices().ok().and_then(|mut it| {
                it.find(|d| device_name(d, "") == *want)
            })
        })
        .or_else(|| host.default_output_device())
        .ok_or_else(|| anyhow!("не найдено устройство вывода"))?;

    let input_name = device_name(&in_dev, "микрофон");
    let output_name = device_name(&out_dev, "динамики");

    let in_cfg = in_dev.default_input_config()?;
    let out_cfg = out_dev.default_output_config()?;

    let reference: Reference = Arc::new(Mutex::new(VecDeque::new()));
    let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(16);
    let stop = Arc::new(AtomicBool::new(false));

    let input = build_input(&in_dev, &in_cfg, pcm_tx)?;
    let output = build_output(&out_dev, &out_cfg, mixer, reference.clone())?;

    // Обработка живёт в своём потоке: в аудиоколбэке жёсткий дедлайн, и
    // нейросеть там держать нельзя. Заодно это единственный способ вообще
    // использовать DeepFilterNet — его состояние не переезжает между потоками,
    // поэтому создаётся прямо внутри рабочего потока.
    let worker = spawn_processing(
        in_cfg.sample_rate(),
        pcm_rx,
        frames_tx,
        controls,
        reference,
        stop.clone(),
    );

    input.play()?;
    output.play()?;

    Ok(AudioEngine {
        _input: input,
        _output: output,
        stop,
        worker: Some(worker),
        input_name,
        output_name,
    })
}

fn build_input(
    device: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    pcm_tx: SyncSender<Vec<f32>>,
) -> Result<Stream> {
    let channels = cfg.channels() as usize;
    let err_fn = |e| eprintln!("ошибка входного потока: {e}");
    let stream_cfg: cpal::StreamConfig = cfg.config();

    // В колбэке делаем самый минимум: сводим в моно и отдаём дальше. Здесь
    // жёсткий дедлайн, и любая просадка слышна сразу как треск.
    let handle = move |samples: &[f32]| {
        let mut mono = Vec::with_capacity(samples.len() / channels.max(1) + 1);
        for chunk in samples.chunks(channels) {
            mono.push(chunk.iter().sum::<f32>() / channels as f32);
        }
        // Полная очередь означает, что обработка не успевает: блок дешевле
        // потерять, чем задержать весь поток.
        let _ = pcm_tx.try_send(mono);
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

/// Весь тракт обработки: ресемплинг, эхоподавление, шумоподавление, ворота
/// и нарезка на кадры Opus.
fn spawn_processing(
    device_rate: u32,
    pcm_rx: Receiver<Vec<f32>>,
    frames_tx: SyncSender<Vec<i16>>,
    controls: Controls,
    reference: Reference,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut resampler = Resampler::new(device_rate, SAMPLE_RATE);
        // Свежие сэмплы, уже на 48 кГц, до эхоподавления.
        let mut resampled: Vec<f32> = Vec::with_capacity(2048);
        let mut echo_free: Vec<f32> = Vec::with_capacity(2048);
        let mut ref_chunk: Vec<f32> = Vec::with_capacity(2048);
        // Сырой поток на 48 кГц, ещё не прошедший через шумодав.
        let mut raw: Vec<f32> = Vec::with_capacity(DENOISE_FRAME * 4);
        // Готовое к упаковке в Opus.
        let mut pending: Vec<f32> = Vec::with_capacity(FRAME * 4);

        let mut aec = {
            let mut config = AecConfig::default();
            config.sample_rate = SAMPLE_RATE;
            match Aec::new(config) {
                Ok(a) => Some(a),
                Err(e) => {
                    eprintln!("эхоподавитель не завёлся: {e}");
                    None
                }
            }
        };

        // Загрузка модели занимает около полусекунды. Она идёт здесь, в фоне,
        // чтобы окно не подвисало при входе в комнату.
        let mut dfn_params = RuntimeParams::default();
        // По умолчанию при SNR ниже -10 дБ модель не приглушает, а обнуляет
        // выход полностью. Оценка SNR отстаёт, поэтому под обнуление попадают
        // и первые кадры речи — начало фразы пропадает. Опускаем порог, чтобы
        // модель всегда хотя бы фильтровала, а не выключалась.
        dfn_params.min_db_thresh = -20.0;

        let mut dfn = match DfTract::new(DfParams::default(), &dfn_params) {
            Ok(d) if d.hop_size == DENOISE_FRAME => {
                controls.dfn_ready.store(true, Ordering::Relaxed);
                Some(d)
            }
            Ok(d) => {
                eprintln!(
                    "DeepFilterNet ждёт кадр {}, а тракт устроен на {} — работаем на RNNoise",
                    d.hop_size, DENOISE_FRAME
                );
                None
            }
            Err(e) => {
                eprintln!("DeepFilterNet не загрузился ({e}) — работаем на RNNoise");
                None
            }
        };
        controls.dfn_done.store(true, Ordering::Relaxed);

        let mut dfn_in = Array2::<f32>::zeros((1, DENOISE_FRAME));
        let mut dfn_out = Array2::<f32>::zeros((1, DENOISE_FRAME));

        // RNNoise нужен ради оценки речи, на которой держатся ворота. Если
        // DeepFilterNet не загрузился, он же становится и шумодавом.
        let mut rnn = DenoiseState::new();
        let mut vad_in = [0f32; DENOISE_FRAME];
        let mut vad_out = [0f32; DENOISE_FRAME];

        let mut gate = VoiceGate::new();
        let mut norm = [0f32; DENOISE_FRAME];

        while !stop.load(Ordering::Relaxed) {
            let mono = match pcm_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(m) => m,
                Err(_) => continue,
            };

            // Сначала отдаём эхоподавителю всё, что успело уйти в динамики.
            ref_chunk.clear();
            {
                let mut r = reference.lock().unwrap();
                ref_chunk.extend(r.drain(..));
            }

            resampled.clear();
            resampler.process(&mono, &mut resampled);

            // Порядок важен: сначала убираем эхо, потом шум. Эхоподавителю
            // нужен микрофон в том виде, в каком эхо в него пришло.
            echo_free.clear();
            let cancelled = match aec.as_mut() {
                Some(a) => {
                    if !ref_chunk.is_empty() {
                        a.feed_reference(&ref_chunk);
                    }
                    a.process(&resampled, &mut echo_free).is_ok()
                }
                None => false,
            };
            if !cancelled {
                echo_free.clear();
                echo_free.extend_from_slice(&resampled);
            }

            if controls.aec.load(Ordering::Relaxed) {
                raw.extend_from_slice(&echo_free);
            } else {
                raw.extend_from_slice(&resampled);
            }

            let dsp_started = std::time::Instant::now();
            while raw.len() >= DENOISE_FRAME {
                let dirty = dfn_in.as_slice_mut().unwrap();
                for (dst, src) in dirty.iter_mut().zip(raw.drain(..DENOISE_FRAME)) {
                    *dst = src;
                }

                // Детектор речи работает по сигналу ДО шумодава. Это важно:
                // после шумодава тишина — идеальный ноль, по которому нельзя
                // понять, что речь начинается, и ворота открываются с
                // опозданием на пол-секунды.
                // RNNoise ждёт шкалу i16, а не диапазон от -1 до 1 — на этом
                // обычно и спотыкаются при интеграции.
                let mut dirty_peak = 0.0f32;
                for (dst, src) in vad_in.iter_mut().zip(dfn_in.as_slice().unwrap()) {
                    dirty_peak = dirty_peak.max(src.abs());
                    *dst = src * i16::MAX as f32;
                }
                let voice = rnn.process_frame(&mut vad_out, &vad_in);
                controls.voice.store(voice.to_bits(), Ordering::Relaxed);

                // DeepFilterNet работает в привычном диапазоне от -1 до 1.
                let dfn_ok = match dfn.as_mut() {
                    Some(d) => d.process(dfn_in.view(), dfn_out.view_mut()).is_ok(),
                    None => false,
                };

                if controls.denoise.load(Ordering::Relaxed) {
                    if dfn_ok {
                        norm.copy_from_slice(dfn_out.as_slice().unwrap());
                    } else {
                        // Запасной вариант, если модель не загрузилась.
                        for (dst, src) in norm.iter_mut().zip(vad_out.iter()) {
                            *dst = src / i16::MAX as f32;
                        }
                    }
                } else {
                    norm.copy_from_slice(dfn_in.as_slice().unwrap());
                }

                let written_from = pending.len();
                if controls.gate.load(Ordering::Relaxed) {
                    let sens = f32::from_bits(controls.gate_sensitivity.load(Ordering::Relaxed));
                    // Чувствительность 0 требует почти уверенной речи, 1 — почти ничего.
                    let open_thr = 0.85 - 0.70 * sens.clamp(0.0, 1.0);
                    let floor = f32::from_bits(controls.gate_floor.load(Ordering::Relaxed));
                    gate.push(&norm, voice, dirty_peak, open_thr, floor, &mut pending);
                } else {
                    pending.extend_from_slice(&norm);
                }

                // Уровень снимается с того, что реально уходит наружу: так на
                // полоске видно и работу шумодава, и работу ворот.
                // Считаем RMS, а не пик: пик скачет от каждого щелчка, и
                // полоска превращается в стробоскоп.
                let out = &pending[written_from..];
                let rms = if out.is_empty() {
                    0.0
                } else {
                    (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt()
                };
                let prev = f32::from_bits(controls.level.load(Ordering::Relaxed));
                controls
                    .level
                    .store(rms.max(prev * 0.93).to_bits(), Ordering::Relaxed);
            }

            Load::add(&controls.load.dsp_ns, dsp_started);

            while pending.len() >= FRAME {
                let frame: Vec<i16> = pending
                    .drain(..FRAME)
                    .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                    .collect();
                if !controls.mic_off() {
                    let _ = frames_tx.try_send(frame);
                }
            }
        }
    })
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
