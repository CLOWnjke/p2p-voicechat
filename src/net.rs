//! Сеть: формат пакетов, режим хоста с пересылкой и режим клиента.
//!
//! Хост — это маленький SFU: он не смешивает звук, а просто пересылает чужие
//! пакеты остальным. Пересылка стоит почти ничего, зато пробивать NAT надо
//! только до хоста, а не между всеми парами.
//!
//! Код приглашения несёт не один адрес, а список кандидатов: внешний, адрес в
//! локальной сети и петлевой. Клиент стучится во все сразу и остаётся на том,
//! откуда пришёл ответ. Так один и тот же код работает и через интернет, и в
//! одной квартире по Wi-Fi, и между двумя копиями на одном компьютере —
//! домашние роутеры почти никогда не заворачивают пакет на собственный внешний
//! адрес обратно внутрь.

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::{self, AudioEngine, Mixer, FRAME, SAMPLE_RATE};
use crate::nat;

const MAGIC: [u8; 2] = *b"VC";
// Версия 2: в звуковой пакет добавлен байт флагов с признаком речи.
// Со сборками версии 1 намеренно несовместимо — лучше не соединиться,
// чем разбирать чужой формат и выдавать кашу.
const VERSION: u8 = 2;

const T_HELLO: u8 = 0x01;
const T_WELCOME: u8 = 0x02;
const T_AUDIO: u8 = 0x03;
const T_PING: u8 = 0x04;
const T_BYE: u8 = 0x05;
/// Пустой пакет, который шлют «навстречу», чтобы NAT открыл путь для ответных.
const T_PUNCH: u8 = 0x06;
/// Состав комнаты: хост рассылает его гостям при каждом изменении.
const T_PEERS: u8 = 0x07;

const HOST_ID: u16 = 1;
const PEER_TIMEOUT: Duration = Duration::from_secs(10);
const PORT_RANGE: std::ops::Range<u16> = 47100..47120;

/// Куда клиент реально дозвонился. Пока None — ещё стучимся.
type Locked = Arc<Mutex<Option<SocketAddr>>>;

/// Адреса, в которые мы шлём пустые пакеты навстречу собеседнику.
///
/// Домашние роутеры обычно пропускают входящий пакет только с того адреса,
/// куда мы сами уже что-то отправляли. Поэтому если обе стороны начнут слать
/// друг другу одновременно, путь открывается в обе стороны — и дальше обычные
/// HELLO долетают. Это и есть пробивание NAT, ради которого стороны меняются
/// кодами в обе стороны, а не только гость получает код хоста.
type PunchList = Arc<Mutex<Vec<(SocketAddr, Instant)>>>;

/// Сколько времени продолжаем стучаться в добавленный адрес.
const PUNCH_FOR: Duration = Duration::from_secs(180);

/// Всё, что видит интерфейс. Ничего тяжёлого сюда не кладём: блокировка берётся
/// и из потока отрисовки, и из сетевых потоков.
#[derive(Default)]
pub struct Shared {
    pub status: String,
    pub invite: Option<String>,
    pub upnp_note: Option<String>,
    pub peers: Vec<(u16, String)>,
    pub log: Vec<String>,
    pub connected: bool,
    pub is_host: bool,
    pub input_name: String,
    pub output_name: String,
    /// Наш номер в комнате. Хост знает его сразу, гость — из WELCOME.
    pub my_id: u16,
    /// Когда от кого в последний раз приходил признак речи.
    pub voice_seen: HashMap<u16, Instant>,
}

impl Shared {
    pub fn log(&mut self, line: impl Into<String>) {
        let line = line.into();
        eprintln!("{line}");
        self.log.push(line);
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }
}

struct Peer {
    id: u16,
    name: String,
    addr: SocketAddr,
    last_seen: Instant,
}

#[derive(Default)]
struct PeerTable {
    peers: Vec<Peer>,
    next_id: u16,
}

impl PeerTable {
    fn addrs_except(&self, except: Option<SocketAddr>) -> Vec<SocketAddr> {
        self.peers
            .iter()
            .filter(|p| Some(p.addr) != except)
            .map(|p| p.addr)
            .collect()
    }

    fn snapshot(&self) -> Vec<(u16, String)> {
        self.peers.iter().map(|p| (p.id, p.name.clone())).collect()
    }
}

/// Результат подготовки соединения. Всё здесь можно передавать между потоками,
/// поэтому медленные шаги (поиск роутера, опрос STUN) уходят в фон, а звуковые
/// потоки создаются уже в потоке интерфейса: cpal не любит переезды между потоками.
pub struct Prepared {
    socket: UdpSocket,
    /// Пусто — значит мы хост.
    candidates: Vec<SocketAddr>,
    my_id: u16,
    nickname: String,
}

pub struct Engine {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    _audio: AudioEngine,
    punch: PunchList,
    shared: Arc<Mutex<Shared>>,
    pub controls: audio::Controls,
}

impl Engine {
    /// Добавляет адреса собеседника, в которые надо стучаться навстречу.
    /// Нужно, когда NAT не пропускает входящие «просто так».
    pub fn add_punch_targets(&self, code: &str) -> Result<usize> {
        let addrs = decode_invite(code)?;
        let now = Instant::now();
        let mut list = self.punch.lock().unwrap();
        for addr in &addrs {
            list.retain(|(a, _)| a != addr);
            list.push((*addr, now));
        }
        drop(list);

        self.shared.lock().unwrap().log(format!(
            "стучимся навстречу: {}",
            addrs
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        Ok(addrs.len())
    }
}

impl Engine {
    pub fn prepare_host(nickname: String, shared: Arc<Mutex<Shared>>) -> Result<Prepared> {
        let (socket, port) = bind_in_range()?;
        shared
            .lock()
            .unwrap()
            .log(format!("сокет открыт на порту {port}"));

        // UPnP — попытка, а не требование. Не вышло, значит спросим STUN и
        // будем надеяться на дружелюбный NAT.
        match nat::try_upnp(port) {
            Ok(addr) => {
                let mut s = shared.lock().unwrap();
                s.upnp_note = Some(format!("роутер пробросил порт: {addr}"));
                s.log(format!("UPnP сработал: {addr}"));
            }
            Err(e) => {
                let mut s = shared.lock().unwrap();
                s.upnp_note = Some("роутер не пробросил порт автоматически".into());
                s.log(format!("UPnP не сработал: {e}"));
            }
        }

        let candidates = my_candidates(&socket, port, &shared)?;
        let invite = encode_invite(&candidates);
        {
            let mut s = shared.lock().unwrap();
            s.is_host = true;
            s.connected = true;
            s.invite = Some(invite);
            s.status = "комната создана".into();
            s.my_id = HOST_ID;
            s.peers = vec![(HOST_ID, nickname.clone())];
        }

        Ok(Prepared {
            socket,
            candidates: Vec::new(),
            my_id: HOST_ID,
            nickname,
        })
    }

    pub fn prepare_join(
        code: &str,
        nickname: String,
        shared: Arc<Mutex<Shared>>,
    ) -> Result<Prepared> {
        let candidates = decode_invite(code)?;
        let (socket, port) = bind_in_range()?;

        {
            let mut s = shared.lock().unwrap();
            s.is_host = false;
            s.status = "подключаемся…".into();
            s.log(format!("наш порт {port}"));
            s.log(format!(
                "пробуем адреса хоста: {}",
                candidates
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        // Свои адреса нужны не только для порядка: если NAT хоста не пропускает
        // входящие, он попросит наш код и начнёт стучаться навстречу.
        let mine = my_candidates(&socket, port, &shared)?;
        shared.lock().unwrap().invite = Some(encode_invite(&mine));

        Ok(Prepared {
            socket,
            candidates,
            my_id: 0,
            nickname,
        })
    }

    /// Поднимает звук и сетевые потоки. Вызывается из потока интерфейса.
    pub fn start(prepared: Prepared, shared: Arc<Mutex<Shared>>) -> Result<Self> {
        let Prepared {
            socket,
            candidates,
            my_id,
            nickname,
        } = prepared;

        socket.set_read_timeout(Some(Duration::from_millis(200)))?;

        let is_host = candidates.is_empty();
        let table = Arc::new(Mutex::new(PeerTable {
            peers: Vec::new(),
            next_id: HOST_ID + 1,
        }));
        let locked: Locked = Arc::new(Mutex::new(None));

        let stop = Arc::new(AtomicBool::new(false));
        let controls = audio::Controls::new();
        let mixer = Arc::new(Mixer::new());

        let (frames_tx, frames_rx) = sync_channel::<Vec<i16>>(8);

        let audio = audio::start(frames_tx, mixer.clone(), controls.clone())?;
        {
            let mut s = shared.lock().unwrap();
            s.input_name = audio.input_name.clone();
            s.output_name = audio.output_name.clone();
            s.log(format!("вход: {}", audio.input_name));
            s.log(format!("выход: {}", audio.output_name));
        }

        let my_id = Arc::new(Mutex::new(my_id));
        let mut threads = Vec::new();

        threads.push(spawn_rx(
            socket.try_clone()?,
            shared.clone(),
            table.clone(),
            mixer.clone(),
            stop.clone(),
            my_id.clone(),
            nickname.clone(),
            locked.clone(),
            is_host,
        ));

        threads.push(spawn_tx(
            socket.try_clone()?,
            table.clone(),
            frames_rx,
            stop.clone(),
            my_id.clone(),
            locked.clone(),
            controls.clone(),
            is_host,
        ));

        let punch: PunchList = Arc::new(Mutex::new(Vec::new()));

        threads.push(spawn_keepalive(
            socket,
            shared.clone(),
            table,
            stop.clone(),
            nickname,
            candidates,
            locked,
            punch.clone(),
            mixer,
            is_host,
        ));

        Ok(Engine {
            stop,
            threads,
            _audio: audio,
            punch,
            shared,
            controls,
        })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn bind_in_range() -> Result<(UdpSocket, u16)> {
    for port in PORT_RANGE {
        if let Ok(sock) = UdpSocket::bind(("0.0.0.0", port)) {
            return Ok((sock, port));
        }
    }
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    let port = sock.local_addr()?.port();
    Ok((sock, port))
}

/// Собирает адреса, по которым до нас можно достучаться: внешний по STUN,
/// адрес в локальной сети и петлевой.
fn my_candidates(
    socket: &UdpSocket,
    port: u16,
    shared: &Arc<Mutex<Shared>>,
) -> Result<Vec<SocketAddr>> {
    let mut candidates: Vec<SocketAddr> = Vec::new();

    match nat::discover_public_addr(socket) {
        Ok(addr) => {
            shared
                .lock()
                .unwrap()
                .log(format!("наш внешний адрес по STUN: {addr}"));
            candidates.push(addr);
        }
        Err(e) => {
            shared.lock().unwrap().log(format!("STUN не ответил: {e}"));
        }
    }

    if let Some(ip) = nat::local_ipv4() {
        candidates.push(SocketAddr::new(IpAddr::V4(ip), port));
    }
    candidates.push(SocketAddr::new(
        IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        port,
    ));

    candidates.dedup();
    if candidates.is_empty() {
        return Err(anyhow!("не удалось определить ни одного адреса"));
    }

    shared.lock().unwrap().log(format!(
        "наши адреса: {}",
        candidates
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    Ok(candidates)
}

fn encode_invite(candidates: &[SocketAddr]) -> String {
    let text = candidates
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(",");
    URL_SAFE_NO_PAD.encode(text)
}

pub fn decode_invite(code: &str) -> Result<Vec<SocketAddr>> {
    let code = code.trim();

    // Удобно для проверки на одной машине: адрес можно вписать как есть,
    // например 127.0.0.1:47100. Через запятую — тоже.
    let direct: Vec<SocketAddr> = code
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    if !direct.is_empty() {
        return Ok(direct);
    }

    let raw = URL_SAFE_NO_PAD
        .decode(code)
        .map_err(|_| anyhow!("код приглашения испорчен"))?;
    let text = String::from_utf8(raw).map_err(|_| anyhow!("код приглашения испорчен"))?;

    let list: Vec<SocketAddr> = text
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    if list.is_empty() {
        return Err(anyhow!("в коде приглашения нет адресов"));
    }
    Ok(list)
}

fn header(kind: u8) -> Vec<u8> {
    vec![MAGIC[0], MAGIC[1], VERSION, kind]
}

/// Состав комнаты в пакете: [кол-во] и дальше [id u16][длина имени][имя].
fn encode_peers(roster: &[(u16, String)]) -> Vec<u8> {
    let mut msg = header(T_PEERS);
    msg.push(roster.len().min(255) as u8);
    for (id, name) in roster.iter().take(255) {
        let bytes = name.as_bytes();
        let len = bytes.len().min(64);
        msg.extend_from_slice(&id.to_be_bytes());
        msg.push(len as u8);
        msg.extend_from_slice(&bytes[..len]);
    }
    msg
}

fn decode_peers(body: &[u8]) -> Vec<(u16, String)> {
    let mut out = Vec::new();
    if body.is_empty() {
        return out;
    }
    let count = body[0] as usize;
    let mut i = 1usize;
    for _ in 0..count {
        if i + 3 > body.len() {
            break;
        }
        let id = u16::from_be_bytes([body[i], body[i + 1]]);
        let len = body[i + 2] as usize;
        i += 3;
        if i + len > body.len() {
            break;
        }
        out.push((id, String::from_utf8_lossy(&body[i..i + len]).to_string()));
        i += len;
    }
    out
}

/// Хост рассылает всем гостям, кто сейчас в комнате.
fn broadcast_peers(socket: &UdpSocket, table: &Arc<Mutex<PeerTable>>, host_name: &str) -> Vec<(u16, String)> {
    let t = table.lock().unwrap();
    let roster: Vec<(u16, String)> = std::iter::once((HOST_ID, host_name.to_string()))
        .chain(t.snapshot())
        .collect();
    let addrs = t.addrs_except(None);
    drop(t);

    let msg = encode_peers(&roster);
    for addr in addrs {
        let _ = socket.send_to(&msg, addr);
    }
    roster
}

#[allow(clippy::too_many_arguments)]
fn spawn_rx(
    socket: UdpSocket,
    shared: Arc<Mutex<Shared>>,
    table: Arc<Mutex<PeerTable>>,
    mixer: Arc<Mixer>,
    stop: Arc<AtomicBool>,
    my_id: Arc<Mutex<u16>>,
    nickname: String,
    locked: Locked,
    is_host: bool,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut decoders: HashMap<u16, opus::Decoder> = HashMap::new();
        let mut pcm = vec![0i16; FRAME];
        let mut buf = [0u8; 2048];

        while !stop.load(Ordering::Relaxed) {
            let (n, from) = match socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(_) => continue, // таймаут — обычное дело, просто крутимся дальше
            };
            if n < 4 || buf[0..2] != MAGIC || buf[2] != VERSION {
                continue;
            }
            let kind = buf[3];
            let body = &buf[4..n];

            match kind {
                T_HELLO if is_host => {
                    let name = String::from_utf8_lossy(body)
                        .chars()
                        .take(24)
                        .collect::<String>();
                    let mut t = table.lock().unwrap();
                    let id = match t.peers.iter_mut().find(|p| p.addr == from) {
                        Some(p) => {
                            p.last_seen = Instant::now();
                            p.id
                        }
                        None => {
                            let id = t.next_id;
                            t.next_id += 1;
                            t.peers.push(Peer {
                                id,
                                name: name.clone(),
                                addr: from,
                                last_seen: Instant::now(),
                            });
                            shared
                                .lock()
                                .unwrap()
                                .log(format!("подключился {name} ({from})"));
                            id
                        }
                    };
                    drop(t);

                    let mut msg = header(T_WELCOME);
                    msg.extend_from_slice(&id.to_be_bytes());
                    let _ = socket.send_to(&msg, from);

                    // Все узнают, кто теперь в комнате.
                    let roster = broadcast_peers(&socket, &table, &nickname);
                    shared.lock().unwrap().peers = roster;
                }

                T_PEERS if !is_host => {
                    let roster = decode_peers(body);
                    if roster.is_empty() {
                        continue;
                    }
                    // Хвосты ушедших не должны продолжать звучать.
                    let ids: Vec<u16> = roster.iter().map(|(id, _)| *id).collect();
                    mixer.retain(&ids);
                    shared.lock().unwrap().peers = roster;
                }

                T_WELCOME if !is_host => {
                    if body.len() >= 2 {
                        let id = u16::from_be_bytes([body[0], body[1]]);
                        *my_id.lock().unwrap() = id;
                        // Запоминаем именно тот адрес, откуда пришёл ответ:
                        // остальные кандидаты больше не нужны.
                        let mut lock = locked.lock().unwrap();
                        let first = lock.is_none();
                        *lock = Some(from);
                        drop(lock);

                        if first {
                            let mut s = shared.lock().unwrap();
                            s.my_id = id;
                            s.connected = true;
                            s.status = "в комнате".into();
                            s.log(format!("хост ответил с {from}, наш номер {id}"));
                        }
                    }
                }

                T_AUDIO => {
                    if body.len() < 5 {
                        continue;
                    }
                    let src = u16::from_be_bytes([body[0], body[1]]);
                    let voiced = body[4] & 1 != 0;

                    // Хост пересылает пакет всем остальным как есть.
                    if is_host {
                        let mut t = table.lock().unwrap();
                        if let Some(p) = t.peers.iter_mut().find(|p| p.addr == from) {
                            p.last_seen = Instant::now();
                        }
                        for addr in t.addrs_except(Some(from)) {
                            let _ = socket.send_to(&buf[..n], addr);
                        }
                    }

                    if src == *my_id.lock().unwrap() {
                        continue; // собственный голос слушать не надо
                    }

                    // Признак речи от собеседника: по нему интерфейс
                    // подсвечивает, кто сейчас говорит.
                    if voiced {
                        shared.lock().unwrap().voice_seen.insert(src, Instant::now());
                    }

                    let dec = decoders.entry(src).or_insert_with(|| {
                        opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)
                            .expect("не удалось создать декодер Opus")
                    });
                    if let Ok(len) = dec.decode(&body[5..], &mut pcm, false) {
                        let samples: Vec<f32> = pcm[..len]
                            .iter()
                            .map(|s| *s as f32 / i16::MAX as f32)
                            .collect();
                        mixer.push(src, &samples);
                    }
                }

                T_PING if is_host => {
                    let mut t = table.lock().unwrap();
                    if let Some(p) = t.peers.iter_mut().find(|p| p.addr == from) {
                        p.last_seen = Instant::now();
                    }
                }

                T_BYE if is_host => {
                    let mut t = table.lock().unwrap();
                    if let Some(pos) = t.peers.iter().position(|p| p.addr == from) {
                        let gone = t.peers.remove(pos);
                        drop(t);
                        mixer.remove(gone.id);
                        shared
                            .lock()
                            .unwrap()
                            .log(format!("{} отключился", gone.name));
                        let roster = broadcast_peers(&socket, &table, &nickname);
                        shared.lock().unwrap().peers = roster;
                    }
                }

                _ => {}
            }
        }
    })
}

fn spawn_tx(
    socket: UdpSocket,
    table: Arc<Mutex<PeerTable>>,
    frames_rx: Receiver<Vec<i16>>,
    stop: Arc<AtomicBool>,
    my_id: Arc<Mutex<u16>>,
    locked: Locked,
    controls: audio::Controls,
    is_host: bool,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut encoder =
            match opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("не удалось создать кодер Opus: {e}");
                    return;
                }
            };
        let _ = encoder.set_bitrate(opus::Bitrate::Bits(32_000));
        // Встроенная защита от потерь: часть предыдущего кадра едет в следующем.
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(10);

        let mut seq: u16 = 0;
        let mut out = vec![0u8; 1024];

        while !stop.load(Ordering::Relaxed) {
            let frame = match frames_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(f) => f,
                Err(_) => continue,
            };

            // До того как дозвонились, кодировать нечего и некуда.
            let target = if is_host { None } else { *locked.lock().unwrap() };
            if !is_host && target.is_none() {
                continue;
            }

            let len = match encoder.encode(&frame, &mut out) {
                Ok(l) => l,
                Err(_) => continue,
            };

            let id = *my_id.lock().unwrap();
            let voiced = audio::level_value(&controls.voice) > 0.55
                && !controls.muted.load(Ordering::Relaxed);

            let mut msg = header(T_AUDIO);
            msg.extend_from_slice(&id.to_be_bytes());
            msg.extend_from_slice(&seq.to_be_bytes());
            msg.push(if voiced { 1 } else { 0 });
            msg.extend_from_slice(&out[..len]);
            seq = seq.wrapping_add(1);

            match target {
                // Клиент шлёт только хосту, тот разошлёт остальным.
                Some(host) => {
                    let _ = socket.send_to(&msg, host);
                }
                // Хост шлёт всем напрямую.
                None => {
                    let addrs = table.lock().unwrap().addrs_except(None);
                    for addr in addrs {
                        let _ = socket.send_to(&msg, addr);
                    }
                }
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_keepalive(
    socket: UdpSocket,
    shared: Arc<Mutex<Shared>>,
    table: Arc<Mutex<PeerTable>>,
    stop: Arc<AtomicBool>,
    nickname: String,
    candidates: Vec<SocketAddr>,
    locked: Locked,
    punch: PunchList,
    mixer: Arc<Mixer>,
    is_host: bool,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut tick = 0u32;
        let mut hinted = false;

        while !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(500));
            tick += 1;

            // Стучимся навстречу по адресам, которые нам дали вручную.
            // Пакет ничего не значит: он нужен только чтобы наш роутер
            // запомнил этот адрес как «мы туда уже писали».
            {
                let mut list = punch.lock().unwrap();
                list.retain(|(_, added)| added.elapsed() < PUNCH_FOR);
                for (addr, _) in list.iter() {
                    let _ = socket.send_to(&header(T_PUNCH), *addr);
                }
            }

            if is_host {
                // Хост выкидывает тех, кто замолчал.
                let mut t = table.lock().unwrap();
                let before = t.peers.len();
                t.peers.retain(|p| p.last_seen.elapsed() < PEER_TIMEOUT);
                let changed = t.peers.len() != before;
                drop(t);

                // Состав рассылаем сразу при изменении и раз в две секунды:
                // UDP теряет пакеты, а список участников разъезжаться не должен.
                if changed || tick % 4 == 0 {
                    if changed {
                        shared.lock().unwrap().log("кто-то отвалился по таймауту");
                    }
                    let roster = broadcast_peers(&socket, &table, &nickname);
                    let ids: Vec<u16> = roster.iter().map(|(id, _)| *id).collect();
                    mixer.retain(&ids);
                    shared.lock().unwrap().peers = roster;
                }
                continue;
            }

            match *locked.lock().unwrap() {
                Some(host) => {
                    let _ = socket.send_to(&header(T_PING), host);
                }
                None => {
                    // Стучимся во все адреса сразу: какой-то из них ответит.
                    // Повторяем, потому что первые пакеты часто уходят в никуда,
                    // пока NAT не откроет путь.
                    let mut msg = header(T_HELLO);
                    msg.extend_from_slice(nickname.as_bytes());
                    for addr in &candidates {
                        let _ = socket.send_to(&msg, *addr);
                    }

                    if tick == 20 && !hinted {
                        hinted = true;
                        let mut s = shared.lock().unwrap();
                        s.status = "хост не отвечает".into();
                        s.log(
                            "за 10 секунд ответа нет ни по одному адресу. Если вы оба в интернете, \
                             NAT у хоста не пропускает входящие: включите UPnP в роутере, \
                             пробросьте UDP-порт вручную или поднимите Radmin VPN.",
                        );
                    }
                }
            }
        }

        if let Some(host) = *locked.lock().unwrap() {
            let _ = socket.send_to(&header(T_BYE), host);
        }
    })
}
