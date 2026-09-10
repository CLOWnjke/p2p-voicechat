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
    chain_open: bool,
    trouble_open: bool,
    log_open: bool,
    /// Удержание пика для индикатора: без него метка дёргается и не читается.
    peak: f32,
    last_frame: Instant,
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
            chain_open: false,
            trouble_open: false,
            log_open: false,
            peak: 0.0,
            last_frame: Instant::now(),
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
            Ok(prepared) => match net::Engine::start(prepared, self.shared.clone()) {
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
        self.peak = 0.0;
        *self.shared.lock().unwrap() = net::Shared::default();
    }

    /// Пик спадает примерно за полторы секунды — успеваешь увидеть, но
    /// метка не залипает.
    fn update_peak(&mut self, level: f32) {
        let dt = self.last_frame.elapsed().as_secs_f32().min(0.2);
        self.last_frame = Instant::now();
        self.peak = (self.peak - dt * 0.55).max(0.0).max(level);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_pending();
        ui.ctx().request_repaint_after(Duration::from_millis(60));

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

        ui.add_space(28.0);
        hairline(ui, LINE_DIM);
        ui.add_space(10.0);
        footer(ui, "P2P · NO SERVER", "OPUS 48K", "AEC · DFN3");
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
                    let n = code.len() / 30 + 1;
                    mono(ui, spaced(&format!("{n} ADDR")), 9.5, FAINT);
                });
            });
            ui.add_space(6.0);

            let start = ui.cursor().min;
            egui::Frame::new()
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
            let frame_rect = egui::Rect::from_min_max(start, ui.cursor().min);
            corner_ticks(ui, frame_rect.shrink(0.5), ACCENT);

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let just = self
                    .copied_at
                    .map(|t| t.elapsed() < Duration::from_secs(2))
                    .unwrap_or(false);
                let bw = ui.available_width() * 0.52;
                if button(
                    ui,
                    if just { "СКОПИРОВАНО" } else { "КОПИРОВАТЬ" },
                    32.0,
                    Some(bw),
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
                ui.add_space(8.0);
                if let Some(note) = &upnp {
                    let ok = note.contains("пробросил порт:");
                    let (r, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 32.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_stroke(
                        r,
                        egui::CornerRadius::ZERO,
                        egui::Stroke::new(1.0, LINE),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter().text(
                        r.center(),
                        egui::Align2::CENTER_CENTER,
                        spaced(if ok { "UPNP · OK" } else { "UPNP · FAILED" }),
                        egui::FontId::monospace(10.0),
                        if ok { ACCENT } else { DIM },
                    );
                }
            });
            ui.add_space(20.0);
        }

        if !is_host && !status.is_empty() {
            mono(ui, spaced(&status.to_uppercase()), 10.5, TEXT_2);
            ui.add_space(14.0);
        }

        // --- участники ---
        let speaking = self
            .engine
            .as_ref()
            .map(|e| audio::level_value(&e.controls.voice) > 0.55)
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
        for (i, (id, name)) in peers.iter().enumerate() {
            // Свой номер известен только хосту; у гостя первый в списке — хост.
            let me = is_host && i == 0;
            ui.add_space(7.0);
            ui.horizontal(|ui| {
                bars(ui, me && speaking);
                ui.add_space(6.0);
                mono(ui, name.clone(), 12.5, if me { TEXT } else { TEXT_2 });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    mono(ui, format!("#{id}"), 10.0, FAINT);
                    if me {
                        ui.add_space(8.0);
                        mono(ui, spaced("ВЫ"), 9.5, ACCENT);
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
        footer(ui, "P2P · NO SERVER", "OPUS 48K", &format!("+{total} MS"));
    }

    fn mic_section(&mut self, ui: &mut egui::Ui) {
        let Some(engine) = &self.engine else { return };
        let controls = engine.controls.clone();
        let level = audio::level_value(&controls.level);
        let muted = controls.muted.load(Ordering::Relaxed);
        let input_name = self.shared.lock().unwrap().input_name.clone();
        let shown = if muted { 0.0 } else { level };
        self.update_peak(shown);

        ui.horizontal(|ui| {
            micro(ui, "ВХОД", DIM);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let short: String = input_name.chars().take(28).collect();
                mono(ui, short.to_uppercase(), 9.0, FAINT);
            });
        });
        ui.add_space(8.0);
        meter(ui, shown, self.peak);
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

    fn chain_section(&mut self, ui: &mut egui::Ui) {
        let Some(engine) = &self.engine else { return };
        let c = engine.controls.clone();

        let mut aec = c.aec.load(Ordering::Relaxed);
        let mut denoise = c.denoise.load(Ordering::Relaxed);
        let mut gate = c.gate.load(Ordering::Relaxed);

        let on_count = [aec, denoise, gate].iter().filter(|x| **x).count();
        let tag = format!("{on_count} / 3");
        section(
            ui,
            &mut self.chain_open,
            "ОБРАБОТКА ЗВУКА",
            Some((&tag, if on_count > 0 { ACCENT } else { FAINT })),
        );

        if !self.chain_open {
            return;
        }

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

            let sens_readout = format!("{sens:.2}");
            slider(
                ui,
                &mut sens,
                0.0..=1.0,
                "ЧУВСТВИТЕЛЬНОСТЬ",
                &sens_readout,
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
                let (r, _) = ui.allocate_exact_size(egui::vec2(1.0, 30.0), egui::Sense::hover());
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
    }

    fn punch_section(&mut self, ui: &mut egui::Ui) {
        section(ui, &mut self.trouble_open, "НЕ СОЕДИНЯЕТСЯ", None);
        if !self.trouble_open {
            return;
        }
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
    }

    fn log_section(&mut self, ui: &mut egui::Ui) {
        let lines = self.shared.lock().unwrap().log.clone();
        let tag = lines.len().to_string();
        section(ui, &mut self.log_open, "ЖУРНАЛ", Some((&tag, FAINT)));
        if !self.log_open {
            return;
        }
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .max_height(170.0)
            .stick_to_bottom(true)
            .id_salt("log")
            .show(ui, |ui| {
                for line in lines {
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
    }
}

/// Три подписи по нижнему краю: слева, по центру, справа.
fn footer(ui: &mut egui::Ui, left: &str, mid: &str, right: &str) {
    ui.horizontal(|ui| {
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 11.0), egui::Sense::hover());
        let f = egui::FontId::monospace(8.5);
        ui.painter().text(
            rect.left_center(),
            egui::Align2::LEFT_CENTER,
            spaced(left),
            f.clone(),
            FAINT,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            spaced(mid),
            f.clone(),
            FAINT,
        );
        ui.painter().text(
            rect.right_center(),
            egui::Align2::RIGHT_CENTER,
            spaced(right),
            f,
            FAINT,
        );
    });
}

/// Три полоски-эквалайзера у имени: горят, когда человек говорит.
fn bars(ui: &mut egui::Ui, active: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 12.0), egui::Sense::hover());
    let heights = if active { [5.0, 11.0, 7.0] } else { [3.0, 3.0, 3.0] };
    let color = if active { ACCENT } else { LINE };
    for (i, h) in heights.iter().enumerate() {
        let x = rect.left() + i as f32 * 5.0;
        let r = egui::Rect::from_min_size(
            egui::pos2(x, rect.bottom() - h),
            egui::vec2(3.0, *h),
        );
        ui.painter().rect_filled(r, egui::CornerRadius::ZERO, color);
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
