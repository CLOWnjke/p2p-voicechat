// В релизе не открываем консоль рядом с окном.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod nat;
mod net;

use eframe::egui;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Акцент интерфейса. Спокойный бирюзовый: он читается и на тёмном фоне,
/// и рядом с красным «выйти», не превращая окно в светофор.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(64, 178, 160);
const ACCENT_DIM: egui::Color32 = egui::Color32::from_rgb(44, 122, 110);
const DANGER: egui::Color32 = egui::Color32::from_rgb(206, 96, 96);
const SURFACE: egui::Color32 = egui::Color32::from_rgb(32, 37, 40);

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([460.0, 720.0])
            .with_min_inner_size([380.0, 320.0])
            .with_title("Голосовой чат"),
        ..Default::default()
    };
    eframe::run_native(
        "voicechat",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

enum Phase {
    Menu,
    Connecting(String),
    Active,
}

struct App {
    shared: Arc<Mutex<net::Shared>>,
    phase: Phase,
    nickname: String,
    code_input: String,
    punch_input: String,
    error: Option<String>,
    engine: Option<net::Engine>,
    pending: Option<Receiver<Result<net::Prepared, String>>>,
    copied_at: Option<std::time::Instant>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_theme(&cc.egui_ctx);
        Self {
            shared: Arc::new(Mutex::new(net::Shared::default())),
            phase: Phase::Menu,
            nickname: default_nickname(),
            code_input: String::new(),
            punch_input: String::new(),
            error: None,
            engine: None,
            pending: None,
            copied_at: None,
        }
    }

    fn begin(&mut self, host: bool) {
        let nickname = self.nickname.trim().to_string();
        if nickname.is_empty() {
            self.error = Some("Введите имя".into());
            return;
        }
        if !host && self.code_input.trim().is_empty() {
            self.error = Some("Вставьте код приглашения".into());
            return;
        }

        self.error = None;
        *self.shared.lock().unwrap() = net::Shared::default();

        let (tx, rx) = channel();
        let shared = self.shared.clone();
        let code = self.code_input.trim().to_string();

        std::thread::spawn(move || {
            let result = if host {
                net::Engine::prepare_host(nickname, shared)
            } else {
                net::Engine::prepare_join(&code, nickname, shared)
            };
            let _ = tx.send(result.map_err(|e| e.to_string()));
        });

        self.pending = Some(rx);
        self.phase = Phase::Connecting(
            if host {
                "Открываем порт и спрашиваем внешний адрес"
            } else {
                "Ищем хоста"
            }
            .into(),
        );
    }

    fn poll_pending(&mut self) {
        let Some(rx) = &self.pending else { return };
        let Ok(result) = rx.try_recv() else { return };
        self.pending = None;

        match result {
            Ok(prepared) => match net::Engine::start(prepared, self.shared.clone()) {
                Ok(engine) => {
                    self.engine = Some(engine);
                    self.phase = Phase::Active;
                }
                Err(e) => {
                    self.error = Some(format!("Не удалось запустить звук: {e}"));
                    self.phase = Phase::Menu;
                }
            },
            Err(e) => {
                self.error = Some(e);
                self.phase = Phase::Menu;
            }
        }
    }

    fn leave(&mut self) {
        self.engine = None; // Drop останавливает потоки и звук
        self.phase = Phase::Menu;
        self.punch_input.clear();
        *self.shared.lock().unwrap() = net::Shared::default();
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_pending();
        // Полоска уровня и список участников должны обновляться сами.
        ui.ctx().request_repaint_after(Duration::from_millis(80));

        egui::CentralPanel::default().show(ui, |ui| {
            // Прокрутка обязательна: в маленьком окне настройки не помещаются,
            // а заставлять человека растягивать окно — плохая идея.
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    self.header(ui);
                    ui.add_space(14.0);

                    match &self.phase {
                        Phase::Menu => self.ui_menu(ui),
                        Phase::Connecting(msg) => {
                            let msg = msg.clone();
                            self.ui_connecting(ui, &msg);
                        }
                        Phase::Active => self.ui_active(ui),
                    }

                    if let Some(err) = self.error.clone() {
                        ui.add_space(12.0);
                        card(ui, DANGER.gamma_multiply(0.18), |ui| {
                            ui.colored_label(DANGER, err);
                        });
                    }
                    ui.add_space(16.0);
                });
        });
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Голосовой чат").size(21.0).strong());
            let connected = self.shared.lock().unwrap().connected;
            if matches!(self.phase, Phase::Active) {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    pill(
                        ui,
                        if connected { "в комнате" } else { "соединяемся" },
                        if connected { ACCENT } else { egui::Color32::GRAY },
                    );
                });
            }
        });
        ui.label(
            egui::RichText::new("комната живёт на компьютере хоста, сервера нет")
                .size(11.5)
                .color(egui::Color32::from_gray(120)),
        );
    }

    fn ui_connecting(&mut self, ui: &mut egui::Ui, msg: &str) {
        card(ui, SURFACE, |ui| {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.add_space(4.0);
                ui.label(msg);
            });
        });
        ui.add_space(10.0);
        self.log_section(ui);
    }

    fn ui_menu(&mut self, ui: &mut egui::Ui) {
        card(ui, SURFACE, |ui| {
            ui.label(egui::RichText::new("Ваше имя").size(12.5).weak());
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.nickname)
                    .desired_width(f32::INFINITY)
                    .char_limit(24),
            );
            ui.add_space(12.0);
            if primary_button(ui, "Создать комнату").clicked() {
                self.begin(true);
            }
        });

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("или присоединиться").size(11.5).weak());
        });
        ui.add_space(6.0);

        card(ui, SURFACE, |ui| {
            ui.label(egui::RichText::new("Код приглашения").size(12.5).weak());
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.code_input)
                    .desired_width(f32::INFINITY)
                    .hint_text("вставьте код от друга"),
            );
            ui.add_space(10.0);
            if ui
                .add_sized(
                    [ui.available_width(), 34.0],
                    egui::Button::new("Подключиться"),
                )
                .clicked()
            {
                self.begin(false);
            }
        });
    }

    fn ui_active(&mut self, ui: &mut egui::Ui) {
        let (invite, upnp, peers, status, is_host) = {
            let s = self.shared.lock().unwrap();
            (
                s.invite.clone(),
                s.upnp_note.clone(),
                s.peers.clone(),
                s.status.clone(),
                s.is_host,
            )
        };

        // --- код приглашения ---
        if let Some(code) = invite {
            card(ui, SURFACE, |ui| {
                ui.label(
                    egui::RichText::new(if is_host {
                        "Ваш код — отправьте друзьям"
                    } else {
                        "Ваш код — отправьте хосту, если не соединяется"
                    })
                    .size(12.5)
                    .weak(),
                );
                ui.add_space(5.0);
                ui.add(
                    egui::TextEdit::multiline(&mut code.clone())
                        .desired_width(f32::INFINITY)
                        .desired_rows(2)
                        .font(egui::TextStyle::Monospace),
                );
                ui.add_space(8.0);
                let just_copied = self
                    .copied_at
                    .map(|t| t.elapsed() < Duration::from_secs(2))
                    .unwrap_or(false);
                if ui
                    .add_sized(
                        [ui.available_width(), 32.0],
                        egui::Button::new(if just_copied {
                            "Скопировано"
                        } else {
                            "Скопировать код"
                        }),
                    )
                    .clicked()
                {
                    ui.ctx().copy_text(code);
                    self.copied_at = Some(std::time::Instant::now());
                }
                if let Some(note) = upnp {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(note).size(11.0).weak());
                }
            });
            ui.add_space(10.0);
        }

        if !is_host && !status.is_empty() {
            ui.label(egui::RichText::new(&status).size(12.5).weak());
            ui.add_space(8.0);
        }

        // --- участники ---
        let speaking = self
            .engine
            .as_ref()
            .map(|e| audio::level_value(&e.controls.voice) > 0.6)
            .unwrap_or(false);

        card(ui, SURFACE, |ui| {
            ui.label(
                egui::RichText::new(format!("В комнате · {}", peers.len()))
                    .size(12.5)
                    .weak(),
            );
            ui.add_space(6.0);
            if peers.is_empty() {
                ui.label(egui::RichText::new("пока никого").weak());
            }
            for (i, (id, name)) in peers.iter().enumerate() {
                // Свой номер знает только хост; для гостя первый в списке — хост.
                let me = is_host && i == 0;
                ui.horizontal(|ui| {
                    dot(ui, if me && speaking { ACCENT } else { egui::Color32::from_gray(80) });
                    ui.add_space(2.0);
                    ui.label(name);
                    if me {
                        ui.label(egui::RichText::new("вы").size(10.5).color(ACCENT_DIM));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(format!("#{id}")).size(10.5).weak());
                    });
                });
            }
        });
        ui.add_space(10.0);

        // --- микрофон ---
        self.mic_section(ui);
        ui.add_space(10.0);

        // --- настройки звука ---
        self.audio_settings(ui);
        ui.add_space(6.0);

        // --- пробивание ---
        self.punch_section(ui);
        ui.add_space(6.0);

        self.log_section(ui);
        ui.add_space(12.0);

        if ui
            .add_sized(
                [ui.available_width(), 30.0],
                egui::Button::new(egui::RichText::new("Выйти из комнаты").color(DANGER)),
            )
            .clicked()
        {
            self.leave();
        }
    }

    fn mic_section(&mut self, ui: &mut egui::Ui) {
        let Some(engine) = &self.engine else { return };
        let level = audio::level_value(&engine.controls.level);
        let muted = engine.controls.muted.load(Ordering::Relaxed);
        let input_name = self.shared.lock().unwrap().input_name.clone();

        card(ui, SURFACE, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Микрофон").size(12.5).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(input_name).size(10.5).weak());
                });
            });
            ui.add_space(6.0);
            ui.add(
                egui::ProgressBar::new(if muted { 0.0 } else { level.min(1.0) })
                    .desired_height(8.0)
                    .corner_radius(4)
                    .fill(if muted { egui::Color32::from_gray(70) } else { ACCENT }),
            );
            ui.add_space(10.0);
            let label = if muted {
                "Включить микрофон"
            } else {
                "Выключить микрофон"
            };
            if ui
                .add_sized([ui.available_width(), 34.0], egui::Button::new(label))
                .clicked()
            {
                engine.controls.muted.store(!muted, Ordering::Relaxed);
            }
        });
    }

    fn audio_settings(&mut self, ui: &mut egui::Ui) {
        let Some(engine) = &self.engine else { return };
        let c = &engine.controls;

        egui::CollapsingHeader::new("Обработка звука")
            .default_open(false)
            .show(ui, |ui| {
                let mut aec = c.aec.load(Ordering::Relaxed);
                let mut denoise = c.denoise.load(Ordering::Relaxed);
                let mut gate = c.gate.load(Ordering::Relaxed);

                if ui
                    .checkbox(&mut aec, "Эхоподавление — можно без наушников")
                    .changed()
                {
                    c.aec.store(aec, Ordering::Relaxed);
                }
                if ui
                    .checkbox(&mut denoise, "Шумоподавление (DeepFilterNet 3)")
                    .changed()
                {
                    c.denoise.store(denoise, Ordering::Relaxed);
                }
                if !c.dfn_ready.load(Ordering::Relaxed) {
                    ui.label(
                        egui::RichText::new("модель загружается, пока работает RNNoise")
                            .size(11.0)
                            .weak(),
                    );
                }
                if ui
                    .checkbox(&mut gate, "Только голос — глушить хлопки и стук")
                    .changed()
                {
                    c.gate.store(gate, Ordering::Relaxed);
                }

                if gate {
                    ui.add_space(4.0);
                    let mut sens = audio::level_value(&c.gate_sensitivity);
                    let mut floor = audio::level_value(&c.gate_floor);
                    ui.add(
                        egui::Slider::new(&mut sens, 0.0..=1.0)
                            .text("чувствительность")
                            .show_value(false),
                    );
                    ui.add(
                        egui::Slider::new(&mut floor, 0.0..=0.15)
                            .text("порог тишины")
                            .show_value(false),
                    );
                    c.gate_sensitivity.store(sens.to_bits(), Ordering::Relaxed);
                    c.gate_floor.store(floor.to_bits(), Ordering::Relaxed);
                    ui.label(
                        egui::RichText::new(
                            "пропадает начало фраз — поднимите чувствительность; \
                             проходят хлопки — опустите",
                        )
                        .size(11.0)
                        .weak(),
                    );
                }
            });
    }

    fn punch_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Не соединяется?")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Попросите код у собеседника и вставьте сюда — начнём \
                         стучаться навстречу, и роутеры откроют путь.",
                    )
                    .size(11.5)
                    .weak(),
                );
                ui.add_space(6.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.punch_input)
                        .desired_width(f32::INFINITY)
                        .hint_text("код собеседника"),
                );
                ui.add_space(6.0);
                let go = ui
                    .add_sized([ui.available_width(), 30.0], egui::Button::new("Пробить"))
                    .clicked();
                if go {
                    let code = self.punch_input.trim().to_string();
                    if let Some(engine) = &self.engine {
                        match engine.add_punch_targets(&code) {
                            Ok(_) => {
                                self.punch_input.clear();
                                self.error = None;
                            }
                            Err(e) => self.error = Some(e.to_string()),
                        }
                    }
                }
            });
    }

    fn log_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Журнал")
            .default_open(false)
            .show(ui, |ui| {
                let lines = self.shared.lock().unwrap().log.clone();
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .stick_to_bottom(true)
                    .id_salt("log")
                    .show(ui, |ui| {
                        for line in lines {
                            ui.label(egui::RichText::new(line).size(11.0).monospace());
                        }
                    });
            });
    }
}

// --- мелкие строительные блоки ---

fn card(ui: &mut egui::Ui, fill: egui::Color32, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(fill)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), 38.0],
        egui::Button::new(egui::RichText::new(text).strong().color(egui::Color32::WHITE))
            .fill(ACCENT_DIM)
            .corner_radius(7),
    )
}

fn pill(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.22))
        .corner_radius(9)
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(11.0).color(color));
        });
}

fn dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn setup_theme(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 7.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.slider_width = 180.0;
        s.visuals.panel_fill = egui::Color32::from_rgb(24, 27, 29);
        s.visuals.window_fill = egui::Color32::from_rgb(24, 27, 29);
        s.visuals.selection.bg_fill = ACCENT_DIM;
        s.visuals.hyperlink_color = ACCENT;
        for w in [
            &mut s.visuals.widgets.inactive,
            &mut s.visuals.widgets.hovered,
            &mut s.visuals.widgets.active,
            &mut s.visuals.widgets.noninteractive,
            &mut s.visuals.widgets.open,
        ] {
            w.corner_radius = egui::CornerRadius::same(6);
        }
    });
}

fn default_nickname() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "Игрок".into())
        .chars()
        .take(24)
        .collect()
}
