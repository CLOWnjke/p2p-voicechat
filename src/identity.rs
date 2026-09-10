//! Кто есть кто.
//!
//! Имя — это просто строка, представиться можно кем угодно. Поэтому у каждой
//! установки есть своя пара ключей: при входе в комнату хост присылает
//! случайную строку, гость подписывает её своим ключом, и хост убеждается,
//! что подпись сходится с предъявленным открытым ключом.
//!
//! Дальше работает доверие при первой встрече, как в SSH: увидев человека
//! впервые, мы запоминаем его отпечаток рядом с именем. Если в следующий раз
//! под тем же именем придёт другой ключ — об этом будет сказано вслух.

use anyhow::{anyhow, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Своя пара ключей. Создаётся один раз и лежит рядом с настройками.
pub struct Identity {
    key: SigningKey,
    pub public: [u8; 32],
}

impl Identity {
    pub fn load_or_create() -> Self {
        let path = key_path();
        if let Some(seed) = path.as_ref().and_then(|p| read_seed(p)) {
            return Self::from_seed(seed);
        }

        let seed = random_bytes::<32>();
        if let Some(p) = &path {
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
            }
            let _ = fs::write(p, hex(&seed));
        }
        Self::from_seed(seed)
    }

    fn from_seed(seed: [u8; 32]) -> Self {
        let key = SigningKey::from_bytes(&seed);
        let public = key.verifying_key().to_bytes();
        Self { key, public }
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.key.sign(msg).to_bytes()
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public)
    }
}

/// Проверяет, что подпись под сообщением сделана владельцем этого ключа.
pub fn verify(public: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(public) else {
        return false;
    };
    vk.verify(msg, &Signature::from_bytes(sig)).is_ok()
}

/// Короткий отпечаток для глаз: восемь знаков, разбитых пополам.
/// Полный ключ никто сверять не станет, а восемь знаков прочитать вслух можно.
pub fn fingerprint(public: &[u8; 32]) -> String {
    let h = hex(&public[..4]).to_uppercase();
    format!("{}-{}", &h[..4], &h[4..])
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    if getrandom::fill(&mut buf).is_err() {
        // Запасной путь на случай, если системный источник недоступен.
        // Он слабее, но лучше, чем нули.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0x9E37_79B9);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((t >> ((i % 16) * 8)) as u8) ^ (i as u8).wrapping_mul(97);
        }
    }
    buf
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn key_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "voicechat").map(|d| d.config_dir().join("identity.key"))
}

fn known_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "voicechat").map(|d| d.config_dir().join("known_peers"))
}

fn read_seed(path: &PathBuf) -> Option<[u8; 32]> {
    let text = fs::read_to_string(path).ok()?;
    let bytes = from_hex(text.trim())?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    Some(arr)
}

/// Кого мы уже видели: имя → отпечаток. Доверие при первой встрече.
#[derive(Default)]
pub struct Known {
    map: HashMap<String, String>,
}

impl Known {
    pub fn load() -> Self {
        let Some(path) = known_path() else {
            return Self::default();
        };
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        let mut map = HashMap::new();
        for line in text.lines() {
            if let Some((fp, name)) = line.split_once(' ') {
                map.insert(name.trim().to_string(), fp.trim().to_string());
            }
        }
        Self { map }
    }

    /// Что мы думаем про эту пару имя-отпечаток.
    pub fn check(&self, name: &str, fp: &str) -> Trust {
        match self.map.get(name) {
            None => Trust::New,
            Some(known) if known == fp => Trust::Known,
            Some(_) => Trust::Changed,
        }
    }

    /// Запоминает пару. Вызывается только после успешной проверки подписи.
    pub fn remember(&mut self, name: &str, fp: &str) {
        if self.map.get(name).map(|v| v.as_str()) == Some(fp) {
            return;
        }
        self.map.insert(name.to_string(), fp.to_string());
        self.save();
    }

    fn save(&self) {
        let Some(path) = known_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let body: String = self
            .map
            .iter()
            .map(|(name, fp)| format!("{fp} {name}\n"))
            .collect();
        let _ = fs::write(path, body);
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Trust {
    /// Видим впервые — запомнили.
    New,
    /// Тот же ключ, что и в прошлый раз.
    Known,
    /// Под знакомым именем пришёл другой ключ. Повод насторожиться.
    Changed,
}

/// Разбирает открытый ключ из шестнадцатеричной записи в коде приглашения.
pub fn parse_public(s: &str) -> Result<[u8; 32]> {
    let bytes = from_hex(s.trim()).ok_or_else(|| anyhow!("испорченный ключ"))?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("ключ неверной длины"))
}
