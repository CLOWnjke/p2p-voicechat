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

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([470.0, 600.0])
            .with_min_inner_size([420.0, 460.0])
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
    show_log: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        cc.egui_ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 8.0);
            s.spacing.button_padding = egui::vec2(12.0, 7.0);
        });

        Self {
            shared: Arc::new(Mutex::new(net::Shared::default())),
            phase: Phase::Menu,
            nickname: default_nickname(),
            code_input: String::new(),
            punch_input: String::new(),
            error: None,
            engine: None,
            pending: None,
            show_log: false,
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
                "Открываем порт и спрашиваем внешний адрес…"
            } else {
                "Ищем хоста…"
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
        *self.shared.lock().unwrap() = net::Shared::default();
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_pending();
        // Полоска уровня и список участников должны обновляться сами.
        ui.ctx().request_repaint_after(Duration::from_millis(80));

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(6.0);
            ui.heading("Голосовой чат");
            ui.label(
                egui::RichText::new("хост держит комнату у себя, сервера нет")
                    .small()
                    .weak(),
            );
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(10.0);

            match &self.phase {
                Phase::Menu => self.ui_menu(ui),
                Phase::Connecting(msg) => {
                    let msg = msg.clone();
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(msg);
                    });
                    ui.add_space(8.0);
                    self.ui_log(ui);
                }
                Phase::Active => self.ui_active(ui),
            }

            if let Some(err) = &self.error {
                ui.add_space(10.0);
                ui.colored_label(egui::Color32::from_rgb(230, 110, 110), err);
            }
        });
    }
}

impl App {
    fn ui_menu(&mut self, ui: &mut egui::Ui) {
        ui.label("Ваше имя");
        ui.add(
            egui::TextEdit::singleline(&mut self.nickname)
                .desired_width(f32::INFINITY)
                .char_limit(24),
        );

        ui.add_space(16.0);
        if ui
            .add_sized([ui.available_width(), 38.0], egui::Button::new("Создать комнату"))
            .clicked()
        {
            self.begin(true);
        }

        ui.add_space(20.0);
        ui.separator();
        ui.add_space(14.0);

        ui.label("Код приглашения от друга");
        ui.add(
            egui::TextEdit::singleline(&mut self.code_input)
                .desired_width(f32::INFINITY)
                .hint_text("вставьте сюда"),
        );
        ui.add_space(8.0);
        if ui
            .add_sized([ui.available_width(), 38.0], egui::Button::new("Подключиться"))
            .clicked()
        {
            self.begin(false);
        }
    }

    fn ui_active(&mut self, ui: &mut egui::Ui) {
        let (invite, upnp, peers, status, is_host, input_name) = {
            let s = self.shared.lock().unwrap();
            (
                s.invite.clone(),
                s.upnp_note.clone(),
                s.peers.clone(),
                s.status.clone(),
                s.is_host,
                s.input_name.clone(),
            )
        };

        if let Some(code) = invite {
            ui.label(if is_host {
                "Ваш код — отправьте его друзьям"
            } else {
                "Ваш код — отправьте его хосту"
            });
            ui.add(
                egui::TextEdit::multiline(&mut code.clone())
                    .desired_width(f32::INFINITY)
                    .desired_rows(2)
                    .font(egui::TextStyle::Monospace),
            );
            ui.add_space(6.0);
            if ui.button("Скопировать код").clicked() {
                ui.ctx().copy_text(code);
            }
            if let Some(note) = upnp {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(note).small().weak());
            }
            ui.add_space(12.0);
        }

        if !is_host {
            ui.label(egui::RichText::new(&status).strong());
            ui.add_space(10.0);
        }

        ui.separator();
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("Не соединяется? Вставьте код собеседника — начнём стучаться навстречу")
                .small()
                .weak(),
        );
        let mut do_punch = false;
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 96.0).max(80.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.punch_input)
                    .desired_width(width)
                    .hint_text("код собеседника"),
            );
            do_punch = ui.button("Пробить").clicked();
        });
        if do_punch {
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
        ui.add_space(12.0);

        ui.separator();
        ui.add_space(10.0);

        ui.label(egui::RichText::new("В комнате").small().weak());
        if peers.is_empty() {
            ui.label("пока никого");
        }
        for (id, name) in &peers {
            ui.horizontal(|ui| {
                ui.label("•");
                ui.label(name);
                ui.label(egui::RichText::new(format!("#{id}")).small().weak());
            });
        }

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(10.0);

        if let Some(engine) = &self.engine {
            let level = audio::level_value(&engine.controls.level);
            let voice = audio::level_value(&engine.controls.voice);
            let muted = engine.controls.muted.load(Ordering::Relaxed);
            let mut denoise = engine.controls.denoise.load(Ordering::Relaxed);
            let mut aec = engine.controls.aec.load(Ordering::Relaxed);
            let mut gate = engine.controls.gate.load(Ordering::Relaxed);
            let mut sens = audio::level_value(&engine.controls.gate_sensitivity);
            let mut floor = audio::level_value(&engine.controls.gate_floor);

            ui.label(egui::RichText::new(&input_name).small().weak());
            ui.add(
                egui::ProgressBar::new(if muted { 0.0 } else { level.min(1.0) })
                    .desired_height(10.0)
                    .fill(if muted {
                        egui::Color32::from_gray(70)
                    } else {
                        egui::Color32::from_rgb(70, 170, 150)
                    }),
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

            ui.add_space(8.0);
            if ui
                .checkbox(&mut aec, "Эхоподавление — можно без наушников")
                .changed()
            {
                engine.controls.aec.store(aec, Ordering::Relaxed);
            }
            if ui
                .checkbox(&mut denoise, "Шумоподавление (нейросеть RNNoise)")
                .changed()
            {
                engine.controls.denoise.store(denoise, Ordering::Relaxed);
            }
            if ui
                .checkbox(&mut gate, "Только голос — глушить хлопки и стук")
                .changed()
            {
                engine.controls.gate.store(gate, Ordering::Relaxed);
            }

            if gate {
                ui.add_space(2.0);
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
                engine
                    .controls
                    .gate_sensitivity
                    .store(sens.to_bits(), Ordering::Relaxed);
                engine
                    .controls
                    .gate_floor
                    .store(floor.to_bits(), Ordering::Relaxed);
                ui.label(
                    egui::RichText::new(
                        "меньше чувствительность — строже отбор, но можно потерять тихую речь",
                    )
                    .small()
                    .weak(),
                );
            }
            ui.label(
                egui::RichText::new(if voice > 0.7 {
                    "слышу голос"
                } else if voice > 0.3 {
                    "что-то есть"
                } else {
                    "тихо"
                })
                .small()
                .weak(),
            );
        }

        ui.add_space(8.0);
        if ui
            .add_sized([ui.available_width(), 30.0], egui::Button::new("Выйти"))
            .clicked()
        {
            self.leave();
            return;
        }

        ui.add_space(10.0);
        self.ui_log(ui);
    }

    fn ui_log(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.show_log, "Показать журнал");
        if !self.show_log {
            return;
        }
        let lines = self.shared.lock().unwrap().log.clone();
        egui::ScrollArea::vertical()
            .max_height(150.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in lines {
                    ui.label(egui::RichText::new(line).small().monospace());
                }
            });
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
