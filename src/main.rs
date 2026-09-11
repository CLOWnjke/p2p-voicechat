// В релизе не открываем консоль рядом с окном.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod identity;
mod nat;
mod net;
mod settings;
mod tray;
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
            .with_title("voicechat")
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "voicechat",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

/// С чего начинаем встречу.
#[derive(Clone, Copy, PartialEq)]
enum Start {
    Host,
    Join,
    /// Возвращение в запомненную комнату: стучимся ко всем, кого помним.
    Return,
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
    chat_input: String,
    /// Сколько сообщений уже видели: разница с нынешним числом и есть
    /// счётчик непрочитанного.
    chat_seen: usize,
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
    /// Своя пара ключей: имя — это просто строка, а ключ подделать нельзя.
    identity: Arc<identity::Identity>,
    /// Имена из запомненной комнаты — для кнопки возвращения.
    last_room: Vec<String>,
    /// Прошлый снимок счётчиков нагрузки и время, когда он сделан.
    /// По разности получается доля ядра, без усреднений и догадок.
    load_mark: Option<(Instant, [u64; 6])>,
    /// Что показываем: доли ядра по этапам и число кадров окна в секунду.
    load_shown: [f32; 6],
    /// То же самое, но замеренное, пока окно было не в фокусе. Смотреть на
    /// цифры во время игры невозможно — окно закрыто игрой, — поэтому
    /// показания за игровое время запоминаются отдельно и ждут, пока на них
    /// посмотрят.
    load_game: [f32; 6],
    load_game_at: Option<Instant>,
    /// Значок в углу экрана. Пока он есть, крестик прячет окно, а закрыть
    /// приложение можно только из его меню.
    tray: Option<tray::Tray>,
    /// Окно спрятано в значок.
    hidden: bool,
    /// Выходим по-настоящему: закрытие больше не перехватываем.
    quitting: bool,
    /// Когда вошли в комнату (или начали в неё стучаться). По этому
    /// на экране ожидания считается, сколько мы уже ждём.
    active_since: Option<Instant>,
    /// Раскрыта ли помощь «друг не может подключиться?».
    show_punch: bool,
    /// Показывать ли сам код приглашения. По умолчанию нет: человеку
    /// нужно его отправить, а не прочитать.
    show_code: bool,
    /// Настройки, которые переживают закрытие приложения.
    settings: settings::Settings,
    /// Когда последний раз писали их на диск. Ползунок за секунду даёт
    /// сотню изменений, и писать файл на каждое было бы дикостью.
    saved_at: Instant,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = settings::Settings::load();
        // Вид выставляем до первой отрисовки: иначе окно моргнёт чужими
        // цветами на первом кадре.
        ui::set_theme(settings.theme);
        setup_theme(&cc.egui_ctx);
        Self {
            shared: Arc::new(Mutex::new(net::Shared::default())),
            phase: Phase::Menu,
            nickname: settings.nickname.clone(),
            code_input: String::new(),
            punch_input: String::new(),
            chat_input: String::new(),
            chat_seen: 0,
            error: None,
            engine: None,
            pending: None,
            copied_at: None,
            disp: 0.0,
            peak: 0.0,
            last_frame: Instant::now(),
            devices: settings.devices(),
            dev_lists: None,
            identity: Arc::new(identity::Identity::load_or_create()),
            last_room: net::last_room().into_iter().map(|(_, _, n)| n).collect(),
            load_mark: None,
            load_shown: [0.0; 6],
            load_game: [0.0; 6],
            load_game_at: None,
            tray: {
                let (rgba, w, h) = icon_rgba();
                tray::Tray::new(rgba, w, h)
            },
            hidden: false,
            quitting: false,
            active_since: None,
            show_punch: false,
            show_code: false,
            settings,
            saved_at: Instant::now(),
        }
    }

    /// Сохраняет настройки, если они поменялись. Вызывается каждый кадр:
    /// сравнение дешёвое, а запись случается не чаще раза в две секунды.
    fn keep_settings(&mut self, force: bool) {
        let mut now = self.settings.clone();
        now.nickname = self.nickname.trim().to_string();
        now.theme = ui::theme();
        now.input = self.devices.input.clone();
        now.output = self.devices.output.clone();
        if let Some(e) = &self.engine {
            now.take_from(&e.controls);
        }
        if now == self.settings {
            return;
        }
        self.settings = now;
        if force || self.saved_at.elapsed() > Duration::from_secs(2) {
            self.settings.save();
            self.saved_at = Instant::now();
        }
    }

    /// Что запускаем: свою комнату, вход по коду или возвращение в ту,
    /// где мы уже были.
    fn begin(&mut self, host: bool) {
        self.begin_mode(if host { Start::Host } else { Start::Join });
    }

    fn begin_mode(&mut self, mode: Start) {
        let nickname = self.nickname.trim().to_string();
        if nickname.is_empty() {
            self.error = Some("Введите имя".into());
            return;
        }
        if matches!(mode, Start::Join) && self.code_input.trim().is_empty() {
            self.error = Some("Вставьте код приглашения".into());
            return;
        }

        self.error = None;
        *self.shared.lock().unwrap() = net::Shared::default();

        let (tx, rx) = channel();
        let shared = self.shared.clone();
        let code = self.code_input.trim().to_string();
        let id = self.identity.clone();

        std::thread::spawn(move || {
            let result = match mode {
                Start::Host => net::Engine::prepare_host(nickname, shared, id),
                Start::Join => net::Engine::prepare_join(&code, nickname, shared, id),
                Start::Return => net::Engine::prepare_return(nickname, shared, id),
            };
            let _ = tx.send(result.map_err(|e| e.to_string()));
        });

        self.pending = Some(rx);
        self.phase = Phase::Connecting(
            match mode {
                Start::Host => "ОТКРЫВАЕМ ПОРТ",
                Start::Join => "ИЩЕМ ХОСТА",
                Start::Return => "СТУЧИМСЯ КО ВСЕМ",
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
                    // Ручки живут в движке и создаются заново на каждый вход,
                    // поэтому сохранённое расставляем здесь.
                    self.settings.apply_to(&engine.controls);
                    self.engine = Some(engine);
                    self.phase = Phase::Active;
                    self.active_since = Some(Instant::now());
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
        self.keep_settings(true);
        self.engine = None; // Drop останавливает потоки и звук
        self.phase = Phase::Menu;
        self.last_room = net::last_room().into_iter().map(|(_, _, n)| n).collect();
        self.punch_input.clear();
        self.chat_input.clear();
        self.show_code = false;
        self.show_punch = false;
        self.active_since = None;
        self.chat_seen = 0;
        self.disp = 0.0;
        self.peak = 0.0;
        *self.shared.lock().unwrap() = net::Shared::default();
    }

    /// Значок в углу экрана: разбираем нажатия и перехватываем крестик.
    fn poll_tray(&mut self, ctx: &egui::Context) {
        let Some(tray) = &self.tray else { return };
        let cmds = tray.poll();
        for cmd in cmds {
            match cmd {
                tray::Cmd::Show => self.unhide(ctx),
                tray::Cmd::ToggleMute => {
                    if let Some(e) = &self.engine {
                        let m = &e.controls.muted;
                        m.store(!m.load(Ordering::Relaxed), Ordering::Relaxed);
                    }
                }
                tray::Cmd::Quit => {
                    self.quitting = true;
                    self.unhide(ctx);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            // Крестик всегда прячет окно, а не закрывает приложение.
            // Разговор при этом не прерывается ничем: звук и сеть живут в
            // своих потоках и окна не касаются. Выйти совсем — только из
            // меню значка: единственная дверь наружу, зато явная.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.hidden = true;
        }
    }

    fn unhide(&mut self, ctx: &egui::Context) {
        if self.hidden {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            self.hidden = false;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Куда уходит процессорное время. Показываем в долях одного ядра:
    /// проценты «всего процессора» на восьмиядерной машине ничего не
    /// говорят, а «полядра» — говорит.
    fn load_section(&mut self, ui: &mut egui::Ui) {
        let Some(load) = self.engine.as_ref().map(|e| e.controls.load.clone()) else {
            return;
        };
        let now = [
            load.dsp_ns.load(Ordering::Relaxed),
            load.enc_ns.load(Ordering::Relaxed),
            load.rx_ns.load(Ordering::Relaxed),
            load.ui_ns.load(Ordering::Relaxed),
            load.ui_frames.load(Ordering::Relaxed),
            load.sent.load(Ordering::Relaxed) + load.recv.load(Ordering::Relaxed),
        ];
        match self.load_mark {
            Some((at, was)) if at.elapsed() >= Duration::from_millis(1000) => {
                let dt = at.elapsed().as_secs_f32();
                for i in 0..4 {
                    // Наносекунды работы за секунду времени — это и есть
                    // доля ядра.
                    self.load_shown[i] = (now[i].saturating_sub(was[i])) as f32 / 1e9 / dt;
                }
                self.load_shown[4] = (now[4].saturating_sub(was[4])) as f32 / dt;
                self.load_shown[5] = (now[5].saturating_sub(was[5])) as f32 / dt;
                self.load_mark = Some((Instant::now(), now));
                if !ui.ctx().input(|i| i.focused) {
                    self.load_game = self.load_shown;
                    self.load_game_at = Some(Instant::now());
                }
            }
            None => self.load_mark = Some((Instant::now(), now)),
            _ => {}
        }

        // Итог показываем по игровому замеру, если он есть: именно он
        // отвечает на вопрос, во что приложение обходится во время игры.
        let total: f32 = self.load_shown[..4].iter().sum();
        let head: f32 = if self.load_game_at.is_some() {
            self.load_game[..4].iter().sum()
        } else {
            total
        };
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            micro(ui, "НАГРУЗКА", DIM());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                mono(
                    ui,
                    spaced(&format!("{:.0}% ЯДРА", head * 100.0)),
                    9.5,
                    if head > 0.35 { DANGER() } else { FAINT() },
                )
            });
        });
        ui.add_space(6.0);
        hairline(ui, LINE_DIM());
        ui.add_space(7.0);
        ui.horizontal(|ui| {
            mono(ui, "", 10.0, DIM());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (r, resp) =
                    ui.allocate_exact_size(egui::vec2(62.0, 11.0), egui::Sense::hover());
                ui.painter().text(
                    r.right_center(),
                    egui::Align2::RIGHT_CENTER,
                    spaced("СЕЙЧАС"),
                    egui::FontId::monospace(9.0),
                    FAINT(),
                );
                resp.on_hover_text("Замер прямо сейчас, когда окно перед вами.");
                let (r, resp) =
                    ui.allocate_exact_size(egui::vec2(62.0, 11.0), egui::Sense::hover());
                ui.painter().text(
                    r.right_center(),
                    egui::Align2::RIGHT_CENTER,
                    spaced("В ИГРЕ"),
                    egui::FontId::monospace(9.0),
                    ACCENT(),
                );
                resp.on_hover_text(
                    "Последний замер, сделанный пока окно было не в фокусе — то есть \
                     пока вы играли. Ради него всё и затевалось: смотреть на цифры \
                     во время игры невозможно, поэтому они дожидаются вас здесь.",
                );
            });
        });

        let pct = |v: f32| format!("{:.1}%", v * 100.0);
        let rows: [(&str, [String; 2], &str); 5] = [
            (
                "обработка микрофона",
                [pct(self.load_shown[0]), pct(self.load_game[0])],
                "Эхоподавитель, нейросетевой шумодав и ворота. Считается всё время, пока вы в комнате, независимо от того, говорите вы или молчите.",
            ),
            (
                "упаковка и отправка",
                [pct(self.load_shown[1]), pct(self.load_game[1])],
                "Кодирование Opus и отправка пакетов. Речь кодировать дороже, чем тишину, — эта строка растёт, когда вы говорите.",
            ),
            (
                "приём и разбор",
                [pct(self.load_shown[2]), pct(self.load_game[2])],
                "Разбор пришедших пакетов, декодирование и микширование. Растёт, когда говорят вам.",
            ),
            (
                "отрисовка окна",
                [pct(self.load_shown[3]), pct(self.load_game[3])],
                "Самое дорогое, что может делать приложение во время игры: каждый нарисованный кадр выводится на экран и мешает игре держать монопольный полноэкранный режим. Свёрнутое или перекрытое окно должно давать здесь около нуля.",
            ),
            (
                "кадров окна в секунду",
                [
                    format!("{:.0}", self.load_shown[4]),
                    format!("{:.0}", self.load_game[4]),
                ],
                "Сколько раз в секунду окно перерисовывается. В фокусе — около шестидесяти, за игрой должно упасть до четырёх, свёрнутым — до одного.",
            ),
        ];
        for (name, values, hint) in rows {
            ui.add_space(7.0);
            ui.horizontal(|ui| {
                mono(ui, name, 11.0, TEXT_2());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (i, v) in values.iter().enumerate() {
                        let (r, _) =
                            ui.allocate_exact_size(egui::vec2(62.0, 13.0), egui::Sense::hover());
                        ui.painter().text(
                            r.right_center(),
                            egui::Align2::RIGHT_CENTER,
                            v,
                            egui::FontId::monospace(11.0),
                            if i == 0 { TEXT() } else { ACCENT() },
                        );
                    }
                });
            })
            .response
            .on_hover_text(hint);
        }
        ui.add_space(7.0);
        ui.horizontal(|ui| {
            mono(ui, "пакетов в секунду", 11.0, DIM());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                mono(ui, format!("{:.0}", self.load_shown[5]), 11.0, DIM());
            });
        });
        if let Some(at) = self.load_game_at {
            ui.add_space(6.0);
            mono(
                ui,
                format!("замер в игре сделан {} с назад", at.elapsed().as_secs()),
                10.0,
                FAINT(),
            );
        }
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
        let painting = Instant::now();
        self.poll_pending();
        self.keep_settings(false);
        self.poll_tray(ui.ctx());

        // Пока окно не на переднем плане, перерисовываться шестьдесят раз в
        // секунду незачем: человек в это время играет. Каждый наш кадр — это
        // полноценный кадр OpenGL с выводом на экран, а окно, которое
        // непрерывно выводит кадры, вынуждает игру уйти из монопольного
        // полноэкранного режима в композитный. Отсюда и берутся потерянные
        // кадры — не из наших вычислений, они ничтожны, а из того, что мы
        // всё время лезем на экран.
        //
        // Смотреть на индикатор во время игры всё равно некому, поэтому
        // фоном обновляемся четыре раза в секунду, а свёрнутыми — раз в
        // секунду, только чтобы не спать вечным сном.
        let (focused, minimized) = ui.ctx().input(|i| {
            (
                i.focused,
                i.viewport().minimized.unwrap_or(false),
            )
        });
        let period = if minimized || self.hidden {
            1000
        } else if focused {
            16
        } else {
            250
        };
        ui.ctx().request_repaint_after(Duration::from_millis(period));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG()))
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
                                mono(ui, "!", 11.0, DANGER());
                                ui.add_space(6.0);
                                mono(ui, err, 11.0, DANGER());
                            });
                        }
                        ui.add_space(18.0);
                            });
                    });
            });

        if let Some(load) = self.engine.as_ref().map(|e| e.controls.load.clone()) {
            load.ui_frames.fetch_add(1, Ordering::Relaxed);
            audio::Load::add(&load.ui_ns, painting);
        }
    }

    /// Закрытие окна — тоже выход из комнаты. Без этого Drop у движка мог
    /// не успеть отработать, и человек ещё несколько секунд висел бы
    /// в чужом списке участников.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.keep_settings(true);
        self.engine = None;
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui) {
        // Заголовок нельзя прижимать к системной рамке окна: они сливаются.
        ui.add_space(14.0);
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
                    .color(TEXT())
                    .monospace(),
            );
            ui.add_space(6.0);
            mono(ui, "v0.1", 9.0, FAINT());

            let live = matches!(self.phase, Phase::Active)
                && self.shared.lock().unwrap().connected;
            // По-русски и по-человечески: это подпись для человека,
            // а не отладочный флаг.
            let (label, color) = match self.phase {
                Phase::Menu => ("НЕ В КОМНАТЕ", DIM()),
                Phase::Connecting(_) => ("ПОДКЛЮЧАЕМСЯ", DIM()),
                Phase::Active if live => ("В КОМНАТЕ", ACCENT()),
                Phase::Active => ("ЖДЁМ", DIM()),
            };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                mono(ui, spaced(label), 9.5, color);

                // Непрочитанное видно и тогда, когда секция чата свёрнута
                // или уехала за край прокрутки.
                if matches!(self.phase, Phase::Active) {
                    let unread = self
                        .shared
                        .lock()
                        .unwrap()
                        .chat
                        .len()
                        .saturating_sub(self.chat_seen);
                    if unread > 0 {
                        ui.add_space(10.0);
                        let (r, resp) = ui.allocate_exact_size(
                            egui::vec2(30.0, 12.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().text(
                            r.right_center(),
                            egui::Align2::RIGHT_CENTER,
                            format!("+{unread}"),
                            egui::FontId::monospace(9.5),
                            ACCENT(),
                        );
                        resp.on_hover_text("Новые сообщения в чате");
                    }
                }
                ui.add_space(6.0);
                let (r, _) = ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
                if color == ACCENT() {
                    ui.painter().rect_filled(r, egui::CornerRadius::ZERO, ACCENT());
                } else {
                    ui.painter().rect_stroke(
                        r,
                        egui::CornerRadius::ZERO,
                        egui::Stroke::new(1.0, DIMMER()),
                        egui::StrokeKind::Inside,
                    );
                }
            });
        });
            });
        ui.add_space(12.0);
        hairline(ui, LINE());
    }

    fn ui_connecting(&mut self, ui: &mut egui::Ui, msg: &str) {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.add_space(6.0);
            mono(ui, spaced(msg), 11.0, TEXT_2());
        });
        ui.add_space(16.0);
        self.log_section(ui);
    }

    /// Что видит гость, пока не вошёл в комнату.
    ///
    /// Две половины одной истории. Пока идёт обычное ожидание — видно,
    /// какой шаг сейчас выполняется. Когда хост не ответил — та самая
    /// инструкция, за которой раньше надо было догадаться полезть в
    /// свёрнутый раздел «Не соединяется». Теперь она и есть экран.
    fn ui_waiting(
        &mut self,
        ui: &mut egui::Ui,
        status: &str,
        invite: Option<String>,
        upnp: Option<String>,
    ) {
        let stuck = status.contains("не отвечает");
        let waited = self
            .active_since
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);

        if !stuck {
            ui.label(
                egui::RichText::new("Ищем комнату")
                    .size(20.0)
                    .color(TEXT())
                    .monospace(),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(
                    "Обычно занимает две-три секунды.\nЕсли дольше — подскажем, что делать.",
                )
                .size(11.5)
                .color(TEXT_2())
                .monospace(),
            );

            ui.add_space(24.0);
            hairline(ui, LINE());
            ui.add_space(12.0);
            step_row(ui, Step::Done, "Открыли свой порт", "и попросили роутер пропускать входящие", "готово");
            ui.add_space(12.0);
            hairline(ui, LINE_DIM());
            ui.add_space(12.0);
            step_row(
                ui,
                if invite.is_some() { Step::Done } else { Step::Now },
                "Узнали свой адрес снаружи",
                match &upnp {
                    Some(n) if n.contains("пробросил порт:") => "роутер открыл порт сам",
                    Some(_) => "роутер порт не открыл — обычно это не мешает",
                    None => "спрашиваем у публичного сервера",
                },
                if invite.is_some() { "готово" } else { "" },
            );
            ui.add_space(12.0);
            hairline(ui, LINE_DIM());
            ui.add_space(12.0);
            step_row(ui, Step::Now, "Стучимся к хосту", "пробуем все адреса из кода сразу", &format!("{waited} с"));
            ui.add_space(12.0);
            hairline(ui, LINE_DIM());
            ui.add_space(12.0);
            step_row(ui, Step::Wait, "Здороваемся и входим", "сверяем ключи и занимаем место", "");
            ui.add_space(12.0);
            hairline(ui, LINE());

            ui.add_space(22.0);
            if button(ui, "ОТМЕНИТЬ", 40.0, None, None, Some(DIMMER()), TEXT_2(), 11.0).clicked() {
                self.leave();
                return;
            }
            ui.add_space(18.0);
            self.log_section(ui);
            return;
        }

        // --- хост не ответил ---
        ui.label(
            egui::RichText::new("Хост не отвечает")
                .size(20.0)
                .color(TEXT())
                .monospace(),
        );
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(
                "Так бывает почти всегда, и это не поломка.\nРоутер друга пропускает к нему только тех,\nкому он писал сам. Значит, надо постучаться\nнавстречу с обеих сторон — это три шага\nи полминуты.",
            )
            .size(11.5)
            .color(TEXT_2())
            .monospace(),
        );

        ui.add_space(22.0);
        hairline(ui, LINE());
        ui.add_space(16.0);

        numbered(ui, 1, true, "Скопируйте свой код", "он у вас уже есть — приложение сделало его при запуске");
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(26.0);
            let just = self
                .copied_at
                .map(|t| t.elapsed() < Duration::from_secs(2))
                .unwrap_or(false);
            if button(
                ui,
                if just { "СКОПИРОВАНО" } else { "СКОПИРОВАТЬ МОЙ КОД" },
                40.0,
                None,
                Some(ACCENT()),
                None,
                ON_ACCENT(),
                11.0,
            )
            .clicked()
            {
                if let Some(code) = &invite {
                    ui.ctx().copy_text(code.clone());
                    self.copied_at = Some(Instant::now());
                }
            }
        });

        ui.add_space(18.0);
        hairline(ui, LINE_DIM());
        ui.add_space(16.0);
        numbered(ui, 2, false, "Отправьте его другу", "туда же, откуда взяли его код: в чат игры,\nв мессенджер, куда угодно");

        ui.add_space(18.0);
        hairline(ui, LINE_DIM());
        ui.add_space(16.0);
        numbered(ui, 3, false, "Пусть он вставит его у себя", "в своём окне он нажмёт «друг не может\nподключиться?» и вставит ваш код");

        ui.add_space(18.0);
        hairline(ui, LINE());
        ui.add_space(18.0);

        // Обещание, без которого инструкция неполна: человек должен знать,
        // что дальше от него ничего не требуется.
        let frame = egui::Frame::new()
            .fill(ACCENT_BG())
            .stroke(egui::Stroke::new(1.0, ACCENT()))
            .corner_radius(egui::CornerRadius::ZERO)
            .inner_margin(egui::Margin::symmetric(14, 13))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    egui::RichText::new(
                        "Ждём его. Как только он вставит код, вы\nсоединитесь сами — нажимать больше\nничего не надо.",
                    )
                    .size(11.0)
                    .color(TEXT_2())
                    .monospace(),
                );
            });
        let _ = frame;

        ui.add_space(20.0);
        // Обратный путь: если друг прислал свой код, вставить его можно и
        // отсюда — постучимся навстречу с обеих сторон сразу.
        self.show_punch = true;
        self.help_row(ui);
        ui.add_space(18.0);
        if button(ui, "ВЕРНУТЬСЯ НАЗАД", 38.0, None, None, Some(DIMMER()), TEXT_2(), 10.5).clicked() {
            self.leave();
            return;
        }
        ui.add_space(18.0);
        self.log_section(ui);
    }

    fn ui_menu(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new("ГОЛОС\nБЕЗ СЕРВЕРА")
                .size(29.0)
                .strong()
                .color(TEXT())
                .monospace(),
        );
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(
                "Комната живёт на компьютере хоста. Друзья\nподключаются по коду напрямую — между\nвами нет ничего.",
            )
            .size(11.0)
            .color(DIM())
            .monospace(),
        );

        // Возвращение показываем прежде всего остального: если человек
        // только что вылетел, это единственное, чего он хочет.
        if !self.last_room.is_empty() {
            ui.add_space(26.0);
            micro(ui, "ВЫ УЖЕ БЫЛИ ЗДЕСЬ", DIM());
            ui.add_space(6.0);
            let who: Vec<&str> = self
                .last_room
                .iter()
                .map(|n| n.as_str())
                .filter(|n| !n.is_empty())
                .collect();
            mono(ui, who.join(", "), 11.5, TEXT_2());
            ui.add_space(10.0);
            if button(
                ui,
                "ВЕРНУТЬСЯ В КОМНАТУ",
                40.0,
                None,
                None,
                Some(ACCENT()),
                ACCENT(),
                11.5,
            )
            .on_hover_text(
                "Постучимся сразу ко всем, кого помним по прошлой встрече. \
                 Кто на месте — тот и ответит, а если хостом стал другой, \
                 он на него покажет. Код спрашивать не надо.",
            )
            .clicked()
            {
                self.begin_mode(Start::Return);
            }
        }

        ui.add_space(30.0);
        micro(ui, "ИМЯ", DIM());
        ui.add_space(6.0);
        field(ui, &mut self.nickname, "как вас зовут", 14.0, 40.0, Some(24));
        ui.add_space(10.0);
        if button(ui, "СОЗДАТЬ КОМНАТУ", 40.0, None, Some(ACCENT()), None, ON_ACCENT(), 12.0).clicked() {
            self.begin(true);
        }

        ui.add_space(24.0);
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 40.0) / 2.0;
            let (r1, _) = ui.allocate_exact_size(egui::vec2(w, 1.0), egui::Sense::hover());
            ui.painter().rect_filled(r1, egui::CornerRadius::ZERO, LINE());
            mono(ui, spaced("ИЛИ"), 9.5, FAINT());
            let (r2, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
            ui.painter().rect_filled(r2, egui::CornerRadius::ZERO, LINE());
        });

        ui.add_space(20.0);
        micro(ui, "КОД ПРИГЛАШЕНИЯ", DIM());
        ui.add_space(6.0);
        field(
            ui,
            &mut self.code_input,
            "вставьте код от друга",
            13.0,
            40.0,
            None,
        );
        ui.add_space(10.0);
        if button(
            ui,
            "ПОДКЛЮЧИТЬСЯ",
            40.0,
            None,
            None,
            Some(DIMMER()),
            TEXT(),
            11.5,
        )
        .clicked()
        {
            self.begin(false);
        }

        ui.add_space(28.0);
        hairline(ui, LINE());
        ui.add_space(16.0);
        micro(ui, "ВИД", DIM());
        ui.add_space(10.0);
        if theme_picker(ui) {
            // Цвета egui берутся из палитры один раз, при настройке стиля,
            // поэтому после смены вида её надо провести заново.
            setup_theme(ui.ctx());
            self.keep_settings(true);
        }

        ui.add_space(22.0);
        self.devices_section(ui);
        ui.add_space(16.0);
        ui.horizontal_top(|ui| {
            // Галочку рисуем сами: в моноширинном шрифте egui её нет,
            // и вместо знака выходит пустой прямоугольник.
            let (r, _) = ui.allocate_exact_size(egui::vec2(12.0, 14.0), egui::Sense::hover());
            let p = ui.painter();
            let st = egui::Stroke::new(1.3, DIM());
            let c = egui::pos2(r.left() + 5.0, r.top() + 7.0);
            p.line_segment([c + egui::vec2(-4.0, 0.0), c + egui::vec2(-1.0, 3.0)], st);
            p.line_segment([c + egui::vec2(-1.0, 3.0), c + egui::vec2(4.5, -3.5)], st);
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(
                    "имя, вид, устройства и все настройки звука\nсохраняются — заново настраивать не придётся",
                )
                .size(10.0)
                .color(DIM())
                .monospace(),
            );
        });

        ui.add_space(16.0);
        hairline(ui, LINE_DIM());
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
        let (
            invite,
            upnp,
            peers,
            status,
            is_host,
            my_id,
            voice_seen,
            peer_seen,
            muted_peers,
            prints,
            trust,
        ) = {
            let s = self.shared.lock().unwrap();
            (
                s.invite.clone(),
                s.upnp_note.clone(),
                s.peers.clone(),
                s.status.clone(),
                s.is_host,
                s.my_id,
                s.voice_seen.clone(),
                s.peer_seen.clone(),
                s.muted_peers.keys().copied().collect::<Vec<_>>(),
                s.fingerprints.clone(),
                s.trust.clone(),
            )
        };

        // Пока гость не вошёл, показывать ему комнату нечестно: комнаты
        // ещё нет. Вместо неё — что происходит и что делать.
        let connected = self.shared.lock().unwrap().connected;
        if !is_host && !connected {
            self.ui_waiting(ui, &status, invite.clone(), upnp.clone());
            return;
        }

        // --- пригласить ---
        //
        // Кода здесь больше нет. Человеку незачем видеть строку в двести
        // знаков: ему нужно её отправить, а не прочитать. Поэтому на виду
        // только действие, а сам код — за ссылкой, для любопытных.
        if let Some(code) = invite {
            micro(ui, if is_host { "ПРИГЛАСИТЬ" } else { "ВАШ КОД" }, DIM());
            ui.add_space(10.0);

            let just = self
                .copied_at
                .map(|t| t.elapsed() < Duration::from_secs(2))
                .unwrap_or(false);
            if button(
                ui,
                if just { "СКОПИРОВАНО" } else { "СКОПИРОВАТЬ КОД ПРИГЛАШЕНИЯ" },
                42.0,
                None,
                Some(ACCENT()),
                None,
                ON_ACCENT(),
                11.5,
            )
            .clicked()
            {
                ui.ctx().copy_text(code.clone());
                self.copied_at = Some(Instant::now());
            }

            ui.add_space(9.0);
            ui.horizontal(|ui| {
                mono(ui, "отправьте его другу любым способом", 10.0, DIM());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (r, resp) =
                        ui.allocate_exact_size(egui::vec2(84.0, 13.0), egui::Sense::click());
                    ui.painter().text(
                        r.right_center(),
                        egui::Align2::RIGHT_CENTER,
                        if self.show_code { "скрыть код" } else { "показать код" },
                        egui::FontId::monospace(10.0),
                        if self.show_code { TEXT_2() } else { DIM() },
                    );
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        self.show_code = !self.show_code;
                    }
                });
            });

            if self.show_code {
                ui.add_space(9.0);
                let frame = egui::Frame::new()
                    .fill(PANEL())
                    .stroke(egui::Stroke::new(1.0, LINE()))
                    .corner_radius(egui::CornerRadius::ZERO)
                    .inner_margin(egui::Margin::symmetric(11, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            egui::RichText::new(&code)
                                .size(11.0)
                                .color(TEXT_2())
                                .monospace(),
                        );
                    });
                corner_ticks(ui, frame.response.rect, ACCENT());
                ui.add_space(6.0);
                let n = net::decode_invite(&code).map(|v| v.len()).unwrap_or(0);
                let (r, resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 12.0),
                    egui::Sense::hover(),
                );
                ui.painter().text(
                    r.left_center(),
                    egui::Align2::LEFT_CENTER,
                    spaced(&format!("АДРЕСОВ · {n}")),
                    egui::FontId::monospace(9.5),
                    FAINT(),
                );
                resp.on_hover_text(
                    "В коде несколько адресов: внешний, локальный и петлевой. Приложение стучится во все сразу и остаётся на том, который ответит.",
                );
            }

            ui.add_space(12.0);
            self.help_row(ui);
            ui.add_space(20.0);
        }

        if !is_host && !status.is_empty() {
            mono(ui, spaced(&status.to_uppercase()), 10.5, TEXT_2());
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
            micro(ui, "КОМНАТА", DIM());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                mono(ui, spaced(&format!("{} / 8", peers.len())), 9.5, FAINT());
            });
        });
        ui.add_space(7.0);
        hairline(ui, LINE());
        if peers.is_empty() {
            ui.add_space(9.0);
            mono(ui, "пока никого", 11.5, DIM());
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

            // Состояние приходит каждые полсекунды, так что три секунды
            // тишины — это уже не потеря пакета, а обрыв.
            let stale = !me
                && peer_seen
                    .get(id)
                    .map(|t| t.elapsed() > Duration::from_secs(3))
                    .unwrap_or(false);

            ui.add_space(7.0);
            ui.horizontal(|ui| {
                bars(ui, active && !stale, muted);
                ui.add_space(6.0);
                let name_color = if stale || muted {
                    DIM()
                } else if me {
                    TEXT()
                } else {
                    TEXT_2()
                };
                mono(ui, name.clone(), 12.5, name_color);

                if stale {
                    ui.add_space(6.0);
                    let resp = micro(ui, "НЕТ СВЯЗИ", DANGER());
                    resp.on_hover_text(
                        "От этого человека давно ничего не приходит. Обычно связь \
                         восстанавливается сама за несколько секунд; если нет — \
                         скорее всего у него сменилась сеть.",
                    );
                }

                // Ключ подделать нельзя, а имя можно: если под знакомым именем
                // пришёл другой ключ, об этом надо сказать вслух.
                if trust.get(id) == Some(&net::Trust::Changed) {
                    ui.add_space(2.0);
                    let (r, resp) =
                        ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter().text(
                        r.center(),
                        egui::Align2::CENTER_CENTER,
                        "!",
                        egui::FontId::monospace(12.0),
                        DANGER(),
                    );
                    resp.on_hover_text(
                        "Под этим именем раньше приходил другой ключ. Либо человек переустановил приложение, либо это не он.",
                    );
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !me {
                        // Квадратик показывает, идёт ли звук напрямую или
                        // пересылается хостом.
                        let straight = self
                            .engine
                            .as_ref()
                            .map(|e| e.direct.lock().unwrap().contains_key(id))
                            .unwrap_or(false);
                        let (r, resp) =
                            ui.allocate_exact_size(egui::vec2(5.0, 5.0), egui::Sense::hover());
                        ui.painter().rect_filled(
                            r,
                            egui::CornerRadius::ZERO,
                            if straight { ACCENT() } else { LINE() },
                        );
                        resp.on_hover_text(if straight {
                            "Звук идёт напрямую, минуя хоста."
                        } else {
                            "Звук пока пересылается хостом: прямой путь ещё не пробит."
                        });
                        ui.add_space(6.0);
                    }
                    let (r, resp) =
                        ui.allocate_exact_size(egui::vec2(58.0, 12.0), egui::Sense::hover());
                    ui.painter().text(
                        r.right_center(),
                        egui::Align2::RIGHT_CENTER,
                        prints.get(id).cloned().unwrap_or_else(|| "····".into()),
                        egui::FontId::monospace(9.5),
                        if trust.get(id) == Some(&net::Trust::Changed) {
                            DANGER()
                        } else {
                            FAINT()
                        },
                    );
                    resp.on_hover_text(
                        "Отпечаток ключа. Он у человека один и тот же от встречи к встрече — по нему и опознают, что это правда он.",
                    );
                    ui.add_space(6.0);
                    if me {
                        mono(ui, spaced("ВЫ"), 9.5, ACCENT());
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
            hairline(ui, LINE_DIM());
        }

        ui.add_space(20.0);
        self.mic_section(ui);
        ui.add_space(20.0);

        self.chat_section(ui, &peers);
        self.chain_section(ui);
        self.log_section(ui);
        hairline(ui, LINE());

        ui.add_space(18.0);
        if button(
            ui,
            "ВЫЙТИ ИЗ КОМНАТЫ",
            36.0,
            None,
            None,
            Some(DANGER_LINE()),
            DANGER(),
            10.5,
        )
        .clicked()
        {
            self.leave();
            return;
        }

        if self.tray.is_some() {
            ui.add_space(8.0);
            mono(
                ui,
                "крестик прячет окно в значок у часов, разговор продолжается.\nзакрыть приложение совсем — из меню значка",
                10.0,
                FAINT(),
            );
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

        let _ = input_name;
        let ptt = controls.ptt.load(Ordering::Relaxed);
        let holding = controls.ptt_down.load(Ordering::Relaxed);

        if muted {
            if button(ui, "ВКЛЮЧИТЬ МИКРОФОН", 40.0, None, None, Some(DIMMER()), TEXT(), 11.0)
                .clicked()
            {
                controls.muted.store(false, Ordering::Relaxed);
            }
        } else if ptt {
            // В режиме кнопки главная строка — подсказка, а не переключатель:
            // видно, слышат тебя сейчас или нет.
            let key = controls.ptt_key.lock().unwrap().clone();
            let label = if holding {
                format!("ГОВОРИТЕ · {key}")
            } else {
                format!("ЗАЖМИТЕ {key}")
            };
            if button(
                ui,
                &label,
                40.0,
                None,
                if holding { Some(ACCENT()) } else { None },
                if holding { None } else { Some(DIMMER()) },
                if holding { ON_ACCENT() } else { DIM() },
                11.0,
            )
            .clicked()
            {
                controls.muted.store(true, Ordering::Relaxed);
            }
        } else if button(ui, "ВЫКЛЮЧИТЬ МИКРОФОН", 40.0, None, Some(ACCENT()), None, ON_ACCENT(), 11.0)
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
            Some((&short.to_uppercase(), FAINT())),
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
                    .color(DIM())
                    .monospace(),
            );
            ui.add_space(12.0);

            micro(ui, "МИКРОФОН", DIM());
            ui.add_space(5.0);
            device_list(ui, &ins, &mut self.devices.input, "in");
            ui.add_space(14.0);

            micro(ui, "ВЫВОД", DIM());
            ui.add_space(5.0);
            device_list(ui, &outs, &mut self.devices.output, "out");

            ui.add_space(10.0);
            if button(ui, "ОБНОВИТЬ СПИСОК", 30.0, None, None, Some(LINE()), DIM(), 10.0).clicked() {
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
            Some((&tag, if on > 0 { ACCENT() } else { FAINT() })),
        );

        state.show_body_unindented(ui, |ui| {
            let (mut aec, mut denoise, mut gate) = (aec0, dn0, gt0);

            ui.add_space(12.0);

            // Шкала стоит здесь, а не в комнате: сама по себе она человеку
            // ничего не говорит, а рядом с ручками показывает, что они
            // делают со звуком.
            ui.horizontal(|ui| {
                micro(ui, "ВХОД", DIM());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let name = self.shared.lock().unwrap().input_name.clone();
                    let short: String = name.chars().take(28).collect();
                    mono(ui, short.to_uppercase(), 9.0, FAINT());
                });
            });
            ui.add_space(8.0);
            meter(ui, self.disp, self.peak);
            ui.add_space(4.0);
            meter_scale(ui);
            ui.add_space(6.0);
            mono(ui, "это то, что слышат остальные — уже после обработки", 10.0, DIM());
            ui.add_space(16.0);
            hairline(ui, LINE());
            ui.add_space(12.0);

            chain(ui, aec, denoise, gate);
            self.load_section(ui);
            ui.add_space(16.0);
            hairline(ui, LINE());
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
            hairline(ui, LINE_DIM());
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
            hairline(ui, LINE_DIM());
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

            ui.add_space(10.0);
            hairline(ui, LINE_DIM());
            ui.add_space(10.0);

            let mut ptt = c.ptt.load(Ordering::Relaxed);
            if toggle_row(
                ui,
                &mut ptt,
                "ГОВОРИТЬ ПО КНОПКЕ",
                "PUSH-TO-TALK",
                "Микрофон открыт, только пока зажата\nклавиша. Работает и вне окна.",
            ) {
                c.ptt.store(ptt, Ordering::Relaxed);
            }
            if ptt {
                ui.add_space(8.0);
                let current = c.ptt_key.lock().unwrap().clone();
                let mut pick: Option<String> = None;
                ui.horizontal_wrapped(|ui| {
                    for key in [
                        "F8", "F9", "F10", "CapsLock", "LControl", "LAlt", "LShift", "V", "X",
                    ] {
                        let on = key.eq_ignore_ascii_case(&current);
                        if button(
                            ui,
                            key,
                            24.0,
                            Some(key.len() as f32 * 7.5 + 22.0),
                            if on { Some(ACCENT()) } else { None },
                            if on { None } else { Some(LINE()) },
                            if on { BG() } else { TEXT_2() },
                            9.5,
                        )
                        .clicked()
                        {
                            pick = Some(key.to_string());
                        }
                    }
                });
                if let Some(k) = pick {
                    *c.ptt_key.lock().unwrap() = k;
                }
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
                    ui.painter().rect_filled(r, egui::CornerRadius::ZERO, DIMMER());
                    ui.add_space(9.0);
                    ui.label(
                        egui::RichText::new(
                            "Пропадает начало фраз — поднимите чувствительность.\nПроходят хлопки — опустите.",
                        )
                        .size(10.0)
                        .color(DIM())
                        .monospace(),
                    );
                });
            }
            ui.add_space(16.0);
        });
    }

    fn chat_section(&mut self, ui: &mut egui::Ui, peers: &[(u16, String)]) {
        let (chat, my_id) = {
            let s = self.shared.lock().unwrap();
            (s.chat.clone(), s.my_id)
        };
        let unread = chat.len().saturating_sub(self.chat_seen);
        let tag = if unread > 0 {
            format!("+{unread}")
        } else {
            chat.len().to_string()
        };
        let mut state = section(
            ui,
            "chat",
            "ЧАТ",
            Some((&tag, if unread > 0 { ACCENT() } else { FAINT() })),
        );

        // Пока секция открыта, всё прочитано.
        if state.is_open() {
            self.chat_seen = chat.len();
        }

        state.show_body_unindented(ui, |ui| {
            ui.add_space(8.0);

            // Отпечаток ключа переехал сюда из комнаты: смотреть на него
            // каждый раз незачем, а когда понадобится сверить — он здесь,
            // вместе со всем остальным техническим.
            let (r, resp) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 13.0), egui::Sense::hover());
            ui.painter().text(
                r.left_center(),
                egui::Align2::LEFT_CENTER,
                spaced(&format!("ВАШ КЛЮЧ · {}", self.identity.fingerprint())),
                egui::FontId::monospace(9.5),
                FAINT(),
            );
            resp.on_hover_text(
                "Отпечаток вашего ключа. Он создаётся один раз и хранится на этом компьютере; собеседники узнают вас именно по нему, а не по имени.",
            );
            ui.add_space(8.0);

            egui::ScrollArea::vertical()
                .max_height(300.0)
                // Без этого область ужимается по содержимому, и полоса
                // прокрутки повисает посреди окна.
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .id_salt("chat")
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if chat.is_empty() {
                        mono(ui, "пока пусто", 11.0, FAINT());
                    }
                    for (id, text) in &chat {
                        let name = peers
                            .iter()
                            .find(|(pid, _)| pid == id)
                            .map(|(_, n)| n.clone())
                            .unwrap_or_else(|| format!("#{id}"));
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            let (r, _) = ui.allocate_exact_size(
                                egui::vec2(80.0, 16.0),
                                egui::Sense::hover(),
                            );
                            let short: String = name.chars().take(10).collect();
                            ui.painter().text(
                                r.right_top() + egui::vec2(0.0, 2.0),
                                egui::Align2::RIGHT_TOP,
                                short,
                                egui::FontId::monospace(11.0),
                                if *id == my_id { ACCENT() } else { DIM() },
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(text)
                                        .size(12.0)
                                        .color(TEXT_2())
                                        .monospace(),
                                )
                                .wrap(),
                            );
                        });
                        ui.add_space(5.0);
                    }
                });

            ui.add_space(12.0);
            let resp = field(ui, &mut self.chat_input, "написать…", 12.5, 38.0, Some(400));
            let entered =
                resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(8.0);
            let clicked = button(ui, "ОТПРАВИТЬ", 34.0, None, None, Some(DIMMER()), TEXT(), 10.5)
                .clicked();

            if entered || clicked {
                if let Some(engine) = &self.engine {
                    engine.send_chat(&self.chat_input);
                }
                self.chat_input.clear();
                if entered {
                    resp.request_focus();
                }
            }
            ui.add_space(14.0);
        });
    }

    /// Строка «друг не может подключиться?» рядом с приглашением.
    ///
    /// Помощь должна лежать там, где возникает вопрос. Раньше она была
    /// свёрнутым разделом в самом низу, и догадаться заглянуть туда мог
    /// только тот, кто и так знает, что ищет.
    fn help_row(&mut self, ui: &mut egui::Ui) {
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::click());
        let open = self.show_punch;
        ui.painter().rect_stroke(
            rect,
            egui::CornerRadius::ZERO,
            egui::Stroke::new(1.0, if open || resp.hovered() { DIMMER() } else { LINE() }),
            egui::StrokeKind::Inside,
        );
        // Кружок с вопросительным знаком, нарисованный вручную.
        let c = egui::pos2(rect.left() + 17.0, rect.center().y);
        ui.painter()
            .circle_stroke(c, 6.5, egui::Stroke::new(1.0, if open { ACCENT() } else { DIM() }));
        ui.painter().text(
            c,
            egui::Align2::CENTER_CENTER,
            "?",
            egui::FontId::monospace(9.0),
            if open { ACCENT() } else { DIM() },
        );
        ui.painter().text(
            egui::pos2(rect.left() + 32.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            "друг не может подключиться?",
            egui::FontId::monospace(10.5),
            if open { TEXT() } else { TEXT_2() },
        );
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            self.show_punch = !self.show_punch;
        }

        if !self.show_punch {
            return;
        }

        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(
                "Роутер друга пропускает к нему только тех, кому\nон писал сам. Попросите у него его код, вставьте\nсюда — и мы постучимся навстречу.",
            )
            .size(10.5)
            .color(DIM())
            .monospace(),
        );
        ui.add_space(10.0);
        field(ui, &mut self.punch_input, "код друга", 12.5, 38.0, None);
        ui.add_space(8.0);
        if button(ui, "ПОСТУЧАТЬСЯ НАВСТРЕЧУ", 38.0, None, None, Some(DIMMER()), TEXT(), 11.0)
            .clicked()
        {
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
    }

    fn log_section(&mut self, ui: &mut egui::Ui) {
        let lines = self.shared.lock().unwrap().log.clone();
        let tag = lines.len().to_string();
        let mut state = section(ui, "log", "ЖУРНАЛ", Some((&tag, FAINT())));
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
                                .color(DIM())
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
                if resp.hovered() { DIM() } else { FAINT() },
            );
            resp.on_hover_text(*tip);
        }
    });
}

/// Список устройств: первая строка — системное по умолчанию.
fn device_list(ui: &mut egui::Ui, items: &[String], picked: &mut Option<String>, salt: &str) {
    let row = |ui: &mut egui::Ui, label: &str, selected: bool| -> bool {
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 26.0),
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
            ui.painter().rect_filled(mark, egui::CornerRadius::ZERO, ACCENT());
        } else {
            ui.painter().rect_stroke(
                mark,
                egui::CornerRadius::ZERO,
                egui::Stroke::new(1.0, if hovered { DIMMER() } else { LINE() }),
                egui::StrokeKind::Inside,
            );
        }
        ui.painter().text(
            egui::pos2(rect.left() + 16.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::monospace(12.0),
            if selected { TEXT() } else { TEXT_2() },
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
        LINE_DIM()
    } else if active {
        ACCENT()
    } else {
        LINE()
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
            egui::Stroke::new(1.0, DIM()),
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
        v.panel_fill = BG();
        v.window_fill = BG();
        v.extreme_bg_color = PANEL();
        v.faint_bg_color = PANEL();
        v.override_text_color = Some(TEXT());
        v.selection.bg_fill = ACCENT().gamma_multiply(0.35);
        v.selection.stroke = egui::Stroke::new(1.0, ACCENT());
        v.hyperlink_color = ACCENT();
        v.window_stroke = egui::Stroke::new(1.0, LINE());

        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = egui::CornerRadius::ZERO;
            w.bg_fill = PANEL();
            w.weak_bg_fill = PANEL();
            w.bg_stroke = egui::Stroke::new(1.0, LINE());
            w.fg_stroke = egui::Stroke::new(1.0, TEXT());
            w.expansion = 0.0;
        }
        v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, DIMMER());
        v.widgets.active.bg_stroke = egui::Stroke::new(1.0, ACCENT());
    });
}

/// Иконка окна. Рисуется прямо здесь, теми же тремя полосками, которыми
/// в комнате помечена речь: так не нужен ни файл, ни распаковщик PNG.
/// Файл `assets/icon.ico` — та же картинка для иконки самого exe.
fn app_icon() -> egui::IconData {
    let (rgba, width, height) = icon_rgba();
    egui::IconData {
        rgba,
        width,
        height,
    }
}

/// Картинка приложения: три полоски эквалайзера с угловыми засечками.
/// Рисуется кодом, чтобы не тащить файл и не расходиться с интерфейсом.
fn icon_rgba() -> (Vec<u8>, u32, u32) {
    const S: i32 = 256;
    let mut rgba = vec![0u8; (S * S * 4) as usize];

    let mut put = |x0: i32, y0: i32, w: i32, h: i32, c: [u8; 4]| {
        for y in y0.max(0)..(y0 + h).min(S) {
            for x in x0.max(0)..(x0 + w).min(S) {
                let i = ((y * S + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&c);
            }
        }
    };

    let bg = [10, 11, 12, 255];
    let accent = [255, 149, 0, 255];
    put(0, 0, S, S, bg);

    // Угловые засечки — тот же приём, что и в интерфейсе.
    let (t, l, inset) = (8, 45, 22);
    put(inset, inset, l, t, accent);
    put(inset, inset, t, l, accent);
    put(S - inset - l, S - inset - t, l, t, accent);
    put(S - inset - t, S - inset - l, t, l, accent);

    // Три полоски эквалайзера.
    let (bar_w, gap, base) = (36, 20, 197);
    let x0 = (S - (bar_w * 3 + gap * 2)) / 2;
    for (i, h) in [92, 150, 112].into_iter().enumerate() {
        put(x0 + i as i32 * (bar_w + gap), base - h, bar_w, h, accent);
    }

    (rgba, S as u32, S as u32)
}

