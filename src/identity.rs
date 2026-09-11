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
    pub fn load_or_create() -> Result<Self> {
        let path = key_path();
        if let Some(seed) = path.as_ref().and_then(|p| read_seed(p)) {
            return Ok(Self::from_seed(seed));
        }

        let seed = random_bytes::<32>()?;
        if let Some(p) = &path {
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
            }
            let _ = fs::write(p, hex(&seed));
            restrict(p);
        }
        Ok(Self::from_seed(seed))
    }

    /// Тот же путь, что и обычное создание, но без файла на диске:
    /// нужен проверкам, чтобы не трогать настоящий ключ человека.
    #[cfg(test)]
    pub fn from_seed_for_test(seed: [u8; 32]) -> Self {
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

/// Случайные байты от системы.
///
/// Запасного пути нет намеренно. Раньше здесь при отказе системного
/// источника байты строились из текущего времени — и ключ, и случайные
/// задачи для подписи становились предсказуемыми по моменту запуска, то
/// есть подбирались перебором. Молча выдать слабый ключ хуже, чем не
/// запуститься: человек будет думать, что защищён.
pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf)
        .map_err(|e| anyhow!("система не выдала случайные числа: {e}"))?;
    Ok(buf)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    // Работаем по байтам, а не по срезам строки. Срез `&s[i..i+2]` падает,
    // если попадает внутрь многобайтового знака, — а сюда приходит и код
    // приглашения от постороннего, и содержимое файла с диска. Падение от
    // вставленной строки — это отказ в обслуживании одним сообщением.
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return None;
    }
    let digit = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    bytes
        .chunks(2)
        .map(|p| Some(digit(p[0])? << 4 | digit(p[1])?))
        .collect()
}

/// Закрывает файл от других пользователей машины. Секретный ключ не
/// должен лежать с правами «читать всем»: на общем компьютере его просто
/// заберут и будут говорить от вашего имени.
fn restrict(path: &PathBuf) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Путь к файлу рядом с настройками. Каталог у всех свой, поэтому спрашиваем
/// его у системы, а не собираем руками.
pub fn config_file(name: &str) -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "voicechat").map(|d| d.config_dir().join(name))
}

fn key_path() -> Option<PathBuf> {
    config_file("identity.key")
}

fn known_path() -> Option<PathBuf> {
    config_file("known_peers")
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
