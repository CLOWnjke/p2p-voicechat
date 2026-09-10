// В релизе не открываем консоль рядом с окном.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod nat;
mod net;
mod ui;

use eframe::egui;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use ui::*;

/// Боковые поля окна.
const GUTTER: i8 = 20;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([460.0, 720.0])
            .with_min_inner_size([400.0, 380.0])
            .with_title("voicechat"),
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
    copied_at: Option<Instant>,
    /// Сглаженное положение полоски и удержание пика. Сырой уровень
    /// дёргается сто раз в секунду, смотреть на него невозможно.
    disp: f32,
    peak: f32,
    last_frame: Instant,
    devices: audio::DevicePrefs,
    /// Списки устройств читаются один раз: опрос звуковой подсистемы не
    /// бесплатный, а делать его каждый кадр отрисовки — верный способ
    /// подвесить окно.
    dev_lists: Option<(Vec<String>, Vec<String>)>,
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
            disp: 0.0,
            peak: 0.0,
            last_frame: Instant::now(),
            devices: audio::DevicePrefs::default(),
            dev_lists: None,
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
                "ОТКРЫВАЕМ ПОРТ"
            } else {
                "ИЩЕМ ХОСТА"
            }
            .into(),
        );
    }

    fn poll_pending(&mut self) {
        let Some(rx) = &self.pending else { return };
        let Ok(result) = rx.try_recv() else { return };
        self.pending = None;

        match result {
            Ok(prepared) => match net::Engine::start(
                prepared,
                self.shared.clone(),
                self.devices.clone(),
            ) {
                Ok(engine) => {
                    self.engine = Some(engine);
                    self.phase = Phase::Active;
                }
                Err(e) => {
                    self.error = Some(format!("Звук не запустился: {e}"));
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
        self.disp = 0.0;
        self.peak = 0.0;
        *self.shared.lock().unwrap() = net::Shared::default();
    }

    /// Мгновенная атака, плавный спад — так ведут себя настоящие индикаторы.
    /// Пик держится отдельно и спадает медленнее, чтобы успеть его заметить.
    fn update_meter(&mut self, level: f32) {
        let dt = self.last_frame.elapsed().as_secs_f32().clamp(0.001, 0.1);
        self.last_frame = Instant::now();

        let target = level_to_pos(level);
        self.disp = if target > self.disp {
            target
        } else {
            let k = 1.0 - (-dt * 9.0).exp();
            self.disp + (target - self.disp) * k
        };
        self.peak = (self.peak - dt * 0.35).max(0.0).max(self.disp);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_pending();
        ui.ctx().request_repaint_after(Duration::from_millis(16));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG))
            .show(ui, |ui| {
                // Шапка и линия под ней — во всю ширину, содержимое — с полями.
                self.header(ui);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(egui::Margin {
                                left: GUTTER,
                                right: GUTTER,
                                top: 0,
                                bottom: 0,
                            })
                            .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add_space(16.0);
                        match &self.phase {
                            Phase::Menu => self.ui_menu(ui),
                            Phase::Connecting(msg) => {
                                let msg = msg.clone();
                                self.ui_connecting(ui, &msg);
                            }
                            Phase::Active => self.ui_active(ui),
                        }
                        if let Some(err) = self.error.clone() {
                            ui.add_space(14.0);
                            ui.horizontal(|ui| {
                                mono(ui, "!", 11.0, DANGER);
                                ui.add_space(6.0);
                                mono(ui, err, 11.0, DANGER);
                            });
                        }
                        ui.add_space(18.0);
                            });
                    });
            });
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        egui::Frame::new()
            .inner_margin(egui::Margin {
                left: GUTTER,
                right: GUTTER,
                top: 0,
                bottom: 0,
            })
            .show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("VOICECHAT")
                    .size(18.0)
                    .strong()
                    .color(TEXT)
                    .monospace(),
            );
            ui.add_space(6.0);
            mono(ui, "v0.1", 9.0, FAINT);

            let live = matches!(self.phase, Phase::Active)
                && self.shared.lock().unwrap().connected;
            let (label, color) = match self.phase {
                Phase::Menu => ("IDLE", DIM),
                Phase::Connecting(_) => ("LINK", DIM),
                Phase::Active if live => ("LIVE", ACCENT),
                Phase::Active => ("WAIT", DIM),
            };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                mono(ui, spaced(label), 9.5, color);
                ui.add_space(6.0);
                let (r, _) = ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
                if color == ACCENT {
                    ui.painter().rect_filled(r, egui::CornerRadius::ZERO, ACCENT);
                } else {
                    ui.painter().rect_stroke(
                        r,
                        egui::CornerRadius::ZERO,
                        egui::Stroke::new(1.0, DIMMER),
                        egui::StrokeKind::Inside,
                    );
                }
            });
        });
            });
        ui.add_space(10.0);
        hairline(ui, LINE);
    }

    fn ui_connecting(&mut self, ui: &mut egui::Ui, msg: &str) {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.add_space(6.0);
            mono(ui, spaced(msg), 11.0, TEXT_2);
        });
        ui.add_space(16.0);
        self.log_section(ui);
    }

    fn ui_menu(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new("ГОЛОС\nБЕЗ СЕРВЕРА")
                .size(29.0)
                .strong()
                .color(TEXT)
                .monospace(),
        );
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(
                "Комната живёт на компьютере хоста. Друзья\nподключаются по коду напрямую — между\nвами нет ничего.",
            )
            .size(11.0)
            .color(DIM)
            .monospace(),
        );

        ui.add_space(30.0);
        micro(ui, "ИМЯ", DIM);
        ui.add_space(6.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.nickname)
                .desired_width(f32::INFINITY)
                .char_limit(24)
                .font(egui::FontId::monospace(13.0)),
        );
        ui.add_space(10.0);
        if button(ui, "СОЗДАТЬ КОМНАТУ", 40.0, None, Some(ACCENT), None, BG, 12.0).clicked() {
            self.begin(true);
        }

        ui.add_space(24.0);
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 40.0) / 2.0;
            let (r1, _) = ui.allocate_exact_size(egui::vec2(w, 1.0), egui::Sense::hover());
            ui.painter().rect_filled(r1, egui::CornerRadius::ZERO, LINE);
            mono(ui, spaced("ИЛИ"), 9.5, FAINT);
            let (r2, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
            ui.painter().rect_filled(r2, egui::CornerRadius::ZERO, LINE);
        });

        ui.add_space(20.0);
        micro(ui, "КОД ПРИГЛАШЕНИЯ", DIM);
        ui.add_space(6.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.code_input)
                .desired_width(f32::INFINITY)
                .hint_text("вставьте код от друга")
                .font(egui::FontId::monospace(12.0)),
        );
        ui.add_space(10.0);
        if button(
            ui,
            "ПОДКЛЮЧИТЬСЯ",
            36.0,
            None,
            None,
            Some(DIMMER),
            TEXT,
            11.5,
        )
        .clicked()
        {
            self.begin(false);
        }

        ui.add_space(26.0);
        self.devices_section(ui);
        ui.add_space(16.0);
        hairline(ui, LINE_DIM);
        ui.add_space(10.0);
        footer(
            ui,
            [
                ("БЕЗ СЕРВЕРА", "Звук идёт напрямую между участниками. Никакой сервер в разговоре не участвует и ничего не хранит."),
                ("ЗВУК 48 кГц", "Кодек Opus, 32 кбит/с на человека — примерно как одна музыкальная дорожка невысокого качества."),
                ("ШУМОДАВ", "Эхоподавление и нейросетевое шумоподавление считаются прямо на вашем компьютере."),
            ],
        );
    }

    fn ui_active(&mut self, ui: &mut egui::Ui) {
        let (invite, upnp, peers, status, is_host, my_id, voice_seen, muted_peers) = {
            let s = self.shared.lock().unwrap();
            (
                s.invite.clone(),
                s.upnp_note.clone(),
                s.peers.clone(),
                s.status.clone(),
                s.is_host,
                s.my_id,
                s.voice_seen.clone(),
                s.muted_peers.keys().copied().collect::<Vec<_>>(),
            )
        };

        // --- код приглашения ---
        if let Some(code) = invite {
            ui.horizontal(|ui| {
                micro(
                    ui,
                    if is_host {
                        "КОД ПРИГЛАШЕНИЯ"
                    } else {
                        "ВАШ КОД"
                    },
                    DIM,
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let n = net::decode_invite(&code).map(|v| v.len()).unwrap_or(0);
                    let (r, resp) =
                        ui.allocate_exact_size(egui::vec2(78.0, 12.0), egui::Sense::hover());
                    ui.painter().text(
                        r.right_center(),
                        egui::Align2::RIGHT_CENTER,
                        spaced(&format!("АДРЕСОВ · {n}")),
                        egui::FontId::monospace(9.5),
                        FAINT,
                    );
                    resp.on_hover_text(
                        "В коде несколько адресов: внешний, локальный и петлевой. Приложение стучится во все сразу и остаётся на том, который ответит.",
                    );
                });
            });
            ui.add_space(6.0);

            let frame = egui::Frame::new()
                .fill(PANEL)
                .stroke(egui::Stroke::new(1.0, LINE))
                .corner_radius(egui::CornerRadius::ZERO)
                .inner_margin(egui::Margin::symmetric(11, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new(&code)
                            .size(10.5)
                            .color(TEXT_2)
                            .monospace(),
                    );
                });
            // Засечки рисуем по настоящему прямоугольнику рамки: считать его
            // по курсору нельзя, туда попадают межэлементные отступы.
            corner_ticks(ui, frame.response.rect, ACCENT);

            ui.add_space(8.0);
            let just = self
                .copied_at
                .map(|t| t.elapsed() < Duration::from_secs(2))
                .unwrap_or(false);
            if button(
                ui,
                if just { "СКОПИРОВАНО" } else { "КОПИРОВАТЬ КОД" },
                32.0,
                None,
                None,
                Some(if just { ACCENT } else { DIMMER }),
                if just { ACCENT } else { TEXT },
                10.5,
            )
            .clicked()
            {
                ui.ctx().copy_text(code.clone());
                self.copied_at = Some(Instant::now());
            }

            if let Some(note) = &upnp {
                let ok = note.contains("пробросил порт:");
                ui.add_space(9.0);
                let (r, resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 12.0),
                    egui::Sense::hover(),
                );
                ui.painter().text(
                    r.left_center(),
                    egui::Align2::LEFT_CENTER,
                    spaced(if ok {
                        "РОУТЕР ОТКРЫЛ ПОРТ"
                    } else {
                        "РОУТЕР НЕ ОТКРЫЛ ПОРТ"
                    }),
                    egui::FontId::monospace(9.5),
                    if ok { ACCENT } else { DIM },
                );
                resp.on_hover_text(if ok {
                    "Приложение попросило роутер пропустить входящие пакеты, и он согласился. Друзья должны подключиться по коду с первого раза."
                } else {
                    "Роутер не пропускает входящие пакеты сам — либо в нём выключен UPnP, либо он его не умеет.\n\nЭто не поломка: если друг не сможет подключиться, откройте раздел «Не соединяется» и обменяйтесь кодами."
                });
            }

            ui.add_space(20.0);
        }

        if !is_host && !status.is_empty() {
            mono(ui, spaced(&status.to_uppercase()), 10.5, TEXT_2);
            ui.add_space(14.0);
        }

        // --- участники ---
        // Про себя знаем из своего же детектора речи. Выключенный микрофон
        // не говорит: детектор считается всегда, но показывать его в этот
        // момент — враньё.
        let i_speak = self
            .engine
            .as_ref()
            .map(|e| {
                audio::level_value(&e.controls.voice) > 0.55
                    && !e.controls.muted.load(Ordering::Relaxed)
            })
            .unwrap_or(false);

        ui.horizontal(|ui| {
            micro(ui, "КОМНАТА", DIM);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                mono(ui, spaced(&format!("{} / 8", peers.len())), 9.5, FAINT);
            });
        });
        ui.add_space(7.0);
        hairline(ui, LINE);
        if peers.is_empty() {
            ui.add_space(9.0);
            mono(ui, "пока никого", 11.5, DIM);
        }
        let i_mute = self
            .engine
            .as_ref()
            .map(|e| e.controls.muted.load(Ordering::Relaxed))
            .unwrap_or(false);

        for (id, name) in peers.iter() {
            let me = *id == my_id;
            let muted = if me {
                i_mute
            } else {
                muted_peers.contains(id)
            };
            // Про остальных — по флагу речи, который приходит в звуковом пакете.
            let active = if me {
                i_speak
            } else {
                voice_seen
                    .get(id)
                    .map(|t| t.elapsed() < Duration::from_millis(350))
                    .unwrap_or(false)
            };

            ui.add_space(7.0);
            ui.horizontal(|ui| {
                bars(ui, active, muted);
                ui.add_space(6.0);
                let name_color = if muted {
                    DIM
                } else if me {
                    TEXT
                } else {
                    TEXT_2
                };
                mono(ui, name.clone(), 12.5, name_color);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    mono(ui, format!("#{id}"), 10.0, FAINT);
                    ui.add_space(8.0);
                    if me {
                        mono(ui, spaced("ВЫ"), 9.5, ACCENT);
                    } else if let Some(engine) = &self.engine {
                        // Громкость собеседника прямо в строке, как канальный
                        // фейдер на пульте: двойной щелчок возвращает единицу.
                        let mut v = engine
                            .volumes
                            .lock()
                            .unwrap()
                            .get(id)
                            .copied()
                            .unwrap_or(1.0);
                        if mini_fader(ui, &mut v, 62.0) {
                            engine.volumes.lock().unwrap().insert(*id, v);
                        }
                    }
                });
            });
            ui.add_space(7.0);
            hairline(ui, LINE_DIM);
        }

        ui.add_space(20.0);
        self.mic_section(ui);
        ui.add_space(20.0);

        self.chain_section(ui);
        self.punch_section(ui);
        self.log_section(ui);
        hairline(ui, LINE);

        ui.add_space(18.0);
        if button(
            ui,
            "ВЫЙТИ ИЗ КОМНАТЫ",
            32.0,
            None,
            None,
            Some(DANGER_LINE),
            DANGER,
            10.5,
        )
        .clicked()
        {
            self.leave();
            return;
        }

        ui.add_space(14.0);
        let total = self
            .engine
            .as_ref()
            .map(|e| {
                20 + if e.controls.denoise.load(Ordering::Relaxed) { 10 } else { 0 }
                    + if e.controls.gate.load(Ordering::Relaxed) { 40 } else { 0 }
            })
            .unwrap_or(20);
        footer(
            ui,
            [
                ("БЕЗ СЕРВЕРА", "Звук идёт напрямую между участниками. Никакой сервер в разговоре не участвует и ничего не хранит."),
                ("ЗВУК 48 кГц", "Кодек Opus, 32 кбит/с на человека — примерно как одна музыкальная дорожка невысокого качества."),
                (&format!("ЗАДЕРЖКА +{total} МС"), "Столько добавляет обработка звука на вашей стороне. Сверху к этому прибавляется дорога по сети."),
            ],
        );
    }

    fn mic_section(&mut self, ui: &mut egui::Ui) {
        let Some(engine) = &self.engine else { return };
        let controls = engine.controls.clone();
        let level = audio::level_value(&controls.level);
        let muted = controls.muted.load(Ordering::Relaxed);
        let input_name = self.shared.lock().unwrap().input_name.clone();
        let shown = if muted { 0.0 } else { level };
        self.update_meter(shown);

        ui.horizontal(|ui| {
            micro(ui, "ВХОД", DIM);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let short: String = input_name.chars().take(28).collect();
                mono(ui, short.to_uppercase(), 9.0, FAINT);
            });
        });
        ui.add_space(8.0);
        meter(ui, self.disp, self.peak);
        ui.add_space(4.0);
        meter_scale(ui);
        ui.add_space(10.0);

        if muted {
            if button(ui, "ВКЛЮЧИТЬ МИКРОФОН", 36.0, None, None, Some(DIMMER), TEXT, 11.0)
                .clicked()
            {
                controls.muted.store(false, Ordering::Relaxed);
            }
        } else if button(ui, "ВЫКЛЮЧИТЬ МИКРОФОН", 36.0, None, Some(ACCENT), None, BG, 11.0)
            .clicked()
        {
            controls.muted.store(true, Ordering::Relaxed);
        }
    }

    fn devices_section(&mut self, ui: &mut egui::Ui) {
        let picked = self
            .devices
            .input
            .clone()
            .unwrap_or_else(|| "СИСТЕМНОЕ".into());
        let short: String = picked.chars().take(18).collect();
        let mut state = section(
            ui,
            "devices",
            "УСТРОЙСТВА",
            Some((&short.to_uppercase(), FAINT)),
        );

        state.show_body_unindented(ui, |ui| {
            if self.dev_lists.is_none() {
                self.dev_lists = Some(audio::list_devices());
            }
            let (ins, outs) = self.dev_lists.clone().unwrap_or_default();

            ui.add_space(10.0);
            ui.label(
                egui::RichText::new("Меняется только до входа в комнату.")
                    .size(10.0)
                    .color(DIM)
                    .monospace(),
            );
            ui.add_space(12.0);

            micro(ui, "МИКРОФОН", DIM);
            ui.add_space(5.0);
            device_list(ui, &ins, &mut self.devices.input, "in");
            ui.add_space(14.0);

            micro(ui, "ВЫВОД", DIM);
            ui.add_space(5.0);
            device_list(ui, &outs, &mut self.devices.output, "out");

            ui.add_space(10.0);
            if button(ui, "ОБНОВИТЬ СПИСОК", 26.0, None, None, Some(LINE), DIM, 9.5).clicked() {
                self.dev_lists = Some(audio::list_devices());
            }
            ui.add_space(12.0);
        });
    }

    fn chain_section(&mut self, ui: &mut egui::Ui) {
        let Some(engine) = &self.engine else { return };
        let c = engine.controls.clone();

        let aec0 = c.aec.load(Ordering::Relaxed);
        let dn0 = c.denoise.load(Ordering::Relaxed);
        let gt0 = c.gate.load(Ordering::Relaxed);
        let on = [aec0, dn0, gt0].iter().filter(|x| **x).count();
        let tag = format!("{on} / 3");

        let mut state = section(
            ui,
            "chain",
            "ОБРАБОТКА ЗВУКА",
            Some((&tag, if on > 0 { ACCENT } else { FAINT })),
        );

        state.show_body_unindented(ui, |ui| {
            let (mut aec, mut denoise, mut gate) = (aec0, dn0, gt0);

            ui.add_space(12.0);
            chain(ui, aec, denoise, gate);
            ui.add_space(16.0);
            hairline(ui, LINE);
            ui.add_space(10.0);

            if toggle_row(
                ui,
                &mut aec,
                "ЭХОПОДАВЛЕНИЕ",
                "DECIBRI-AEC",
                "Вычитает из микрофона то, что звучит\nв динамиках. Можно без наушников.",
            ) {
                c.aec.store(aec, Ordering::Relaxed);
            }
            ui.add_space(10.0);
            hairline(ui, LINE_DIM);
            ui.add_space(10.0);

            if toggle_row(
                ui,
                &mut denoise,
                "ШУМОПОДАВЛЕНИЕ",
                if c.dfn_ready.load(Ordering::Relaxed) {
                    "DEEPFILTERNET 3"
                } else {
                    "ЗАГРУЗКА…"
                },
                "Нейросеть предсказывает усиление для\nкаждой полосы на кадре в 10 мс.",
            ) {
                c.denoise.store(denoise, Ordering::Relaxed);
            }
            ui.add_space(10.0);
            hairline(ui, LINE_DIM);
            ui.add_space(10.0);

            if toggle_row(
                ui,
                &mut gate,
                "ТОЛЬКО ГОЛОС",
                "VAD GATE",
                "Глушит хлопки и стук: решает по\nвероятности речи, а не по громкости.",
            ) {
                c.gate.store(gate, Ordering::Relaxed);
            }

            if gate {
                ui.add_space(18.0);
                let mut sens = audio::level_value(&c.gate_sensitivity);
                let mut floor = audio::level_value(&c.gate_floor);

                let readout = format!("{sens:.2}");
                slider(
                    ui,
                    &mut sens,
                    0.0..=1.0,
                    "ЧУВСТВИТЕЛЬНОСТЬ",
                    &readout,
                    "СТРОГО",
                    "МЯГКО",
                );
                ui.add_space(16.0);
                let db = if floor <= 1e-6 {
                    "-∞".to_string()
                } else {
                    format!("{:.0} dB", 20.0 * floor.log10())
                };
                slider(ui, &mut floor, 0.0..=0.15, "ПОРОГ ТИШИНЫ", &db, "-60", "-16");

                c.gate_sensitivity.store(sens.to_bits(), Ordering::Relaxed);
                c.gate_floor.store(floor.to_bits(), Ordering::Relaxed);

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    let (r, _) =
                        ui.allocate_exact_size(egui::vec2(1.0, 30.0), egui::Sense::hover());
                    ui.painter().rect_filled(r, egui::CornerRadius::ZERO, DIMMER);
                    ui.add_space(9.0);
                    ui.label(
                        egui::RichText::new(
                            "Пропадает начало фраз — поднимите чувствительность.\nПроходят хлопки — опустите.",
                        )
                        .size(10.0)
                        .color(DIM)
                        .monospace(),
                    );
                });
            }
            ui.add_space(16.0);
        });
    }

    fn punch_section(&mut self, ui: &mut egui::Ui) {
        let mut state = section(ui, "trouble", "НЕ СОЕДИНЯЕТСЯ", None);
        state.show_body_unindented(ui, |ui| {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(
                    "Попросите код у собеседника и вставьте сюда —\nначнём стучаться навстречу, и роутеры откроют\nпуть с обеих сторон.",
                )
                .size(10.5)
                .color(DIM)
                .monospace(),
            );
            ui.add_space(10.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.punch_input)
                    .desired_width(f32::INFINITY)
                    .hint_text("код собеседника")
                    .font(egui::FontId::monospace(11.0)),
            );
            ui.add_space(8.0);
            if button(ui, "ПРОБИТЬ", 30.0, None, None, Some(DIMMER), TEXT, 10.5).clicked() {
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
            ui.add_space(16.0);
        });
    }

    fn log_section(&mut self, ui: &mut egui::Ui) {
        let lines = self.shared.lock().unwrap().log.clone();
        let tag = lines.len().to_string();
        let mut state = section(ui, "log", "ЖУРНАЛ", Some((&tag, FAINT)));
        state.show_body_unindented(ui, |ui| {
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .max_height(170.0)
                .stick_to_bottom(true)
                .id_salt("log")
                .show(ui, |ui| {
                    for line in &lines {
                        ui.label(
                            egui::RichText::new(line)
                                .size(10.0)
                                .color(DIM)
                                .monospace(),
                        );
                        ui.add_space(2.0);
                    }
                });
            ui.add_space(14.0);
        });
    }
}

/// Три подписи по нижнему краю. У каждой пояснение по наведению —
/// без него это просто набор жаргона.
fn footer(ui: &mut egui::Ui, items: [(&str, &str); 3]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let w = ui.available_width() / 3.0;
        for (i, (text, tip)) in items.iter().enumerate() {
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(w, 12.0), egui::Sense::hover());
            let (anchor, pos) = match i {
                0 => (egui::Align2::LEFT_CENTER, rect.left_center()),
                1 => (egui::Align2::CENTER_CENTER, rect.center()),
                _ => (egui::Align2::RIGHT_CENTER, rect.right_center()),
            };
            ui.painter().text(
                pos,
                anchor,
                spaced(text),
                egui::FontId::monospace(8.5),
                if resp.hovered() { DIM } else { FAINT },
            );
            resp.on_hover_text(*tip);
        }
    });
}

/// Список устройств: первая строка — системное по умолчанию.
fn device_list(ui: &mut egui::Ui, items: &[String], picked: &mut Option<String>, salt: &str) {
    let mut row = |ui: &mut egui::Ui, label: &str, selected: bool| -> bool {
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 22.0),
            egui::Sense::click(),
        );
        let hovered = resp.hovered();
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let mark = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.center().y - 4.0),
            egui::vec2(8.0, 8.0),
        );
        if selected {
            ui.painter().rect_filled(mark, egui::CornerRadius::ZERO, ACCENT);
        } else {
            ui.painter().rect_stroke(
                mark,
                egui::CornerRadius::ZERO,
                egui::Stroke::new(1.0, if hovered { DIMMER } else { LINE }),
                egui::StrokeKind::Inside,
            );
        }
        ui.painter().text(
            egui::pos2(rect.left() + 16.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::monospace(11.0),
            if selected { TEXT } else { TEXT_2 },
        );
        resp.clicked()
    };

    ui.push_id(salt, |ui| {
        if row(ui, "системное по умолчанию", picked.is_none()) {
            *picked = None;
        }
        for name in items {
            let short: String = name.chars().take(38).collect();
            if row(ui, &short, picked.as_deref() == Some(name.as_str())) {
                *picked = Some(name.clone());
            }
        }
    });
}

/// Три полоски-эквалайзера у имени. Горят, когда человек говорит;
/// перечёркнуты, когда он выключил микрофон.
fn bars(ui: &mut egui::Ui, active: bool, muted: bool) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(14.0, 12.0), egui::Sense::hover());
    let heights = if active && !muted {
        [5.0, 11.0, 7.0]
    } else {
        [3.0, 3.0, 3.0]
    };
    let color = if muted {
        LINE_DIM
    } else if active {
        ACCENT
    } else {
        LINE
    };
    for (i, h) in heights.iter().enumerate() {
        let x = rect.left() + i as f32 * 5.0;
        let r = egui::Rect::from_min_size(
            egui::pos2(x, rect.bottom() - h),
            egui::vec2(3.0, *h),
        );
        ui.painter().rect_filled(r, egui::CornerRadius::ZERO, color);
    }
    if muted {
        ui.painter().line_segment(
            [
                egui::pos2(rect.left() - 1.0, rect.bottom() + 1.0),
                egui::pos2(rect.right() + 1.0, rect.top() - 1.0),
            ],
            egui::Stroke::new(1.0, DIM),
        );
        resp.on_hover_text("Микрофон выключен");
    }
}

fn setup_theme(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 6.0);
        s.spacing.window_margin = egui::Margin::same(0);

        // Приборная панель: моноширинный шрифт везде и ни одного скругления.
        for (_, font) in s.text_styles.iter_mut() {
            font.family = egui::FontFamily::Monospace;
        }

        let v = &mut s.visuals;
        v.panel_fill = BG;
        v.window_fill = BG;
        v.extreme_bg_color = PANEL;
        v.faint_bg_color = PANEL;
        v.override_text_color = Some(TEXT);
        v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        v.selection.stroke = egui::Stroke::new(1.0, ACCENT);
        v.hyperlink_color = ACCENT;
        v.window_stroke = egui::Stroke::new(1.0, LINE);

        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = egui::CornerRadius::ZERO;
            w.bg_fill = PANEL;
            w.weak_bg_fill = PANEL;
            w.bg_stroke = egui::Stroke::new(1.0, LINE);
            w.fg_stroke = egui::Stroke::new(1.0, TEXT);
            w.expansion = 0.0;
        }
        v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, DIMMER);
        v.widgets.active.bg_stroke = egui::Stroke::new(1.0, ACCENT);
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
