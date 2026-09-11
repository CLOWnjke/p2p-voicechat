//! Настройки, которые переживают закрытие приложения.
//!
//! Пожаловались справедливо: каждый запуск приходилось заново выставлять
//! чувствительность, порог, устройства. Настройка, которую нужно делать
//! каждый раз, — это не настройка, а обряд.
//!
//! Формат — строки «ключ = значение». Не потому, что так проще писать, а
//! потому, что так проще чинить: файл можно открыть блокнотом и увидеть,
//! что в нём написано. Ничего не понятное просто пропускается — испорченная
//! строка не должна мешать приложению запуститься.

use crate::audio::{Controls, DevicePrefs};
use std::collections::HashMap;
use std::fs;
use std::sync::atomic::Ordering;

#[derive(Clone, PartialEq)]
pub struct Settings {
    pub nickname: String,
    /// Выбранный вид, номер в `ui::PALETTES`.
    pub theme: usize,
    pub input: Option<String>,
    pub output: Option<String>,
    pub denoise: bool,
    pub aec: bool,
    pub gate: bool,
    pub gate_sensitivity: f32,
    pub gate_floor: f32,
    pub ptt: bool,
    pub ptt_key: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            nickname: default_nickname(),
            theme: 0,
            input: None,
            output: None,
            denoise: true,
            aec: true,
            gate: true,
            gate_sensitivity: 0.5,
            gate_floor: 0.02,
            ptt: false,
            ptt_key: "F8".into(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Self::default();
        let Some(text) = crate::identity::config_file("settings").and_then(|p| fs::read_to_string(p).ok())
        else {
            return s;
        };

        let map: HashMap<&str, &str> = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();

        if let Some(v) = map.get("имя") {
            let v: String = v.chars().take(24).collect();
            if !v.is_empty() {
                s.nickname = v;
            }
        }
        if let Some(v) = map.get("вид") {
            // По имени, а не по номеру: порядок палитр в коде может
            // поменяться, а «Фосфор» останется «Фосфором».
            if let Some(i) = crate::ui::PALETTES.iter().position(|p| p.name == *v) {
                s.theme = i;
            }
        }
        s.input = map.get("микрофон").map(|v| v.to_string()).filter(|v| !v.is_empty());
        s.output = map.get("динамики").map(|v| v.to_string()).filter(|v| !v.is_empty());
        if let Some(v) = map.get("эхоподавление") { s.aec = *v == "да"; }
        if let Some(v) = map.get("шумоподавление") { s.denoise = *v == "да"; }
        if let Some(v) = map.get("только-голос") { s.gate = *v == "да"; }
        if let Some(v) = map.get("чувствительность") {
            if let Ok(f) = v.parse::<f32>() { s.gate_sensitivity = f.clamp(0.0, 1.0); }
        }
        if let Some(v) = map.get("порог-тишины") {
            if let Ok(f) = v.parse::<f32>() { s.gate_floor = f.clamp(0.0, 0.2); }
        }
        if let Some(v) = map.get("говорить-по-кнопке") { s.ptt = *v == "да"; }
        if let Some(v) = map.get("клавиша") {
            if !v.is_empty() { s.ptt_key = v.to_string(); }
        }
        s
    }

    pub fn save(&self) {
        let Some(path) = crate::identity::config_file("settings") else { return };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let yes = |b: bool| if b { "да" } else { "нет" };
        let body = format!(
            "имя = {}\n\
             вид = {}\n\
             микрофон = {}\n\
             динамики = {}\n\
             эхоподавление = {}\n\
             шумоподавление = {}\n\
             только-голос = {}\n\
             чувствительность = {:.3}\n\
             порог-тишины = {:.3}\n\
             говорить-по-кнопке = {}\n\
             клавиша = {}\n",
            self.nickname,
            crate::ui::PALETTES[self.theme.min(crate::ui::PALETTES.len() - 1)].name,
            self.input.clone().unwrap_or_default(),
            self.output.clone().unwrap_or_default(),
            yes(self.aec),
            yes(self.denoise),
            yes(self.gate),
            self.gate_sensitivity,
            self.gate_floor,
            yes(self.ptt),
            self.ptt_key,
        );
        let _ = fs::write(path, body);
    }

    pub fn devices(&self) -> DevicePrefs {
        DevicePrefs {
            input: self.input.clone(),
            output: self.output.clone(),
        }
    }

    /// Расставляет сохранённое по ручкам звукового тракта. Вызывается сразу
    /// после запуска движка: ручки живут в нём и создаются заново на каждый
    /// вход в комнату.
    pub fn apply_to(&self, c: &Controls) {
        c.aec.store(self.aec, Ordering::Relaxed);
        c.denoise.store(self.denoise, Ordering::Relaxed);
        c.gate.store(self.gate, Ordering::Relaxed);
        c.gate_sensitivity.store(self.gate_sensitivity.to_bits(), Ordering::Relaxed);
        c.gate_floor.store(self.gate_floor.to_bits(), Ordering::Relaxed);
        c.ptt.store(self.ptt, Ordering::Relaxed);
        *c.ptt_key.lock().unwrap() = self.ptt_key.clone();
    }

    /// Снимает нынешнее положение ручек обратно в настройки.
    pub fn take_from(&mut self, c: &Controls) {
        self.aec = c.aec.load(Ordering::Relaxed);
        self.denoise = c.denoise.load(Ordering::Relaxed);
        self.gate = c.gate.load(Ordering::Relaxed);
        self.gate_sensitivity = f32::from_bits(c.gate_sensitivity.load(Ordering::Relaxed));
        self.gate_floor = f32::from_bits(c.gate_floor.load(Ordering::Relaxed));
        self.ptt = c.ptt.load(Ordering::Relaxed);
        self.ptt_key = c.ptt_key.lock().unwrap().clone();
    }
}

fn default_nickname() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "Игрок".into())
        .chars()
        .take(24)
        .collect()
}
