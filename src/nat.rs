//! Определение внешнего адреса (STUN) и попытка открыть порт на роутере (UPnP).
//!
//! Важная тонкость: STUN-запрос обязан уходить с того же UDP-сокета, по которому
//! потом пойдёт звук. NAT создаёт отображение для конкретного сокета, и адрес,
//! полученный с другого сокета, к нашему трафику отношения иметь не будет.

use anyhow::{anyhow, Result};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

const STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.cloudflare.com:3478",
    "stun.nextcloud.com:443",
];

const MAGIC_COOKIE: u32 = 0x2112_A442;

/// Спрашивает у публичных STUN-серверов, каким наш сокет виден снаружи.
pub fn discover_public_addr(sock: &UdpSocket) -> Result<SocketAddr> {
    let prev_timeout = sock.read_timeout().ok().flatten();
    sock.set_read_timeout(Some(Duration::from_millis(700)))?;

    let mut last_err = anyhow!("не удалось опросить ни один STUN-сервер");

    for server in STUN_SERVERS {
        match query_one(sock, server) {
            Ok(addr) => {
                let _ = sock.set_read_timeout(prev_timeout);
                return Ok(addr);
            }
            Err(e) => last_err = e,
        }
    }

    let _ = sock.set_read_timeout(prev_timeout);
    Err(last_err)
}

fn query_one(sock: &UdpSocket, server: &str) -> Result<SocketAddr> {
    let target = server
        .to_socket_addrs()?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| anyhow!("{server}: не удалось разрешить имя"))?;

    // Транзакционный идентификатор: 12 псевдослучайных байт.
    let mut txid = [0u8; 12];
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678)
        ^ (sock as *const _ as u64);
    for (i, b) in txid.iter_mut().enumerate() {
        *b = ((seed >> ((i % 8) * 8)) as u8) ^ (i as u8).wrapping_mul(31);
    }

    // Заголовок STUN Binding Request: 20 байт.
    let mut req = Vec::with_capacity(20);
    req.extend_from_slice(&0x0001u16.to_be_bytes()); // тип: Binding Request
    req.extend_from_slice(&0u16.to_be_bytes()); // длина тела
    req.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    req.extend_from_slice(&txid);

    // Три попытки: UDP теряет пакеты, а один потерянный запрос — не отказ сервера.
    for _ in 0..3 {
        sock.send_to(&req, target)?;

        let mut buf = [0u8; 1500];
        // Читаем до пяти пакетов: в сокет могут прилетать и посторонние.
        for _ in 0..5 {
            let (n, from) = match sock.recv_from(&mut buf) {
                Ok(v) => v,
                Err(_) => break,
            };
            if from.ip() != target.ip() {
                continue;
            }
            if let Some(addr) = parse_response(&buf[..n], &txid) {
                return Ok(addr);
            }
        }
    }

    Err(anyhow!("{server}: ответа нет"))
}

fn parse_response(buf: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if buf.len() < 20 {
        return None;
    }
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    if msg_type != 0x0101 {
        // 0x0101 — Binding Success Response
        return None;
    }
    if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != MAGIC_COOKIE {
        return None;
    }
    if &buf[8..20] != txid {
        return None;
    }

    let body_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let body = buf.get(20..20 + body_len)?;

    let mut i = 0usize;
    let mut fallback: Option<SocketAddr> = None;

    while i + 4 <= body.len() {
        let attr_type = u16::from_be_bytes([body[i], body[i + 1]]);
        let attr_len = u16::from_be_bytes([body[i + 2], body[i + 3]]) as usize;
        let value = body.get(i + 4..i + 4 + attr_len)?;

        match attr_type {
            // XOR-MAPPED-ADDRESS — предпочтительный: не ломается NAT-ами,
            // которые переписывают адреса внутри тела пакета.
            0x0020 => {
                if let Some(addr) = parse_addr(value, true) {
                    return Some(addr);
                }
            }
            // MAPPED-ADDRESS — старый вариант, годится как запасной.
            0x0001 => {
                if fallback.is_none() {
                    fallback = parse_addr(value, false);
                }
            }
            _ => {}
        }

        // Атрибуты выровнены по 4 байта.
        i += 4 + (attr_len + 3) / 4 * 4;
    }

    fallback
}

fn parse_addr(value: &[u8], xored: bool) -> Option<SocketAddr> {
    if value.len() < 8 || value[1] != 0x01 {
        return None; // поддерживаем только IPv4
    }
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    let mut octets = [value[4], value[5], value[6], value[7]];

    if xored {
        port ^= (MAGIC_COOKIE >> 16) as u16;
        for (i, b) in octets.iter_mut().enumerate() {
            *b ^= MAGIC_COOKIE.to_be_bytes()[i];
        }
    }

    Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
}

/// Локальный адрес в сети — нужен, чтобы попросить роутер пробросить порт именно на нас.
pub fn local_ipv4() -> Option<Ipv4Addr> {
    // Ничего никуда не отправляем: connect на UDP лишь выбирает исходящий интерфейс.
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("8.8.8.8:80").ok()?;
    match probe.local_addr().ok()?.ip() {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(_) => None,
    }
}

/// Просит роутер пробросить UDP-порт наружу. Работает далеко не везде:
/// UPnP может быть выключен, и это не ошибка, а обычное дело.
pub fn try_upnp(local_port: u16) -> Result<SocketAddr> {
    use igd_next::{search_gateway, PortMappingProtocol, SearchOptions};

    let local_ip = local_ipv4().ok_or_else(|| anyhow!("нет локального IPv4-адреса"))?;
    let local_addr = SocketAddr::new(IpAddr::V4(local_ip), local_port);

    let gateway = search_gateway(SearchOptions {
        timeout: Some(Duration::from_secs(3)),
        ..Default::default()
    })?;

    // Просим тот же номер порта наружу — так проще диагностировать.
    gateway.add_port(
        PortMappingProtocol::UDP,
        local_port,
        local_addr,
        3600,
        "voicechat",
    )?;

    let external_ip = gateway.get_external_ip()?;
    Ok(SocketAddr::new(external_ip, local_port))
}
