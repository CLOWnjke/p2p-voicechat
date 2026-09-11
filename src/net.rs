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
use std::fs;
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
// Версия 5: опознание стало взаимным. Раньше подпись предъявлял только
// гость, а хост — нет, и любой, кто видел код приглашения, мог хостом
// притвориться: код полупубличный, его пересылают в переписке. Теперь
// обе стороны подписывают задачу друг друга.
//
// Со старыми сборками намеренно несовместимо — лучше не соединиться, чем
// разбирать чужой формат и выдавать кашу.
const VERSION: u8 = 5;

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
/// «Хост теперь я»: рассылает тот, кого выбрали после ухода прежнего.
const T_HOST: u8 = 0x0B;

const HOST_ID: u16 = 1;
/// Сколько человек пускаем в комнату. Ограничение не от жадности:
/// без него поток HELLO со случайными ключами набивал таблицу без предела.
const MAX_PEERS: usize = 16;
/// Сколько собеседников держим в памяти на приёме. Раньше на каждый номер
/// из тела звукового пакета заводился свой декодер и своя дорожка микшера,
/// и посторонний мог развести их шестьдесят пять тысяч.
const MAX_STREAMS: usize = 32;
/// Через сколько молчания считаем, что человека больше нет.
const PEER_TIMEOUT: Duration = Duration::from_secs(8);
/// Через сколько молчания хоста считаем связь потерянной и начинаем
/// заново стучаться. Сеть у людей меняется: отвалился VPN, переключился
/// Wi-Fi — и внешний адрес стал другим.
const HOST_SILENCE: Duration = Duration::from_secs(5);
/// Через сколько молчания хоста считаем, что он ушёл совсем, и выбираем
/// нового. Заметно больше HOST_SILENCE: сначала надо дать шанс простому
/// переподключению, смена хоста — крайняя мера.
const HOST_GONE: Duration = Duration::from_secs(12);
/// Насколько свежим должен быть след человека, чтобы считать его живым
/// при выборе нового хоста.
const ALIVE_FOR: Duration = Duration::from_secs(10);
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

/// Когда от хоста последний раз приходил хоть какой-то пакет. По этому
/// гость понимает, что связь оборвалась, и начинает искать хоста заново.
type HostSeen = Arc<Mutex<Instant>>;

/// Кто сейчас хост и как его найти.
///
/// Раньше это решалось один раз при запуске: создал комнату — хост, вошёл по
/// коду — гость. Но хост — обычный человек, он может закрыть приложение
/// первым, и комната не должна умирать вместе с ним. Поэтому «кто хост» —
/// состояние, которое живёт всю встречу и может смениться.
///
/// Выбор нового делается без переговоров: у всех на руках один и тот же
/// состав, и каждый берёт из него живого участника с наименьшим номером.
/// Раз правило одинаковое, все приходят к одному ответу сами.
struct Room {
    is_host: AtomicBool,
    /// Номер того, кто сейчас хост.
    host_id: Mutex<u16>,
    /// Ключ, который хост обязан предъявить. Сначала берётся из кода
    /// приглашения, после смены — из состава комнаты.
    expect_host: Mutex<Option<[u8; 32]>>,
    /// Адреса, в которые стучимся, пока не нашли хоста.
    targets: Mutex<Vec<SocketAddr>>,
    /// Свои адреса. Лежат наготове: если хостом станем мы, из них
    /// собирается новый код приглашения.
    mine: Vec<SocketAddr>,
    /// Свой внешний адрес — хост объявляет его в составе комнаты.
    my_addr: Mutex<Option<SocketAddr>>,
    /// Последний известный состав. Только по нему и можно выбрать нового
    /// хоста, когда прежнего уже не спросить.
    roster: Mutex<Vec<RosterEntry>>,
    /// Хост попрощался явно — ждать двенадцать секунд тишины незачем.
    host_gone: AtomicBool,
    /// Случайная задача, которую мы отправляем в HELLO и которую хост
    /// обязан подписать в ответ. Без неё хостом мог притвориться любой,
    /// кто видел код приглашения: открытый ключ в нём и лежит.
    my_challenge: Mutex<[u8; 16]>,
    /// Мы уже были в комнате. Пока нет — тишина означает, что мы просто не
    /// дозвонились, и выбирать нового хоста не из чего: состав, поднятый
    /// из памяти, это лишь список тех, к кому мы стучимся.
    joined: AtomicBool,
}

type RoomRef = Arc<Room>;

impl Room {
    fn is_host(&self) -> bool {
        self.is_host.load(Ordering::Relaxed)
    }

    fn host_id(&self) -> u16 {
        *self.host_id.lock().unwrap()
    }
}

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
    /// Когда от кого приходил хоть какой-нибудь пакет. По этому в списке
    /// видно, что человек пропал, ещё до того как его выкинет по таймауту.
    pub peer_seen: HashMap<u16, Instant>,
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
    /// Адреса, с которых постучались тем же ключом, и выданные им задачи.
    ///
    /// Переезжаем только на тот адрес, что подпишет свою задачу: иначе
    /// чужой, знающий открытый ключ, увёл бы на себя чужой звук. А задача
    /// на каждый адрес своя и не перевыдаётся — раньше одна общая задача
    /// перевыпускалась на каждый стук, и достаточно было слать стук чужим
    /// открытым ключом (он публичен), чтобы человек не мог войти никогда:
    /// его подпись всё время оказывалась под уже устаревшей задачей.
    pending: HashMap<SocketAddr, [u8; 16]>,
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
    /// Свои адреса: пригодятся, если хостом придётся стать нам.
    mine: Vec<SocketAddr>,
    /// Состав запомненной комнаты, если возвращаемся в неё.
    seed: Vec<RosterEntry>,
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
            mine: candidates.clone(),
            seed: Vec::new(),
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
            mine,
            seed: Vec::new(),
            my_id: 0,
            nickname,
            identity,
            expect_host,
            host_addr: None,
        })
    }

    /// Возвращение в запомненную комнату.
    ///
    /// Кто в ней сейчас хост — неизвестно и неважно: стучимся сразу ко всем,
    /// кого помним. Кто на месте, тот и ответит — хост рукопожатием, а
    /// любой другой покажет на хоста. Достаточно одного уцелевшего.
    pub fn prepare_return(
        nickname: String,
        shared: Arc<Mutex<Shared>>,
        identity: Arc<Identity>,
    ) -> Result<Prepared> {
        let saved = last_room();
        let mine_keys = identity.public;
        let seed: Vec<RosterEntry> = saved
            .iter()
            .enumerate()
            .map(|(i, (pk, addr, name))| (i as u16 + HOST_ID, name.clone(), *pk, Some(*addr)))
            .collect();
        let candidates: Vec<SocketAddr> = saved
            .iter()
            .filter(|(pk, _, _)| *pk != mine_keys)
            .map(|(_, addr, _)| *addr)
            .collect();
        if candidates.is_empty() {
            return Err(anyhow!("нет запомненной комнаты"));
        }
        let (socket, port) = bind_in_range()?;

        {
            let mut s = shared.lock().unwrap();
            s.is_host = false;
            s.status = "возвращаемся…".into();
            s.log(format!("наш порт {port}"));
            s.log(format!(
                "стучимся ко всем, кого помним по комнате: {}",
                saved
                    .iter()
                    .map(|(_, _, n)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        let mine = my_candidates(&socket, port, &shared)?;
        shared.lock().unwrap().invite = Some(encode_invite(&mine, &identity.public));

        Ok(Prepared {
            socket,
            candidates,
            mine,
            seed,
            my_id: 0,
            nickname,
            identity,
            // Кто именно хост — выяснится из ответа. Проверим, что он хотя бы
            // один из тех, кого мы в этой комнате видели.
            expect_host: None,
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
            mine,
            seed,
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
        let host_seen: HostSeen = Arc::new(Mutex::new(Instant::now()));
        let room: RoomRef = Arc::new(Room {
            is_host: AtomicBool::new(is_host),
            host_id: Mutex::new(HOST_ID),
            expect_host: Mutex::new(expect_host),
            targets: Mutex::new(candidates),
            mine,
            my_addr: Mutex::new(host_addr),
            roster: Mutex::new(seed),
            host_gone: AtomicBool::new(false),
            my_challenge: Mutex::new(identity::random_bytes::<16>()?),
            joined: AtomicBool::new(is_host),
        });
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
            mesh.clone(),
            direct.clone(),
            punch.clone(),
            host_seen.clone(),
            room.clone(),
            controls.load.clone(),
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
            room.clone(),
        ));

        threads.push(spawn_keepalive(
            socket,
            shared.clone(),
            table,
            stop.clone(),
            nickname,
            locked,
            punch.clone(),
            mixer,
            controls.clone(),
            my_id,
            chat_rx,
            identity,
            mesh,
            host_seen,
            room,
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
    // Проверяем длину здесь, а не надеемся на вызывающего: сегодня все
    // места безопасны, но любое новое обращение с коротким срезом уронило
    // бы приложение.
    let b = b.get(..6)?;
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
        // Режем по границе знака, а не по байту: иначе у всех в списке
        // будет имя с мусорным хвостом.
        let cut = name
            .char_indices()
            .map(|(i, c)| i + c.len_utf8())
            .take_while(|i| *i <= 64)
            .last()
            .unwrap_or(0);
        let bytes = &name.as_bytes()[..cut];
        let len = bytes.len();
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
/// Запоминает комнату на диск: имена, ключи и адреса всех, кто в ней был.
///
/// Ради этого файла всё и затевалось. Пока его не было, вернувшемуся после
/// вылета приходилось выяснять в игровом чате, у кого теперь комната, и
/// просить код заново. Теперь достаточно постучаться во всех разом: кто-то
/// из них наверняка на месте, а кто именно стал хостом — разберётся
/// приложение.
fn remember_room(roster: &[RosterEntry]) {
    if roster.len() < 2 {
        return; // комната из одного себя запоминать нечего
    }
    let Some(path) = identity::config_file("last_room") else {
        return;
    };
    let body: String = roster
        .iter()
        .filter_map(|(_, name, pk, addr)| {
            let addr = (*addr)?;
            Some(format!(
                "{} {} {}\n",
                identity::hex(pk),
                addr,
                name.replace('\n', " ")
            ))
        })
        .collect();
    if body.is_empty() {
        return;
    }
    if fs::read_to_string(&path).ok().as_deref() == Some(body.as_str()) {
        return; // ничего не поменялось — не трогаем диск
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(path, body);
}

/// Читает запомненную комнату: ключи, адреса и имена.
pub fn last_room() -> Vec<([u8; 32], SocketAddr, String)> {
    let Some(text) = identity::config_file("last_room").and_then(|p| fs::read_to_string(p).ok())
    else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut it = line.splitn(3, ' ');
            let pk = identity::parse_public(it.next()?).ok()?;
            let addr: SocketAddr = it.next()?.parse().ok()?;
            Some((pk, addr, it.next().unwrap_or("").to_string()))
        })
        .collect()
}

/// Переходит под нового хоста: перестаём быть хостом сами, если были, и
/// начинаем знакомиться с ним заново — обычным HELLO, как при входе.
/// Номер и имя при этом сохраняются: новый хост поднял состав из того же
/// списка, что и мы.
#[allow(clippy::too_many_arguments)]
fn follow_host(
    shared: &Arc<Mutex<Shared>>,
    table: &Arc<Mutex<PeerTable>>,
    room: &RoomRef,
    locked: &Locked,
    host_seen: &HostSeen,
    id: u16,
    public: [u8; 32],
    addr: SocketAddr,
    was_host: bool,
) {
    if was_host {
        table.lock().unwrap().peers.clear();
    }
    room.is_host.store(false, Ordering::Relaxed);
    room.host_gone.store(false, Ordering::Relaxed);
    *room.host_id.lock().unwrap() = id;
    *room.expect_host.lock().unwrap() = Some(public);
    *room.targets.lock().unwrap() = vec![addr];
    *locked.lock().unwrap() = None;
    if let Ok(fresh) = identity::random_bytes::<16>() {
        *room.my_challenge.lock().unwrap() = fresh;
    }
    *host_seen.lock().unwrap() = Instant::now();

    let mut s = shared.lock().unwrap();
    s.is_host = false;
    s.status = "комната у нового хоста…".into();
    let name = s
        .peers
        .iter()
        .find(|(i, _)| *i == id)
        .map(|(_, n)| n.clone())
        .unwrap_or_else(|| format!("#{id}"));
    s.log(format!("комната перешла к {name} ({addr}) — переподключаемся"));
}

/// Принимает комнату на себя. Состав известен, адреса тоже — поэтому
/// поднимаем таблицу участников такой, какой она была у прежнего хоста,
/// сохраняя всем номера. Подписи придётся собрать заново: чужому слову о
/// том, кто есть кто, мы не верим даже в наследство.
fn become_host(
    socket: &UdpSocket,
    room: &RoomRef,
    table: &Arc<Mutex<PeerTable>>,
    shared: &Arc<Mutex<Shared>>,
    locked: &Locked,
    nickname: &str,
    identity: &Arc<Identity>,
    my_id: u16,
) {
    let old = room.host_id();
    let roster = room.roster.lock().unwrap().clone();
    {
        let mut t = table.lock().unwrap();
        t.peers.clear();
        let mut max = my_id;
        for (id, name, pk, addr) in roster.iter() {
            max = max.max(*id);
            if *id == my_id || *id == old {
                continue;
            }
            let Some(a) = addr else { continue };
            let Ok(challenge) = identity::random_bytes::<16>() else {
                continue;
            };
            t.peers.push(Peer {
                id: *id,
                name: name.clone(),
                addr: *a,
                last_seen: Instant::now(),
                public: *pk,
                challenge,
                pending: HashMap::new(),
                verified: false,
                joined: Instant::now(),
            });
        }
        t.next_id = max.saturating_add(1);
    }

    room.is_host.store(true, Ordering::Relaxed);
    room.host_gone.store(false, Ordering::Relaxed);
    *room.host_id.lock().unwrap() = my_id;
    *room.expect_host.lock().unwrap() = None;
    *room.my_addr.lock().unwrap() = room.mine.first().copied();
    *locked.lock().unwrap() = None;

    {
        let mut s = shared.lock().unwrap();
        s.is_host = true;
        s.invite = Some(encode_invite(&room.mine, &identity.public));
        s.status = "хост теперь вы".into();
        s.log("прежний хост ушёл — комната перешла к вам");
    }

    // Объявляемся несколько раз: пакет легко теряется, а от этого
    // объявления зависит, соберётся комната обратно или рассыплется.
    let mut msg = header(T_HOST);
    msg.extend_from_slice(&my_id.to_be_bytes());
    msg.extend_from_slice(&identity.public);
    msg.extend_from_slice(&encode_addr(*room.my_addr.lock().unwrap()));
    for _ in 0..5 {
        let addrs = table.lock().unwrap().addrs_except(None);
        for addr in addrs {
            let _ = socket.send_to(&msg, addr);
        }
        thread::sleep(Duration::from_millis(60));
    }

    let roster = broadcast_peers(socket, table, nickname, &identity.public, room);
    shared.lock().unwrap().peers = roster.iter().map(|(i, n, _, _)| (*i, n.clone())).collect();
}

/// Кому быть новым хостом: живому участнику с наименьшим номером.
///
/// Никаких переговоров: у всех на руках один и тот же состав и одно и то же
/// правило, поэтому каждый приходит к одному ответу сам. Разойтись во
/// мнениях они всё-таки могут — если кто-то считает соседа живым, а кто-то
/// нет; на этот случай объявившийся хост с меньшим номером перебивает
/// объявившегося с большим.
fn elect(room: &RoomRef, shared: &Arc<Mutex<Shared>>, my_id: u16) -> Option<RosterEntry> {
    let gone = room.host_id();
    let seen = shared.lock().unwrap().peer_seen.clone();
    let mut alive: Vec<RosterEntry> = room
        .roster
        .lock()
        .unwrap()
        .iter()
        .filter(|(id, _, _, addr)| {
            *id != gone
                && (*id == my_id
                    || (addr.is_some()
                        && seen
                            .get(id)
                            .map(|t| t.elapsed() < ALIVE_FOR)
                            .unwrap_or(false)))
        })
        .cloned()
        .collect();
    alive.sort_by_key(|(id, _, _, _)| *id);
    alive.into_iter().next()
}

/// Что именно подписывает хост в ответе на стук.
///
/// Не голая задача, а задача вместе с назначением и номерами. Без метки
/// назначения подпись из одного места протокола можно предъявить в
/// другом: и там, и там это были просто шестнадцать байт.
fn welcome_msg(ask: &[u8], host_id: u16, guest_id: u16) -> Vec<u8> {
    let mut m = Vec::with_capacity(ask.len() + 14);
    m.extend_from_slice(b"voicechat-welcome");
    m.extend_from_slice(ask);
    m.extend_from_slice(&host_id.to_be_bytes());
    m.extend_from_slice(&guest_id.to_be_bytes());
    m
}

/// То же для ответа гостя.
fn auth_msg(challenge: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(challenge.len() + 14);
    m.extend_from_slice(b"voicechat-auth");
    m.extend_from_slice(challenge);
    m
}

/// Чей это пакет на самом деле.
///
/// Номер в теле — не доказательство: его пишет отправитель. Верим ему
/// ровно в одном случае — когда пакет пришёл от хоста: хост пересылает
/// чужую речь и перед пересылкой подставляет туда проверенный номер.
/// Во всех остальных случаях номер обязан совпасть с тем, кого мы узнали
/// по адресу.
fn speaker(sender: Option<u16>, body: &[u8], room: &RoomRef, is_host: bool) -> Option<u16> {
    if body.len() < 2 {
        return None;
    }
    let claimed = u16::from_be_bytes([body[0], body[1]]);
    let sender = sender?;
    if !is_host && sender == room.host_id() {
        // Хост говорит и за себя, и за других — но только он.
        return Some(claimed);
    }
    (sender == claimed).then_some(sender)
}

/// Копия пакета с подставленным номером отправителя.
fn with_src(packet: &[u8], src: u16) -> Vec<u8> {
    let mut out = packet.to_vec();
    if out.len() >= 6 {
        out[4..6].copy_from_slice(&src.to_be_bytes());
    }
    out
}

/// Рассылает состав комнаты и отдаёт его же вызывающему.
///
/// Себя вписываем под своим номером, а не под первым: хостом мог стать
/// гость, и если бы он вдруг назвался номером один, у всех разъехались бы
/// номера — вместе с ними громкости и заглушки, настроенные на людей.
fn broadcast_peers(
    socket: &UdpSocket,
    table: &Arc<Mutex<PeerTable>>,
    host_name: &str,
    host_key: &[u8; 32],
    room: &RoomRef,
) -> Vec<RosterEntry> {
    let me = room.host_id();
    let my_addr = *room.my_addr.lock().unwrap();
    let t = table.lock().unwrap();
    let roster: Vec<RosterEntry> =
        std::iter::once((me, host_name.to_string(), *host_key, my_addr))
            .chain(t.snapshot())
            .collect();
    let addrs = t.addrs_except(None);
    drop(t);

    let msg = encode_peers(&roster);
    for addr in addrs {
        let _ = socket.send_to(&msg, addr);
    }
    *room.roster.lock().unwrap() = roster.clone();
    remember_room(&roster);
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
    mesh: Direct,
    direct: Direct,
    punch: PunchList,
    host_seen: HostSeen,
    room: RoomRef,
    load: Arc<audio::Load>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut known = Known::load();
        // Уже были в комнате хоть раз: следующий вход — не первый, а возврат.
        let mut was_connected = false;
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
            let started = Instant::now();
            let kind = buf[3];
            let body = &buf[4..n];
            // Роль может смениться посреди встречи, поэтому спрашиваем её
            // на каждом пакете, а не запоминаем при запуске.
            let is_host = room.is_host();

            // Кто прислал пакет — решаем по адресу, а не по тому, что
            // написано в теле. Это ключевое место: почти всё остальное в
            // протоколе доверяло номеру из тела пакета, а его может
            // написать кто угодно. Отправитель либо проверенный участник
            // (у хоста), либо сам хост, либо тот, до кого хост дал нам
            // прямой путь.
            let sender: Option<u16> = if is_host {
                table
                    .lock()
                    .unwrap()
                    .peers
                    .iter()
                    .find(|p| p.addr == from && p.verified)
                    .map(|p| p.id)
            } else if *locked.lock().unwrap() == Some(from) {
                Some(room.host_id())
            } else {
                mesh.lock()
                    .unwrap()
                    .iter()
                    .find(|(_, a)| **a == from)
                    .map(|(id, _)| *id)
            };

            // Любой разобранный пакет от хоста — признак, что связь жива.
            // Молчание дольше HOST_SILENCE означает, что путь оборвался.
            if !is_host && *locked.lock().unwrap() == Some(from) {
                *host_seen.lock().unwrap() = Instant::now();
                shared
                    .lock()
                    .unwrap()
                    .peer_seen
                    .insert(room.host_id(), Instant::now());
            }

            match kind {
                T_HELLO if is_host => {
                    if body.len() < 48 {
                        continue;
                    }
                    let mut public = [0u8; 32];
                    public.copy_from_slice(&body[..32]);
                    // Задача от гостя: он хочет убедиться, что мы — это мы.
                    let ask = &body[32..48];
                    let name = String::from_utf8_lossy(&body[48..])
                        .chars()
                        .take(24)
                        .collect::<String>();

                    let mut t = table.lock().unwrap();
                    // Комната не резиновая. Без этого поток HELLO со
                    // случайными ключами набивал таблицу без предела —
                    // и это укладывало приложение по памяти.
                    if t.peers.len() >= MAX_PEERS
                        && !t.peers.iter().any(|p| p.public == public)
                    {
                        continue;
                    }
                    // Человека узнаём по ключу, а не по адресу: адрес меняется
                    // от переключения сети, а ключ — нет.
                    let (id, challenge) = match t.peers.iter_mut().find(|p| p.public == public) {
                        Some(p) if p.addr == from => {
                            p.last_seen = Instant::now();
                            (p.id, p.challenge)
                        }
                        Some(p) => {
                            // Тот же ключ с нового адреса — похоже, сеть у
                            // человека сменилась. Даём задачу этому адресу
                            // и ждём подписи, прежде чем переезжать.
                            if p.pending.len() >= 4 && !p.pending.contains_key(&from) {
                                continue;
                            }
                            let ask = match p.pending.get(&from) {
                                Some(c) => *c,
                                None => {
                                    let Ok(fresh) = identity::random_bytes::<16>() else {
                                        continue;
                                    };
                                    p.pending.insert(from, fresh);
                                    fresh
                                }
                            };
                            (p.id, ask)
                        }
                        None => {
                            let id = t.next_id;
                            // Номера не должны ни переполниться, ни начать
                            // повторяться: к номеру привязаны громкости,
                            // заглушки и дорожки микшера. Кончились — не
                            // пускаем, это честнее, чем выдать дубль.
                            if id == u16::MAX {
                                continue;
                            }
                            t.next_id += 1;
                            // Случайная строка, которую гость должен подписать.
                            // Без неё открытый ключ можно было бы просто скопировать.
                            let Ok(challenge) = identity::random_bytes::<16>() else {
                                continue;
                            };
                            t.peers.push(Peer {
                                id,
                                name: name.clone(),
                                addr: from,
                                last_seen: Instant::now(),
                                public,
                                challenge,
                                pending: HashMap::new(),
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
                    // Свой номер: хостом мог стать гость, и вернувшемуся
                    // неоткуда узнать, под каким номером его теперь искать.
                    msg.extend_from_slice(&room.host_id().to_be_bytes());
                    // И подпись под задачей гостя: доказательство, что
                    // ключом владеем мы, а не тот, кто увидел код.
                    msg.extend_from_slice(&identity.sign(&welcome_msg(ask, room.host_id(), id)));
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
                    let Some(p) = t
                        .peers
                        .iter_mut()
                        .find(|p| p.id == id && (p.addr == from || p.pending.contains_key(&from)))
                    else {
                        continue;
                    };
                    let moving = p.addr != from;
                    if p.verified && !moving {
                        continue;
                    }
                    // Задача та, что выдана именно этому адресу.
                    let ask = if moving {
                        match p.pending.get(&from) {
                            Some(c) => *c,
                            None => continue,
                        }
                    } else {
                        p.challenge
                    };
                    if !identity::verify(&p.public, &auth_msg(&ask), &sig) {
                        let name = p.name.clone();
                        drop(t);
                        shared
                            .lock()
                            .unwrap()
                            .log(format!("подпись {name} не сошлась — не пускаем"));
                        continue;
                    }
                    p.verified = true;
                    p.last_seen = Instant::now();
                    if moving {
                        p.addr = from;
                        p.pending.clear();
                    }
                    let (name, fp) = (p.name.clone(), identity::fingerprint(&p.public));
                    drop(t);

                    if moving {
                        shared
                            .lock()
                            .unwrap()
                            .log(format!("{name} переехал на {from}"));
                    }

                    // Вот здесь запоминать можно: подпись только что
                    // сошлась, значит ключом владеет тот, кто его предъявил.
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

                    let roster = broadcast_peers(&socket, &table, &nickname, &identity.public, &room);
                    let mut sh = shared.lock().unwrap();
                    sh.peers = roster.iter().map(|(i, n, _, _)| (*i, n.clone())).collect();
                }

                T_HELLO if !is_host => {
                    // Дверь в комнату — любой из своих. Пришедшему незачем
                    // знать, кто сейчас хост: мы просто показываем на него.
                    // Ради этого и затевалось: человек возвращается по
                    // старому коду, а не выясняет в игровом чате, у кого
                    // теперь комната.
                    //
                    // Но показываем только тому, кого знаем в лицо. Раньше
                    // мы выдавали ключ и адрес хоста любому, кто прислал
                    // четыре байта, — это и утечка, и усилитель для чужого
                    // потока: на короткий запрос уходил ответ вчетверо
                    // длиннее, с подставным обратным адресом.
                    if body.len() < 32 {
                        continue;
                    }
                    let mut asker = [0u8; 32];
                    asker.copy_from_slice(&body[..32]);
                    let ours = room
                        .roster
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|(_, _, pk, _)| *pk == asker);
                    if !ours {
                        continue;
                    }
                    let hid = room.host_id();
                    let roster = room.roster.lock().unwrap().clone();
                    let Some((_, _, pk, addr)) = roster.iter().find(|(i, _, _, _)| *i == hid)
                    else {
                        continue;
                    };
                    // Адрес берём из состава, а не тот, по которому ходим
                    // сами: наш может оказаться домашним и чужому бесполезен.
                    let Some(addr) = addr.or(*locked.lock().unwrap()) else {
                        continue;
                    };
                    let mut msg = header(T_HOST);
                    msg.extend_from_slice(&hid.to_be_bytes());
                    msg.extend_from_slice(pk);
                    msg.extend_from_slice(&encode_addr(Some(addr)));
                    let _ = socket.send_to(&msg, from);
                }

                T_HOST => {
                    if body.len() < 40 {
                        continue;
                    }
                    let id = u16::from_be_bytes([body[0], body[1]]);
                    let mut pk = [0u8; 32];
                    pk.copy_from_slice(&body[2..34]);
                    let Some(addr) = decode_addr(&body[34..40]) else {
                        continue;
                    };
                    let me = *my_id.lock().unwrap();
                    if id == me || (id == room.host_id() && !room.host_gone.load(Ordering::Relaxed))
                    {
                        continue;
                    }

                    // Комнату не отдаём, пока прежний хост жив. Иначе любой
                    // участник забирал бы её себе одним пакетом посреди
                    // разговора.
                    let in_room = locked.lock().unwrap().is_some() || is_host;
                    if in_room
                        && !room.host_gone.load(Ordering::Relaxed)
                        && host_seen.lock().unwrap().elapsed() < HOST_SILENCE
                    {
                        continue;
                    }
                    let roster = room.roster.lock().unwrap().clone();

                    // Два разных случая, и путать их нельзя.
                    //
                    // Первый: мы в комнате, и кто-то объявляет себя новым
                    // хостом. Верим, только если пакет пришёл именно от
                    // него самого (узнали по адресу) и если ключ сходится с
                    // тем, что стоит в составе под этим номером. Раньше
                    // хватало «такой ключ где-то в составе есть» — а свой
                    // ключ в составе есть у каждого участника, и любой из
                    // них мог увести комнату.
                    let announced = sender == Some(id)
                        && roster
                            .iter()
                            .any(|(i, _, k, _)| *i == id && *k == pk);

                    // Второй: мы ещё стучимся и получили перенаправление от
                    // того, к кому шли. Проверять нечего и незачем: доверие
                    // тут ровно то же, что и к коду приглашения, а ключ
                    // хоста всё равно будет проверен подписью.
                    let by_code = locked.lock().unwrap().is_none()
                        && room.targets.lock().unwrap().contains(&from);

                    if !announced && !by_code {
                        continue;
                    }
                    // Если хост сейчас мы, уступаем только младшему номеру:
                    // иначе двое, объявившиеся одновременно, гоняли бы
                    // комнату друг другу без конца.
                    if is_host && id > me {
                        continue;
                    }
                    follow_host(
                        &shared, &table, &room, &locked, &host_seen, id, pk, addr, is_host,
                    );
                }

                T_PEERS if !is_host => {
                    // Только от хоста. Раньше состав комнаты принимался от
                    // кого угодно — а из него растёт всё остальное: куда
                    // слать звук, кому верить, кого выбирать хостом. Один
                    // пакет от постороннего отдавал ему комнату целиком.
                    if *locked.lock().unwrap() != Some(from) {
                        continue;
                    }
                    let roster = decode_peers(body);
                    if roster.is_empty() || roster.len() > MAX_PEERS + 1 {
                        continue;
                    }
                    // Состав нужен не только для показа: если хост уйдёт,
                    // выбирать нового будет не у кого спросить — только по
                    // этому списку.
                    *room.roster.lock().unwrap() = roster.clone();
                    remember_room(&roster);
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
                        // Показываем, но НЕ запоминаем. Состав приходит от
                        // хоста, а подписи этих людей проверял он, не мы.
                        // Раньше здесь ключ из чужих рук записывался в файл
                        // знакомых — и потом настоящий человек приходил под
                        // предупреждением «у него другой ключ», а самозванец
                        // проходил молча. Это ровно наоборот тому, ради чего
                        // доверие при первой встрече и придумано.
                        if sh.trust.get(id) != Some(&trust) && trust == Trust::Changed {
                            sh.log(format!("ВНИМАНИЕ: у {name} другой ключ ({fp})"));
                        }
                        sh.fingerprints.insert(*id, fp);
                        sh.trust.insert(*id, trust);
                    }
                }

                T_WELCOME if !is_host => {
                    // Пока мы уже в комнате, второе приглашение нам не
                    // нужно. Раньше любой мог переслать подслушанный
                    // WELCOME со своего адреса и перецепить нас на себя
                    // посреди разговора — подпись-то в нём настоящая.
                    if locked.lock().unwrap().is_some() {
                        continue;
                    }
                    // И отвечать нам может только тот, к кому мы стучались.
                    if !room.targets.lock().unwrap().contains(&from) {
                        continue;
                    }
                    if body.len() >= 116 {
                        let id = u16::from_be_bytes([body[0], body[1]]);
                        let challenge = &body[2..18];
                        let mut host_pk = [0u8; 32];
                        host_pk.copy_from_slice(&body[18..50]);
                        let host_id = u16::from_be_bytes([body[50], body[51]]);
                        let mut host_sig = [0u8; 64];
                        host_sig.copy_from_slice(&body[52..116]);

                        // Хост обязан подписать нашу задачу. Это и есть
                        // вторая половина опознания: раньше подпись
                        // предъявлял только гость, и хостом мог назваться
                        // любой, кто видел код приглашения.
                        let ask = *room.my_challenge.lock().unwrap();
                        if !identity::verify(&host_pk, &welcome_msg(&ask, host_id, id), &host_sig) {
                            shared
                                .lock()
                                .unwrap()
                                .log("на стук ответили без подписи — не подключаемся");
                            continue;
                        }

                        // Ключ из кода приглашения обязан совпасть: иначе это
                        // не тот, к кому нас звали.
                        if let Some(expect) = *room.expect_host.lock().unwrap() {
                            if expect != host_pk {
                                shared.lock().unwrap().log(
                                    "ключ хоста не совпал с кодом приглашения — не подключаемся",
                                );
                                continue;
                            }
                        } else {
                            // Возвращаемся в запомненную комнату: кто в ней
                            // теперь хост — неизвестно, но он обязан быть
                            // одним из тех, кого мы там видели.
                            //
                            // Пустой состав означает другое: адрес вписали
                            // руками, ключа мы не знаем и знать не могли.
                            // Тогда доверие — к адресу, как при входе по
                            // коду: подпись уже проверена выше, а смену
                            // ключа под знакомым именем поймает первая
                            // встреча.
                            let roster = room.roster.lock().unwrap().clone();
                            let ok = roster.is_empty()
                                || roster.iter().any(|(_, _, pk, _)| *pk == host_pk);
                            if !ok {
                                shared
                                    .lock()
                                    .unwrap()
                                    .log("на стук ответил чужой — не подключаемся");
                                continue;
                            }
                        }
                        *room.host_id.lock().unwrap() = host_id;
                        room.joined.store(true, Ordering::Relaxed);
                        // Задача отработала: следующая попытка получит новую,
                        // чтобы этот же подписанный ответ нельзя было
                        // предъявить снова.
                        if let Ok(fresh) = identity::random_bytes::<16>() {
                            *room.my_challenge.lock().unwrap() = fresh;
                        }

                        let mut reply = header(T_AUTH);
                        reply.extend_from_slice(&id.to_be_bytes());
                        reply.extend_from_slice(&identity.sign(&auth_msg(challenge)));
                        let _ = socket.send_to(&reply, from);

                        *my_id.lock().unwrap() = id;
                        // Запоминаем именно тот адрес, откуда пришёл ответ:
                        // остальные кандидаты больше не нужны.
                        let mut lock = locked.lock().unwrap();
                        let first = lock.is_none();
                        *lock = Some(from);
                        drop(lock);
                        *host_seen.lock().unwrap() = Instant::now();

                        // Хост — тоже прямой путь, причём уже проверенный:
                        // именно с этого адреса он нам и ответил.
                        mesh.lock().unwrap().insert(host_id, from);
                        direct.lock().unwrap().insert(host_id, from);

                        if first {
                            let mut s = shared.lock().unwrap();
                            s.my_id = id;
                            s.connected = true;
                            s.status = "в комнате".into();
                            if was_connected {
                                // Возвращение после обрыва: ключ и номер те же,
                                // повторять их незачем.
                                s.log(format!("связь восстановлена, хост на {from}"));
                            } else {
                                s.log(format!("хост ответил с {from}, наш номер {id}"));
                                s.log(format!("ключ хоста {}", identity::fingerprint(&host_pk)));
                            }
                            was_connected = true;
                        }
                    }
                }

                T_AUDIO => {
                    if body.len() < 5 {
                        continue;
                    }
                    let Some(src) = speaker(sender, body, &room, is_host) else {
                        continue;
                    };
                    let seq = u16::from_be_bytes([body[2], body[3]]);
                    let voiced = body[4] & 1 != 0;

                    // Хост пересылает пакет остальным, подставив настоящий
                    // номер отправителя. Без этого пересылка была бы дырой:
                    // участник написал бы в теле чужой номер, а хост
                    // добросовестно разнёс бы это всем как чужую речь.
                    if is_host {
                        let mut t = table.lock().unwrap();
                        if let Some(p) = t.peers.iter_mut().find(|p| p.addr == from) {
                            p.last_seen = Instant::now();
                        }
                        let addrs = t.addrs_except(Some(from));
                        drop(t);
                        let out = with_src(&buf[..n], src);
                        for addr in addrs {
                            let _ = socket.send_to(&out, addr);
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
                    {
                        let mut sh = shared.lock().unwrap();
                        sh.peer_seen.insert(src, Instant::now());
                        if voiced {
                            sh.voice_seen.insert(src, Instant::now());
                        }
                    }

                    // Новый декодер заводим, только пока их немного: на
                    // каждого собеседника это состояние Opus и дорожка
                    // микшера, и раньше посторонний мог развести их
                    // десятками тысяч, уложив приложение по памяти.
                    let full = streams.len() >= MAX_STREAMS;
                    let stream = match streams.entry(src) {
                        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::hash_map::Entry::Vacant(e) => {
                            if full {
                                continue;
                            }
                            match Incoming::new() {
                                Ok(v) => e.insert(v),
                                Err(_) => continue,
                            }
                        }
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
                    let Some(src) = speaker(sender, body, &room, is_host) else {
                        continue;
                    };
                    let text = String::from_utf8_lossy(&body[2..])
                        .chars()
                        .take(400)
                        .collect::<String>();

                    if is_host {
                        let addrs = table.lock().unwrap().addrs_except(Some(from));
                        let out = with_src(&buf[..n], src);
                        for addr in addrs {
                            let _ = socket.send_to(&out, addr);
                        }
                    }

                    let mut sh = shared.lock().unwrap();
                    sh.peer_seen.insert(src, Instant::now());
                    sh.chat.push((src, text));
                    if sh.chat.len() > 200 {
                        sh.chat.remove(0);
                    }
                }

                T_STATE => {
                    if body.len() < 3 {
                        continue;
                    }
                    let Some(src) = speaker(sender, body, &room, is_host) else {
                        continue;
                    };
                    let muted = body[2] & 1 != 0;

                    // Хост пересылает состояние остальным, тоже подставив
                    // настоящий номер.
                    if is_host {
                        let addrs = table.lock().unwrap().addrs_except(Some(from));
                        let out = with_src(&buf[..n], src);
                        for addr in addrs {
                            let _ = socket.send_to(&out, addr);
                        }
                    }

                    let mut sh = shared.lock().unwrap();
                    sh.peer_seen.insert(src, Instant::now());
                    if muted {
                        sh.muted_peers.insert(src, Instant::now());
                    } else {
                        sh.muted_peers.remove(&src);
                    }
                }

                T_PING if is_host => {
                    if sender.is_none() {
                        continue;
                    }
                    let mut t = table.lock().unwrap();
                    let id = t.peers.iter_mut().find(|p| p.addr == from).map(|p| {
                        p.last_seen = Instant::now();
                        p.id
                    });
                    drop(t);
                    if let Some(id) = id {
                        shared.lock().unwrap().peer_seen.insert(id, Instant::now());
                    }
                }

                T_BYE if !is_host => {
                    if body.len() < 2 {
                        continue;
                    }
                    let who = u16::from_be_bytes([body[0], body[1]]);
                    // Прощаться можно только за себя, а пересылать чужое
                    // прощание — только хосту. Раньше любой мог одним
                    // пакетом выкинуть из комнаты кого угодно, а назвав
                    // номер хоста — устроить перевыборы, и так по кругу.
                    let ok = match sender {
                        Some(s) if s == who => true,
                        Some(s) => s == room.host_id(),
                        None => false,
                    };
                    if !ok {
                        continue;
                    }
                    if who == room.host_id() {
                        // Ушёл хост. Ждать двенадцать секунд тишины незачем —
                        // он сказал об этом сам.
                        room.host_gone.store(true, Ordering::Relaxed);
                    }
                    mixer.remove(who);
                    let mut sh = shared.lock().unwrap();
                    sh.peers.retain(|(id, _)| *id != who);
                    sh.peer_seen.remove(&who);
                    sh.voice_seen.remove(&who);
                    sh.log(format!("#{who} вышел"));
                    drop(sh);
                    mesh.lock().unwrap().remove(&who);
                    direct.lock().unwrap().remove(&who);
                }

                T_BYE if is_host => {
                    // Только от того, кто в комнате, и только за себя.
                    // Раньше хост пересылал это всем ещё до проверки, то
                    // есть послушно разносил чужую команду «выкинуть».
                    if body.len() < 2 || sender.is_none() {
                        continue;
                    }
                    if sender != Some(u16::from_be_bytes([body[0], body[1]])) {
                        continue;
                    }
                    {
                        let t = table.lock().unwrap();
                        for addr in t.addrs_except(Some(from)) {
                            let _ = socket.send_to(&buf[..n], addr);
                        }
                    }
                    let mut t = table.lock().unwrap();
                    if let Some(pos) = t.peers.iter().position(|p| p.addr == from) {
                        let gone = t.peers.remove(pos);
                        drop(t);
                        mixer.remove(gone.id);
                        shared
                            .lock()
                            .unwrap()
                            .log(format!("{} отключился", gone.name));
                        let roster = broadcast_peers(&socket, &table, &nickname, &identity.public, &room);
                        shared.lock().unwrap().peers =
                            roster.iter().map(|(i, n, _, _)| (*i, n.clone())).collect();
                    }
                }

                _ => {}
            }

            if kind == T_AUDIO {
                load.recv.fetch_add(1, Ordering::Relaxed);
            }
            audio::Load::add(&load.rx_ns, started);
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
    room: RoomRef,
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
            let is_host = room.is_host();
            let target = if is_host { None } else { *locked.lock().unwrap() };
            if !is_host && target.is_none() {
                continue;
            }

            let started = Instant::now();
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
            controls.load.sent.fetch_add(1, Ordering::Relaxed);
            audio::Load::add(&controls.load.enc_ns, started);
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
    locked: Locked,
    punch: PunchList,
    mixer: Arc<Mixer>,
    controls: audio::Controls,
    my_id: Arc<Mutex<u16>>,
    chat_rx: Receiver<String>,
    identity: Arc<Identity>,
    mesh: Direct,
    host_seen: HostSeen,
    room: RoomRef,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut tick = 0u32;
        let mut hinted = false;
        // Сколько раз подряд теряли хоста. Нужно только для сообщения в журнал.
        let mut drops = 0u32;

        while !stop.load(Ordering::Relaxed) {
            // Шаг короткий, чтобы отправка сообщения не ждала полсекунды;
            // всё периодическое делается раз в пять шагов.
            thread::sleep(Duration::from_millis(100));
            let is_host = room.is_host();

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
                } else {
                    if let Some(host) = *locked.lock().unwrap() {
                        let _ = socket.send_to(&msg, host);
                    }
                    // И всем, до кого добиваем напрямую. Это единственный
                    // след жизни молчащего человека: пока хост на месте, он
                    // виден через пересылку, а без хоста — только отсюда.
                    // По нему же потом выбирается новый хост.
                    for addr in mesh.lock().unwrap().values() {
                        let _ = socket.send_to(&msg, *addr);
                    }
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
                    let roster = broadcast_peers(&socket, &table, &nickname, &identity.public, &room);
                    let ids: Vec<u16> = roster.iter().map(|(id, _, _, _)| *id).collect();
                    mixer.retain(&ids);
                    shared.lock().unwrap().peers =
                        roster.iter().map(|(i, n, _, _)| (*i, n.clone())).collect();
                }
                continue;
            }

            if !is_host {
                let silent = host_seen.lock().unwrap().elapsed();
                let gone = room.host_gone.load(Ordering::Relaxed);
                let me = *my_id.lock().unwrap();

                if room.joined.load(Ordering::Relaxed) && (gone || silent > HOST_GONE) {
                    // Хоста больше нет. Прямые пути между остальными живы,
                    // разговор не прервался — не хватает только того, кто
                    // пускает новых и рассылает состав. Выбираем его сами.
                    match elect(&room, &shared, me) {
                        Some((id, ..)) if id == me => {
                            become_host(
                                &socket, &room, &table, &shared, &locked, &nickname, &identity,
                                me,
                            );
                        }
                        Some((id, _, pk, addr)) => {
                            if let Some(addr) = addr {
                                follow_host(
                                    &shared, &table, &room, &locked, &host_seen, id, pk, addr,
                                    false,
                                );
                            }
                        }
                        None => {
                            room.host_gone.store(false, Ordering::Relaxed);
                            *host_seen.lock().unwrap() = Instant::now();
                            shared
                                .lock()
                                .unwrap()
                                .log("хост ушёл, а больше в комнате никого — звать некого");
                        }
                    }
                } else if silent > HOST_SILENCE && locked.lock().unwrap().is_some() {
                    // Сеть у людей меняется на ходу: отвалился VPN,
                    // переключился Wi-Fi — и внешний адрес стал другим. Хост
                    // об этом не знает и продолжает слать на мёртвый адрес.
                    // Отпускаем найденный адрес и знакомимся заново: хост
                    // узнаёт нас по ключу и просто переставит адрес.
                    //
                    // Прямые пути до остальных при этом не трогаем — они
                    // ни в чём не виноваты, и звук по ним идёт как шёл.
                    *locked.lock().unwrap() = None;
                    // Новая задача на каждую попытку: подписанный ответ,
                    // записанный кем-то раньше, не должен подойти снова.
                    if let Ok(fresh) = identity::random_bytes::<16>() {
                        *room.my_challenge.lock().unwrap() = fresh;
                    }
                    drops += 1;
                    hinted = false;
                    tick = 1;
                    let mut s = shared.lock().unwrap();
                    s.status = "связь потеряна, переподключаемся".into();
                    // Раз в минуту, а не каждые пять секунд: если хост ушёл
                    // насовсем, журнал не должен превращаться в ленту.
                    if drops == 1 || drops % 12 == 0 {
                        s.log(format!(
                            "от хоста {} секунд тишины — восстанавливаем связь (попытка {drops})",
                            silent.as_secs()
                        ));
                    }
                }
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
                    msg.extend_from_slice(&*room.my_challenge.lock().unwrap());
                    msg.extend_from_slice(nickname.as_bytes());
                    for addr in room.targets.lock().unwrap().iter() {
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

        // Прощаемся несколько раз: один пакет UDP легко теряет, а
        // повиснуть в чужом списке на восемь секунд — некрасиво.
        let mut bye = header(T_BYE);
        bye.extend_from_slice(&my_id.lock().unwrap().to_be_bytes());
        for _ in 0..3 {
            if room.is_host() {
                let addrs = table.lock().unwrap().addrs_except(None);
                for addr in addrs {
                    let _ = socket.send_to(&bye, addr);
                }
            } else if let Some(host) = *locked.lock().unwrap() {
                let _ = socket.send_to(&bye, host);
            }
            thread::sleep(Duration::from_millis(40));
        }
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Поток случайных байт без внешних зависимостей: для проверки того,
    /// что разбор пакетов не падает ни на чём.
    struct Noise(u64);
    impl Noise {
        fn byte(&mut self) -> u8 {
            // xorshift: годится ровно для того, чтобы насыпать мусора.
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 33) as u8
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.byte()).collect()
        }
    }

    /// Главное, чего мы боимся: чтобы специально сделанный пакет не ронял
    /// приложение. Падение — это отказ в обслуживании всем, кому его
    /// послали, поэтому сыплем мусор во все разборщики.
    #[test]
    fn разбор_не_падает_на_мусоре() {
        let mut noise = Noise(0x5EED);
        for len in 0..300usize {
            for _ in 0..20 {
                let junk = noise.bytes(len);
                let _ = decode_peers(&junk);
                let _ = decode_addr(&junk);
                let text = String::from_utf8_lossy(&junk).to_string();
                let _ = decode_invite(&text);
                let _ = invite_key(&text);
                let _ = identity::from_hex(&text);
                let _ = identity::parse_public(&text);
            }
        }
    }

    /// Та самая паника, из-за которой присланный «код приглашения» ронял
    /// приложение: срез строки попадал внутрь многобайтового знака.
    #[test]
    fn многобайтовые_знаки_не_роняют_разбор() {
        for s in ["\u{20ac}\u{20ac}", "\u{439}\u{439}", "\u{2014}", "a\u{20ac}", "\u{401}\u{401}\u{401}"] {
            assert!(identity::from_hex(s).is_none());
            assert!(identity::parse_public(s).is_err());
        }
        assert_eq!(identity::from_hex("0aFF"), Some(vec![0x0a, 0xff]));
        assert_eq!(identity::from_hex("abc"), None);
        assert_eq!(identity::from_hex("zz"), None);
    }

    #[test]
    fn адрес_короче_шести_байт_не_падает() {
        for n in 0..6 {
            assert!(decode_addr(&vec![7u8; n]).is_none());
        }
        assert!(decode_addr(&[127, 0, 0, 1, 0xB8, 0x0C]).is_some());
    }

    #[test]
    fn состав_комнаты_переживает_дорогу_туда_и_обратно() {
        let roster: Vec<RosterEntry> = vec![
            (1, "\u{414}\u{430}\u{43d}\u{44f}".into(), [7u8; 32], "127.0.0.1:47100".parse().ok()),
            (2, "\u{41b}\u{438}\u{437}\u{430}".into(), [9u8; 32], None),
        ];
        let bytes = encode_peers(&roster);
        let back = decode_peers(&bytes[4..]);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].0, 1);
        assert_eq!(back[0].2, [7u8; 32]);
        assert_eq!(back[1].3, None);
    }

    /// Взаимное опознание: подпись сходится только у владельца ключа и
    /// только под той задачей, которую ему дали.
    #[test]
    fn подпись_проверяется_и_не_переигрывается() {
        let me = Identity::from_seed_for_test([3u8; 32]);
        let ask = [42u8; 16];
        let sig = me.sign(&ask);

        assert!(identity::verify(&me.public, &ask, &sig));
        // Другая задача — записанный ответ не подходит.
        assert!(!identity::verify(&me.public, &[43u8; 16], &sig));
        // Чужой ключ — не подходит.
        let other = Identity::from_seed_for_test([4u8; 32]);
        assert!(!identity::verify(&other.public, &ask, &sig));
        // Испорченная подпись — не подходит.
        let mut broken = sig;
        broken[0] ^= 1;
        assert!(!identity::verify(&me.public, &ask, &broken));
    }

    #[test]
    fn джиттер_буфер_не_растёт_без_предела() {
        let mut inc = Incoming::new().unwrap();
        let mut pcm = vec![0i16; FRAME * 2];
        let mut out = Vec::new();
        for seq in (0u16..2000).step_by(7) {
            inc.push(seq, &[0xFC, 0xFF, 0xFE], &mut pcm, &mut out);
            assert!(inc.pending.len() <= JITTER_FRAMES * 6);
            out.clear();
        }
    }
}
