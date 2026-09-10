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
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::{self, AudioEngine, Mixer, FRAME, SAMPLE_RATE};
use crate::identity::{self, Identity, Known};
pub use crate::identity::Trust;
use crate::nat;

const MAGIC: [u8; 2] = *b"VC";
// Версия 3: в рукопожатие добавлены ключи и проверка подписи. Со старыми
// сборками намеренно несовместимо — лучше не соединиться, чем разбирать
// чужой формат и выдавать кашу.
const VERSION: u8 = 3;

const T_HELLO: u8 = 0x01;
const T_WELCOME: u8 = 0x02;
const T_AUDIO: u8 = 0x03;
const T_PING: u8 = 0x04;
const T_BYE: u8 = 0x05;
/// Пустой пакет, который шлют «навстречу», чтобы NAT открыл путь для ответных.
const T_PUNCH: u8 = 0x06;
/// Состав комнаты: хост рассылает его гостям при каждом изменении.
const T_PEERS: u8 = 0x07;
/// Состояние участника: пока это только выключенный микрофон. Едет
/// отдельным пакетом, потому что молчащий человек не шлёт звук вообще,
/// и по звуковым пакетам о нём ничего не узнать.
const T_STATE: u8 = 0x08;
/// Строка текстового чата.
const T_CHAT: u8 = 0x09;
/// Ответ на присланную хостом случайную строку, подписанный своим ключом.
const T_AUTH: u8 = 0x0A;

const HOST_ID: u16 = 1;
const PEER_TIMEOUT: Duration = Duration::from_secs(10);
const PORT_RANGE: std::ops::Range<u16> = 47100..47120;

/// Сколько кадров держим, прежде чем начать проигрывать. Три кадра — это
/// 60 мс: хватает, чтобы переставить местами пришедшие не по порядку пакеты,
/// и ещё не слышно как задержка.
const JITTER_FRAMES: usize = 3;

/// Приёмная сторона одного собеседника: буфер, декодер и порядковый счёт.
///
/// UDP не обещает ни порядка, ни доставки. Без этого буфера переставленные
/// пакеты слышны как щелчки, а потерянные — как дырки. Здесь первые
/// раскладываются по местам, а на вторые Opus достраивает правдоподобный
/// кусок сам.
struct Incoming {
    dec: opus::Decoder,
    pending: std::collections::BTreeMap<u16, Vec<u8>>,
    next: Option<u16>,
}

impl Incoming {
    fn new() -> Result<Self> {
        Ok(Self {
            dec: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)?,
            pending: std::collections::BTreeMap::new(),
            next: None,
        })
    }

    /// Кладёт пакет и отдаёт всё, что уже можно проиграть.
    fn push(&mut self, seq: u16, payload: &[u8], pcm: &mut [i16], out: &mut Vec<f32>) {
        if let Some(next) = self.next {
            let ahead = seq.wrapping_sub(next) as i16;
            // Опоздавший или повторный пакет: его место уже проиграно.
            // Это же условие делает бесплатным отбрасывание дубликатов —
            // а они появляются, как только один и тот же кадр приходит
            // и напрямую, и пересланным через хоста.
            if ahead < 0 {
                return;
            }
            // Ушли слишком далеко вперёд — проще начать заново.
            if ahead > 0x3000 {
                self.pending.clear();
                self.next = None;
            }
        }
        self.pending.insert(seq, payload.to_vec());

        // Слишком большой разрыв — проще начать заново, чем достраивать.
        if self.pending.len() > JITTER_FRAMES * 6 {
            self.pending.clear();
            self.next = None;
            return;
        }
        if self.pending.len() <= JITTER_FRAMES {
            return;
        }

        while self.pending.len() > JITTER_FRAMES {
            let Some((&seq, _)) = self.pending.iter().next() else {
                break;
            };
            let want = self.next.unwrap_or(seq);

            // Пропущенные кадры достраиваем: Opus умеет восстанавливать
            // потерю по предыдущему кадру, и это гораздо лучше тишины.
            let gap = seq.wrapping_sub(want);
            for _ in 0..gap.min(3) {
                if let Ok(n) = self.dec.decode(&[], pcm, false) {
                    out.extend(pcm[..n].iter().map(|s| *s as f32 / i16::MAX as f32));
                }
            }

            let data = self.pending.remove(&seq).unwrap();
            if let Ok(n) = self.dec.decode(&data, pcm, false) {
                out.extend(pcm[..n].iter().map(|s| *s as f32 / i16::MAX as f32));
            }
            self.next = Some(seq.wrapping_add(1));
        }
    }
}

/// Строка состава комнаты: номер, имя, ключ и адрес, по которому до
/// человека можно дозвониться напрямую.
type RosterEntry = (u16, String, [u8; 32], Option<SocketAddr>);

/// Куда мы умеем слать напрямую, минуя хоста.
pub type Direct = Arc<Mutex<HashMap<u16, SocketAddr>>>;

/// Громкость каждого собеседника, 0..2. Крутится из интерфейса.
pub type Volumes = Arc<Mutex<HashMap<u16, f32>>>;

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
    /// У кого выключен микрофон.
    pub muted_peers: HashMap<u16, Instant>,
    /// Текстовый чат: кто и что сказал.
    pub chat: Vec<(u16, String)>,
    /// Отпечаток ключа каждого участника.
    pub fingerprints: HashMap<u16, String>,
    /// Кого мы встречали раньше и совпал ли ключ.
    pub trust: HashMap<u16, Trust>,
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
    public: [u8; 32],
    /// Случайная строка, которую мы отправили и ждём подписанной обратно.
    challenge: [u8; 16],
    /// Подпись сошлась: это точно владелец ключа, а не тот, кто его скопировал.
    verified: bool,
    joined: Instant,
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

    fn snapshot(&self) -> Vec<RosterEntry> {
        self.peers
            .iter()
            .map(|p| (p.id, p.name.clone(), p.public, Some(p.addr)))
            .collect()
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
    identity: Arc<Identity>,
    /// Ключ, который хост обязан предъявить. Берётся из кода приглашения.
    expect_host: Option<[u8; 32]>,
    /// Свой внешний адрес — хост объявляет его в составе комнаты.
    host_addr: Option<SocketAddr>,
}

pub struct Engine {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    _audio: AudioEngine,
    punch: PunchList,
    shared: Arc<Mutex<Shared>>,
    pub controls: audio::Controls,
    pub volumes: Volumes,
    /// До кого дозваниваемся напрямую, минуя хоста.
    pub direct: Direct,
    chat_tx: SyncSender<String>,
}

impl Engine {
    /// Ставит строку в очередь на отправку. Уходит она из потока, который
    /// владеет сокетом, — иначе пришлось бы тащить сокет в интерфейс.
    pub fn send_chat(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let text: String = text.chars().take(400).collect();
        let me = self.shared.lock().unwrap().my_id;
        self.shared.lock().unwrap().chat.push((me, text.clone()));
        let _ = self.chat_tx.try_send(text);
    }
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
    pub fn prepare_host(
        nickname: String,
        shared: Arc<Mutex<Shared>>,
        identity: Arc<Identity>,
    ) -> Result<Prepared> {
        // Ниже пригодится первый кандидат: его хост объявляет своим адресом
        // в составе комнаты, чтобы гости знали, куда стучаться напрямую.
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
        let invite = encode_invite(&candidates, &identity.public);
        {
            let mut s = shared.lock().unwrap();
            s.is_host = true;
            s.connected = true;
            s.invite = Some(invite);
            s.status = "комната создана".into();
            s.my_id = HOST_ID;
            s.peers = vec![(HOST_ID, nickname.clone())];
            s.fingerprints.insert(HOST_ID, identity.fingerprint());
        }

        Ok(Prepared {
            socket,
            candidates: Vec::new(),
            my_id: HOST_ID,
            nickname,
            identity,
            expect_host: None,
            host_addr: candidates.first().copied(),
        })
    }

    pub fn prepare_join(
        code: &str,
        nickname: String,
        shared: Arc<Mutex<Shared>>,
        identity: Arc<Identity>,
    ) -> Result<Prepared> {
        let candidates = decode_invite(code)?;
        let expect_host = invite_key(code);
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
        shared.lock().unwrap().invite = Some(encode_invite(&mine, &identity.public));

        Ok(Prepared {
            socket,
            candidates,
            my_id: 0,
            nickname,
            identity,
            expect_host,
            host_addr: None,
        })
    }

    /// Поднимает звук и сетевые потоки. Вызывается из потока интерфейса.
    pub fn start(
        prepared: Prepared,
        shared: Arc<Mutex<Shared>>,
        devices: audio::DevicePrefs,
    ) -> Result<Self> {
        let Prepared {
            socket,
            candidates,
            my_id,
            nickname,
            identity,
            expect_host,
            host_addr,
        } = prepared;

        socket.set_read_timeout(Some(Duration::from_millis(200)))?;

        let is_host = candidates.is_empty();
        let table = Arc::new(Mutex::new(PeerTable {
            peers: Vec::new(),
            next_id: HOST_ID + 1,
        }));
        let locked: Locked = Arc::new(Mutex::new(None));
        let volumes: Volumes = Arc::new(Mutex::new(HashMap::new()));
        // Кого мы знаем по адресу и до кого уже достучались напрямую.
        let punch: PunchList = Arc::new(Mutex::new(Vec::new()));
        let mesh: Direct = Arc::new(Mutex::new(HashMap::new()));
        let direct: Direct = Arc::new(Mutex::new(HashMap::new()));
        let (chat_tx, chat_rx) = sync_channel::<String>(32);

        let stop = Arc::new(AtomicBool::new(false));
        let controls = audio::Controls::new();
        let mixer = Arc::new(Mixer::new());

        let (frames_tx, frames_rx) = sync_channel::<Vec<i16>>(8);

        let audio = audio::start(frames_tx, mixer.clone(), controls.clone(), devices)?;
        {
            let mut s = shared.lock().unwrap();
            s.input_name = audio.input_name.clone();
            s.output_name = audio.output_name.clone();
            s.log(format!("вход: {}", audio.input_name));
            s.log(format!("выход: {}", audio.output_name));
        }

        let my_id = Arc::new(Mutex::new(my_id));
        let mut threads = Vec::new();
        threads.push(audio::spawn_ptt_watcher(controls.clone(), stop.clone()));

        threads.push(spawn_rx(
            socket.try_clone()?,
            shared.clone(),
            table.clone(),
            mixer.clone(),
            stop.clone(),
            my_id.clone(),
            nickname.clone(),
            locked.clone(),
            volumes.clone(),
            identity.clone(),
            expect_host,
            mesh.clone(),
            direct.clone(),
            punch.clone(),
            host_addr,
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
            mesh.clone(),
            direct.clone(),
            is_host,
        ));

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
            controls.clone(),
            my_id,
            chat_rx,
            identity,
            host_addr,
            is_host,
        ));

        Ok(Engine {
            stop,
            threads,
            _audio: audio,
            punch,
            shared,
            controls,
            volumes,
            direct,
            chat_tx,
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

fn encode_invite(candidates: &[SocketAddr], public: &[u8; 32]) -> String {
    let text = format!(
        "{}|{}",
        candidates
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(","),
        identity::hex(public)
    );
    URL_SAFE_NO_PAD.encode(text)
}

/// Ключ хоста из кода приглашения. По нему гость убеждается, что попал
/// туда, куда его звали, а не к тому, кто перехватил адрес.
pub fn invite_key(code: &str) -> Option<[u8; 32]> {
    let raw = URL_SAFE_NO_PAD.decode(code.trim()).ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (_, key) = text.split_once('|')?;
    identity::parse_public(key).ok()
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
    // Ключ хоста живёт в том же коде после разделителя.
    let text = text.split('|').next().unwrap_or(&text).to_string();

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
fn encode_addr(a: Option<SocketAddr>) -> [u8; 6] {
    let mut out = [0u8; 6];
    if let Some(SocketAddr::V4(v4)) = a {
        out[..4].copy_from_slice(&v4.ip().octets());
        out[4..].copy_from_slice(&v4.port().to_be_bytes());
    }
    out
}

fn decode_addr(b: &[u8]) -> Option<SocketAddr> {
    let port = u16::from_be_bytes([b[4], b[5]]);
    if port == 0 {
        return None;
    }
    Some(SocketAddr::new(
        IpAddr::V4(std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3])),
        port,
    ))
}

fn encode_peers(roster: &[RosterEntry]) -> Vec<u8> {
    let mut msg = header(T_PEERS);
    msg.push(roster.len().min(255) as u8);
    for (id, name, pk, addr) in roster.iter().take(255) {
        let bytes = name.as_bytes();
        let len = bytes.len().min(64);
        msg.extend_from_slice(&id.to_be_bytes());
        msg.extend_from_slice(pk);
        msg.extend_from_slice(&encode_addr(*addr));
        msg.push(len as u8);
        msg.extend_from_slice(&bytes[..len]);
    }
    msg
}

fn decode_peers(body: &[u8]) -> Vec<RosterEntry> {
    let mut out = Vec::new();
    if body.is_empty() {
        return out;
    }
    let count = body[0] as usize;
    let mut i = 1usize;
    for _ in 0..count {
        if i + 41 > body.len() {
            break;
        }
        let id = u16::from_be_bytes([body[i], body[i + 1]]);
        let mut pk = [0u8; 32];
        pk.copy_from_slice(&body[i + 2..i + 34]);
        let addr = decode_addr(&body[i + 34..i + 40]);
        let len = body[i + 40] as usize;
        i += 41;
        if i + len > body.len() {
            break;
        }
        out.push((
            id,
            String::from_utf8_lossy(&body[i..i + len]).to_string(),
            pk,
            addr,
        ));
        i += len;
    }
    out
}

/// Хост рассылает всем гостям, кто сейчас в комнате.
fn broadcast_peers(
    socket: &UdpSocket,
    table: &Arc<Mutex<PeerTable>>,
    host_name: &str,
    host_key: &[u8; 32],
    host_addr: Option<SocketAddr>,
) -> Vec<RosterEntry> {
    let t = table.lock().unwrap();
    let roster: Vec<RosterEntry> =
        std::iter::once((HOST_ID, host_name.to_string(), *host_key, host_addr))
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
    volumes: Volumes,
    identity: Arc<Identity>,
    expect_host: Option<[u8; 32]>,
    mesh: Direct,
    direct: Direct,
    punch: PunchList,
    host_addr: Option<SocketAddr>,
    is_host: bool,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut known = Known::load();
        let mut streams: HashMap<u16, Incoming> = HashMap::new();
        let mut pcm = vec![0i16; FRAME * 2];
        let mut decoded: Vec<f32> = Vec::with_capacity(FRAME * 4);
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
                    if body.len() < 33 {
                        continue;
                    }
                    let mut public = [0u8; 32];
                    public.copy_from_slice(&body[..32]);
                    let name = String::from_utf8_lossy(&body[32..])
                        .chars()
                        .take(24)
                        .collect::<String>();

                    let mut t = table.lock().unwrap();
                    let (id, challenge) = match t.peers.iter_mut().find(|p| p.addr == from) {
                        Some(p) => {
                            p.last_seen = Instant::now();
                            (p.id, p.challenge)
                        }
                        None => {
                            let id = t.next_id;
                            t.next_id += 1;
                            // Случайная строка, которую гость должен подписать.
                            // Без неё открытый ключ можно было бы просто скопировать.
                            let challenge = identity::random_bytes::<16>();
                            t.peers.push(Peer {
                                id,
                                name: name.clone(),
                                addr: from,
                                last_seen: Instant::now(),
                                public,
                                challenge,
                                verified: false,
                                joined: Instant::now(),
                            });
                            shared
                                .lock()
                                .unwrap()
                                .log(format!("подключается {name} ({from})"));
                            (id, challenge)
                        }
                    };
                    drop(t);

                    let mut msg = header(T_WELCOME);
                    msg.extend_from_slice(&id.to_be_bytes());
                    msg.extend_from_slice(&challenge);
                    msg.extend_from_slice(&identity.public);
                    let _ = socket.send_to(&msg, from);
                }

                T_AUTH if is_host => {
                    if body.len() < 66 {
                        continue;
                    }
                    let id = u16::from_be_bytes([body[0], body[1]]);
                    let mut sig = [0u8; 64];
                    sig.copy_from_slice(&body[2..66]);

                    let mut t = table.lock().unwrap();
                    let Some(p) = t.peers.iter_mut().find(|p| p.id == id && p.addr == from) else {
                        continue;
                    };
                    if p.verified {
                        continue;
                    }
                    if !identity::verify(&p.public, &p.challenge, &sig) {
                        let name = p.name.clone();
                        drop(t);
                        shared
                            .lock()
                            .unwrap()
                            .log(format!("подпись {name} не сошлась — не пускаем"));
                        continue;
                    }
                    p.verified = true;
                    let (name, fp) = (p.name.clone(), identity::fingerprint(&p.public));
                    drop(t);

                    let trust = known.check(&name, &fp);
                    known.remember(&name, &fp);
                    {
                        let mut sh = shared.lock().unwrap();
                        sh.fingerprints.insert(id, fp.clone());
                        sh.trust.insert(id, trust);
                        sh.log(match trust {
                            Trust::Changed => format!("ВНИМАНИЕ: у {name} другой ключ ({fp})"),
                            Trust::New => format!("{name} подключился, ключ {fp} (впервые)"),
                            Trust::Known => format!("{name} подключился, ключ {fp}"),
                        });
                    }

                    let roster = broadcast_peers(&socket, &table, &nickname, &identity.public, host_addr);
                    let mut sh = shared.lock().unwrap();
                    sh.peers = roster.iter().map(|(i, n, _, _)| (*i, n.clone())).collect();
                }

                T_PEERS if !is_host => {
                    let roster = decode_peers(body);
                    if roster.is_empty() {
                        continue;
                    }
                    // Хвосты ушедших не должны продолжать звучать.
                    let ids: Vec<u16> = roster.iter().map(|(id, _, _, _)| *id).collect();
                    mixer.retain(&ids);

                    // Адреса всех участников: с этого начинается прямая связь.
                    // Хост знакомит нас друг с другом, дальше мы стучимся
                    // навстречу сами — никакого ручного обмена кодами.
                    {
                        let me = shared.lock().unwrap().my_id;
                        let mut m = mesh.lock().unwrap();
                        let mut p = punch.lock().unwrap();
                        let now = Instant::now();
                        for (id, _, _, addr) in &roster {
                            if *id == me {
                                continue;
                            }
                            let Some(a) = addr else { continue };
                            m.insert(*id, *a);
                            p.retain(|(x, _)| x != a);
                            p.push((*a, now));
                        }
                    }

                    let mut sh = shared.lock().unwrap();
                    sh.peers = roster.iter().map(|(i, n, _, _)| (*i, n.clone())).collect();
                    for (id, name, pk, _) in &roster {
                        let fp = identity::fingerprint(pk);
                        let trust = known.check(name, &fp);
                        if !sh.fingerprints.contains_key(id) {
                            if trust == Trust::Changed {
                                sh.log(format!("ВНИМАНИЕ: у {name} другой ключ ({fp})"));
                            }
                            known.remember(name, &fp);
                        }
                        sh.fingerprints.insert(*id, fp);
                        sh.trust.insert(*id, trust);
                    }
                }

                T_WELCOME if !is_host => {
                    if body.len() >= 50 {
                        let id = u16::from_be_bytes([body[0], body[1]]);
                        let challenge = &body[2..18];
                        let mut host_pk = [0u8; 32];
                        host_pk.copy_from_slice(&body[18..50]);

                        // Ключ из кода приглашения обязан совпасть: иначе это
                        // не тот, к кому нас звали.
                        if let Some(expect) = expect_host {
                            if expect != host_pk {
                                shared.lock().unwrap().log(
                                    "ключ хоста не совпал с кодом приглашения — не подключаемся",
                                );
                                continue;
                            }
                        }

                        let mut reply = header(T_AUTH);
                        reply.extend_from_slice(&id.to_be_bytes());
                        reply.extend_from_slice(&identity.sign(challenge));
                        let _ = socket.send_to(&reply, from);

                        *my_id.lock().unwrap() = id;
                        // Запоминаем именно тот адрес, откуда пришёл ответ:
                        // остальные кандидаты больше не нужны.
                        let mut lock = locked.lock().unwrap();
                        let first = lock.is_none();
                        *lock = Some(from);
                        drop(lock);

                        // Хост — тоже прямой путь, причём уже проверенный:
                        // именно с этого адреса он нам и ответил.
                        mesh.lock().unwrap().insert(HOST_ID, from);
                        direct.lock().unwrap().insert(HOST_ID, from);

                        if first {
                            let mut s = shared.lock().unwrap();
                            s.my_id = id;
                            s.connected = true;
                            s.status = "в комнате".into();
                            s.log(format!("хост ответил с {from}, наш номер {id}"));
                            s.log(format!("ключ хоста {}", identity::fingerprint(&host_pk)));
                        }
                    }
                }

                T_AUDIO => {
                    if body.len() < 5 {
                        continue;
                    }
                    let src = u16::from_be_bytes([body[0], body[1]]);
                    let seq = u16::from_be_bytes([body[2], body[3]]);
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

                    // Пакет пришёл прямо с адреса собеседника, а не от хоста —
                    // значит путь пробит и дальше можно слать ему напрямую.
                    if mesh.lock().unwrap().get(&src) == Some(&from) {
                        let mut d = direct.lock().unwrap();
                        if d.insert(src, from).is_none() {
                            shared
                                .lock()
                                .unwrap()
                                .log(format!("прямой путь до #{src} ({from})"));
                        }
                    }

                    // Признак речи от собеседника: по нему интерфейс
                    // подсвечивает, кто сейчас говорит.
                    if voiced {
                        shared.lock().unwrap().voice_seen.insert(src, Instant::now());
                    }

                    let stream = match streams.entry(src) {
                        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::hash_map::Entry::Vacant(e) => match Incoming::new() {
                            Ok(v) => e.insert(v),
                            Err(_) => continue,
                        },
                    };

                    decoded.clear();
                    stream.push(seq, &body[5..], &mut pcm, &mut decoded);
                    if !decoded.is_empty() {
                        let gain = volumes
                            .lock()
                            .unwrap()
                            .get(&src)
                            .copied()
                            .unwrap_or(1.0);
                        if (gain - 1.0).abs() > 0.01 {
                            for s in decoded.iter_mut() {
                                *s *= gain;
                            }
                        }
                        mixer.push(src, &decoded);
                    }
                }

                T_CHAT => {
                    if body.len() < 3 {
                        continue;
                    }
                    let src = u16::from_be_bytes([body[0], body[1]]);
                    let text = String::from_utf8_lossy(&body[2..])
                        .chars()
                        .take(400)
                        .collect::<String>();

                    if is_host {
                        let t = table.lock().unwrap();
                        for addr in t.addrs_except(Some(from)) {
                            let _ = socket.send_to(&buf[..n], addr);
                        }
                    }

                    let mut sh = shared.lock().unwrap();
                    sh.chat.push((src, text));
                    if sh.chat.len() > 200 {
                        sh.chat.remove(0);
                    }
                }

                T_STATE => {
                    if body.len() < 3 {
                        continue;
                    }
                    let src = u16::from_be_bytes([body[0], body[1]]);
                    let muted = body[2] & 1 != 0;

                    // Хост пересылает состояние остальным.
                    if is_host {
                        let t = table.lock().unwrap();
                        for addr in t.addrs_except(Some(from)) {
                            let _ = socket.send_to(&buf[..n], addr);
                        }
                    }

                    let mut sh = shared.lock().unwrap();
                    if muted {
                        sh.muted_peers.insert(src, Instant::now());
                    } else {
                        sh.muted_peers.remove(&src);
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
                        let roster = broadcast_peers(&socket, &table, &nickname, &identity.public, host_addr);
                        shared.lock().unwrap().peers =
                            roster.iter().map(|(i, n, _, _)| (*i, n.clone())).collect();
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
    mesh: Direct,
    direct: Direct,
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
            let voiced = audio::level_value(&controls.voice) > 0.55 && !controls.mic_off();

            let mut msg = header(T_AUDIO);
            msg.extend_from_slice(&id.to_be_bytes());
            msg.extend_from_slice(&seq.to_be_bytes());
            msg.push(if voiced { 1 } else { 0 });
            msg.extend_from_slice(&out[..len]);
            seq = seq.wrapping_add(1);

            match target {
                Some(host) => {
                    // Всем, до кого пробит прямой путь, шлём сами: это короче
                    // и не грузит канал хоста.
                    let known = mesh.lock().unwrap().len();
                    let reachable: Vec<SocketAddr> =
                        direct.lock().unwrap().values().copied().collect();
                    for addr in &reachable {
                        let _ = socket.send_to(&msg, *addr);
                    }
                    // Хосту — только пока кто-то остаётся недостижимым напрямую.
                    // Дубликаты, если и случатся, отсеет джиттер-буфер по номеру.
                    if reachable.len() < known || known == 0 {
                        let _ = socket.send_to(&msg, host);
                    }
                }
                // Хост шлёт всем напрямую: у него адреса всех есть по построению.
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
    controls: audio::Controls,
    my_id: Arc<Mutex<u16>>,
    chat_rx: Receiver<String>,
    identity: Arc<Identity>,
    host_addr: Option<SocketAddr>,
    is_host: bool,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut tick = 0u32;
        let mut hinted = false;

        while !stop.load(Ordering::Relaxed) {
            // Шаг короткий, чтобы отправка сообщения не ждала полсекунды;
            // всё периодическое делается раз в пять шагов.
            thread::sleep(Duration::from_millis(100));

            while let Ok(text) = chat_rx.try_recv() {
                let mut msg = header(T_CHAT);
                msg.extend_from_slice(&my_id.lock().unwrap().to_be_bytes());
                msg.extend_from_slice(text.as_bytes());
                if is_host {
                    let addrs = table.lock().unwrap().addrs_except(None);
                    for addr in addrs {
                        let _ = socket.send_to(&msg, addr);
                    }
                } else if let Some(host) = *locked.lock().unwrap() {
                    let _ = socket.send_to(&msg, host);
                }
            }

            if tick % 5 != 0 {
                tick += 1;
                continue;
            }
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

            // Своё состояние рассылаем всем: выключенный микрофон вообще не
            // шлёт звук, поэтому узнать о нём по звуковым пакетам нельзя.
            {
                let mut msg = header(T_STATE);
                msg.extend_from_slice(&my_id.lock().unwrap().to_be_bytes());
                msg.push(if controls.muted.load(Ordering::Relaxed) { 1 } else { 0 });
                if is_host {
                    let addrs = table.lock().unwrap().addrs_except(None);
                    for addr in addrs {
                        let _ = socket.send_to(&msg, addr);
                    }
                } else if let Some(host) = *locked.lock().unwrap() {
                    let _ = socket.send_to(&msg, host);
                }
            }

            if is_host {
                // Хост выкидывает тех, кто замолчал.
                let mut t = table.lock().unwrap();
                let before = t.peers.len();
                // Не подтвердивший подпись за десять секунд не входит.
                t.peers.retain(|p| {
                    p.last_seen.elapsed() < PEER_TIMEOUT
                        && (p.verified || p.joined.elapsed() < Duration::from_secs(10))
                });
                let changed = t.peers.len() != before;
                drop(t);

                // Состав рассылаем сразу при изменении и раз в две секунды:
                // UDP теряет пакеты, а список участников разъезжаться не должен.
                if changed || tick % 4 == 0 {
                    if changed {
                        shared.lock().unwrap().log("кто-то отвалился по таймауту");
                    }
                    let roster = broadcast_peers(&socket, &table, &nickname, &identity.public, host_addr);
                    let ids: Vec<u16> = roster.iter().map(|(id, _, _, _)| *id).collect();
                    mixer.retain(&ids);
                    shared.lock().unwrap().peers =
                        roster.iter().map(|(i, n, _, _)| (*i, n.clone())).collect();
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
                    msg.extend_from_slice(&identity.public);
                    msg.extend_from_slice(nickname.as_bytes());
                    for addr in &candidates {
                        let _ = socket.send_to(&msg, *addr);
                    }

                    if tick >= 100 && !hinted {
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
