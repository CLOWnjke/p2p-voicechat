//! Строительные блоки интерфейса.
//!
//! Эстетика приборной панели: чистый чёрный, волосяные линии, нулевые
//! скругления, моноширинный шрифт и один сигнальный акцент, которым
//! помечено только живое — речь, включённая обработка, активные ручки.
//!
//! Половину элементов приходится рисовать вручную: у egui нет ни разрядки
//! букв, ни сегментных индикаторов, ни ползунков нужного вида.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Rect, Response, Sense, Stroke, StrokeKind, Ui,
};

/// Палитра: все цвета интерфейса в одном месте.
///
/// Раньше это были константы, и вид приложения был решением автора. Теперь
/// это настройка: строение, плотность и шрифт не меняются, меняется только
/// цвет — поэтому переключать вид можно когда угодно и ничего не поедет.
#[derive(Clone, Copy)]
pub struct Palette {
    pub name: &'static str,
    pub bg: Color32,
    pub panel: Color32,
    pub line: Color32,
    pub line_dim: Color32,
    pub text: Color32,
    pub text_2: Color32,
    pub dim: Color32,
    pub dimmer: Color32,
    pub faint: Color32,
    pub accent: Color32,
    /// Цвет надписи поверх акцентной заливки. На тёмных палитрах это фон,
    /// на светлой — белый: чёрные буквы на синем не читаются.
    pub on_accent: Color32,
    pub accent_bg: Color32,
    pub danger: Color32,
    pub danger_line: Color32,
    pub meter_hot: Color32,
}

const fn c(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

pub const PALETTES: [Palette; 4] = [
    // Иней: холодный синий на почти чёрном. Читается как студийный прибор.
    Palette {
        name: "Иней",
        bg: c(5, 7, 10),
        panel: c(10, 14, 19),
        line: c(23, 29, 36),
        line_dim: c(14, 19, 24),
        text: c(230, 236, 242),
        text_2: c(168, 180, 192),
        dim: c(107, 120, 133),
        dimmer: c(71, 82, 93),
        faint: c(42, 51, 60),
        accent: c(77, 166, 255),
        on_accent: c(4, 8, 13),
        accent_bg: c(7, 16, 24),
        danger: c(255, 90, 82),
        danger_line: c(44, 26, 26),
        meter_hot: c(11, 32, 48),
    },
    // Фосфор: зелёный по чёрному, как осциллограф или старый терминал.
    Palette {
        name: "Фосфор",
        bg: c(5, 8, 6),
        panel: c(10, 15, 11),
        line: c(22, 33, 26),
        line_dim: c(13, 20, 15),
        text: c(223, 234, 226),
        text_2: c(163, 181, 169),
        dim: c(102, 120, 108),
        dimmer: c(68, 85, 74),
        faint: c(42, 58, 47),
        accent: c(70, 208, 122),
        on_accent: c(4, 22, 10),
        accent_bg: c(6, 18, 11),
        danger: c(255, 95, 86),
        danger_line: c(44, 26, 26),
        meter_hot: c(10, 36, 22),
    },
    // Бумага: тёмное по светлому. Серые здесь свои: те, что тихо звучат на
    // чёрном, на светлом фоне просто исчезают.
    Palette {
        name: "Бумага",
        bg: c(244, 243, 240),
        panel: c(255, 255, 255),
        line: c(217, 214, 208),
        line_dim: c(230, 228, 223),
        text: c(22, 24, 28),
        text_2: c(63, 69, 77),
        dim: c(100, 106, 113),
        dimmer: c(153, 160, 167),
        faint: c(125, 132, 140),
        accent: c(31, 95, 208),
        on_accent: c(255, 255, 255),
        accent_bg: c(233, 238, 250),
        danger: c(179, 38, 30),
        danger_line: c(224, 196, 192),
        meter_hot: c(227, 217, 214),
    },
    // Янтарь: то, с чего всё начиналось.
    Palette {
        name: "Янтарь",
        bg: c(0, 0, 0),
        panel: c(6, 7, 8),
        line: c(26, 29, 31),
        line_dim: c(16, 19, 20),
        text: c(233, 236, 238),
        text_2: c(182, 188, 192),
        dim: c(106, 112, 117),
        dimmer: c(58, 64, 69),
        faint: c(46, 52, 56),
        accent: c(255, 149, 0),
        on_accent: c(0, 0, 0),
        accent_bg: c(13, 9, 4),
        danger: c(255, 69, 58),
        danger_line: c(51, 25, 26),
        meter_hot: c(42, 20, 8),
    },
];

/// Выбранный вид. Обычное число, а не мьютекс: читается из потока
/// отрисовки на каждый цвет каждого кадра, и платить за это блокировкой
/// было бы расточительно.
static THEME: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn theme() -> usize {
    THEME.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_theme(i: usize) {
    THEME.store(i.min(PALETTES.len() - 1), std::sync::atomic::Ordering::Relaxed);
}

pub fn palette() -> &'static Palette {
    &PALETTES[theme().min(PALETTES.len() - 1)]
}

// Имена оставлены заглавными: это те же цвета, что и раньше, просто теперь
// их значение зависит от выбранного вида.
#[allow(non_snake_case)] pub fn BG() -> Color32 { palette().bg }
#[allow(non_snake_case)] pub fn PANEL() -> Color32 { palette().panel }
#[allow(non_snake_case)] pub fn LINE() -> Color32 { palette().line }
#[allow(non_snake_case)] pub fn LINE_DIM() -> Color32 { palette().line_dim }
#[allow(non_snake_case)] pub fn TEXT() -> Color32 { palette().text }
#[allow(non_snake_case)] pub fn TEXT_2() -> Color32 { palette().text_2 }
#[allow(non_snake_case)] pub fn DIM() -> Color32 { palette().dim }
#[allow(non_snake_case)] pub fn DIMMER() -> Color32 { palette().dimmer }
#[allow(non_snake_case)] pub fn FAINT() -> Color32 { palette().faint }
#[allow(non_snake_case)] pub fn ACCENT() -> Color32 { palette().accent }
#[allow(non_snake_case)] pub fn ON_ACCENT() -> Color32 { palette().on_accent }
#[allow(non_snake_case)] pub fn ACCENT_BG() -> Color32 { palette().accent_bg }
#[allow(non_snake_case)] pub fn DANGER() -> Color32 { palette().danger }
#[allow(non_snake_case)] pub fn DANGER_LINE() -> Color32 { palette().danger_line }
#[allow(non_snake_case)] pub fn METER_HOT() -> Color32 { palette().meter_hot }

/// Разрядка букв. В egui её нет, поэтому вставляем тонкие пробелы —
/// без неё мелкие заглавные подписи выглядят слипшимися.
pub fn spaced(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for (i, c) in s.chars().enumerate() {
        if i > 0 {
            out.push('\u{2009}');
        }
        out.push(c);
    }
    out
}

/// Мелкая заглавная подпись с разрядкой.
pub fn micro(ui: &mut Ui, text: &str, color: Color32) -> egui::Response {
    ui.label(
        egui::RichText::new(spaced(text))
            .size(9.5)
            .color(color)
            .monospace(),
    )
}

pub fn mono(ui: &mut Ui, text: impl Into<String>, size: f32, color: Color32) {
    ui.label(
        egui::RichText::new(text.into())
            .size(size)
            .color(color)
            .monospace(),
    );
}

/// Волосяная линия во всю ширину.
pub fn hairline(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, color);
}

/// Угловые засечки вместо полной рамки — так отмечен активный блок.
pub fn corner_ticks(ui: &Ui, rect: Rect, color: Color32) {
    let p = ui.painter();
    let s = Stroke::new(1.0, color);
    let l = 7.0;
    p.line_segment([rect.left_top(), rect.left_top() + egui::vec2(l, 0.0)], s);
    p.line_segment([rect.left_top(), rect.left_top() + egui::vec2(0.0, l)], s);
    p.line_segment(
        [rect.right_bottom(), rect.right_bottom() - egui::vec2(l, 0.0)],
        s,
    );
    p.line_segment(
        [rect.right_bottom(), rect.right_bottom() - egui::vec2(0.0, l)],
        s,
    );
}

/// Поле ввода в высоту кнопки. Стандартное у egui вдвое ниже и с мелким
/// шрифтом — рядом с кнопками оно выглядит вдавленной щелью.
pub fn field(
    ui: &mut Ui,
    text: &mut String,
    hint: &str,
    size: f32,
    height: f32,
    limit: Option<usize>,
) -> Response {
    // Высота набирается вертикальными полями: у однострочного поля она
    // складывается из высоты строки и отступов, задать её напрямую нельзя.
    let pad = ((height - size * 1.35) / 2.0).max(4.0) as i8;
    let mut edit = egui::TextEdit::singleline(text)
        .desired_width(f32::INFINITY)
        .font(FontId::monospace(size))
        .margin(egui::Margin {
            left: 12,
            right: 12,
            top: pad,
            bottom: pad,
        })
        .hint_text(egui::RichText::new(hint).size(size).color(DIM()).monospace());
    if let Some(n) = limit {
        edit = edit.char_limit(n);
    }
    ui.add(edit)
}

/// Прямоугольная кнопка без скруглений.
pub fn button(
    ui: &mut Ui,
    text: &str,
    height: f32,
    width: Option<f32>,
    fill: Option<Color32>,
    border: Option<Color32>,
    fg: Color32,
    size: f32,
) -> Response {
    let w = width.unwrap_or(ui.available_width());
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, height), Sense::click());
    let hovered = resp.hovered();

    if let Some(f) = fill {
        let f = if hovered { lighten(f, 0.12) } else { f };
        ui.painter().rect_filled(rect, CornerRadius::ZERO, f);
    }
    if let Some(b) = border {
        let b = if hovered { lighten(b, 0.35) } else { b };
        ui.painter().rect_stroke(
            rect,
            CornerRadius::ZERO,
            Stroke::new(1.0, b),
            StrokeKind::Inside,
        );
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        spaced(text),
        FontId::monospace(size),
        fg,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn lighten(c: Color32, k: f32) -> Color32 {
    let f = |v: u8| (v as f32 + (255.0 - v as f32) * k).min(255.0) as u8;
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

/// В каком состоянии шаг подключения.
#[derive(PartialEq, Clone, Copy)]
pub enum Step {
    Done,
    /// Идёт прямо сейчас.
    Now,
    Wait,
}

/// Строка шага: отметка, название, пояснение и время справа.
///
/// Ради неё всё и затевалось: пока человек ждёт, он должен видеть, что
/// именно происходит. Молчаливое «ИЩЕМ ХОСТА» неотличимо от зависания.
pub fn step_row(ui: &mut Ui, state: Step, title: &str, note: &str, right: &str) {
    ui.horizontal_top(|ui| {
        let (mark, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), Sense::hover());
        let mark = Rect::from_min_size(mark.min + egui::vec2(0.0, 2.0), egui::vec2(13.0, 13.0));
        match state {
            Step::Done => {
                ui.painter().rect_filled(mark, CornerRadius::ZERO, ACCENT());
                let p = ui.painter();
                let s = Stroke::new(1.6, ON_ACCENT());
                p.line_segment(
                    [mark.left_center() + egui::vec2(3.0, 0.5), mark.center() + egui::vec2(-0.5, 3.0)],
                    s,
                );
                p.line_segment(
                    [mark.center() + egui::vec2(-0.5, 3.0), mark.right_top() + egui::vec2(-2.5, 3.5)],
                    s,
                );
            }
            Step::Now => {
                ui.painter().rect_stroke(
                    mark,
                    CornerRadius::ZERO,
                    Stroke::new(1.0, ACCENT()),
                    StrokeKind::Inside,
                );
                // Точка мигает: так видно, что оно живое, а не повисло.
                let t = ui.input(|i| i.time) as f32;
                let a = 0.35 + 0.65 * (t * 3.0).sin().abs();
                ui.painter().rect_filled(
                    Rect::from_center_size(mark.center(), egui::vec2(5.0, 5.0)),
                    CornerRadius::ZERO,
                    ACCENT().gamma_multiply(a),
                );
                ui.ctx().request_repaint();
            }
            Step::Wait => {
                ui.painter().rect_stroke(
                    mark,
                    CornerRadius::ZERO,
                    Stroke::new(1.0, FAINT()),
                    StrokeKind::Inside,
                );
            }
        }

        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.horizontal(|ui| {
                mono(
                    ui,
                    title,
                    12.5,
                    match state {
                        Step::Done => TEXT_2(),
                        Step::Now => TEXT(),
                        Step::Wait => DIM(),
                    },
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    mono(
                        ui,
                        spaced(right),
                        9.0,
                        if state == Step::Now { ACCENT() } else { DIM() },
                    );
                });
            });
            ui.label(
                egui::RichText::new(note)
                    .size(10.5)
                    .color(DIM())
                    .monospace(),
            );
        });
    });
}

/// Пронумерованный шаг инструкции: квадрат с цифрой и текст рядом.
pub fn numbered(ui: &mut Ui, n: u8, active: bool, title: &str, note: &str) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), Sense::hover());
        let box_rect = Rect::from_min_size(r.min + egui::vec2(0.0, 1.0), egui::vec2(19.0, 19.0));
        if active {
            ui.painter().rect_filled(box_rect, CornerRadius::ZERO, ACCENT());
        } else {
            ui.painter().rect_stroke(
                box_rect,
                CornerRadius::ZERO,
                Stroke::new(1.0, FAINT()),
                StrokeKind::Inside,
            );
        }
        ui.painter().text(
            box_rect.center(),
            Align2::CENTER_CENTER,
            n.to_string(),
            FontId::monospace(11.0),
            if active { ON_ACCENT() } else { TEXT_2() },
        );
        ui.add_space(7.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            mono(ui, title, 13.0, TEXT());
            ui.label(
                egui::RichText::new(note)
                    .size(10.5)
                    .color(DIM())
                    .monospace(),
            );
        });
    });
}

/// Выбор вида: четыре образца в ряд, каждый показан своими же цветами.
///
/// Название палитры мало что говорит, а три полоски — фон, акцент, текст —
/// говорят сразу. Поэтому выбираем глазами, а не по списку слов.
pub fn theme_picker(ui: &mut Ui) -> bool {
    let mut changed = false;
    let now = theme();
    let gap = 8.0;
    let w = (ui.available_width() - gap * (PALETTES.len() as f32 - 1.0)) / PALETTES.len() as f32;

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (i, p) in PALETTES.iter().enumerate() {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 46.0), Sense::click());
            let active = i == now;
            let hovered = resp.hovered();

            ui.painter().rect_filled(rect, CornerRadius::ZERO, p.bg);
            ui.painter().rect_stroke(
                rect,
                CornerRadius::ZERO,
                Stroke::new(1.0, if active { ACCENT() } else if hovered { DIMMER() } else { LINE() }),
                StrokeKind::Inside,
            );

            // Три полоски внутри — тем самым цветом, который выбираем.
            let sw_w = (rect.width() - 16.0 - 4.0) / 3.0;
            for (k, col) in [p.line, p.accent, p.text_2].into_iter().enumerate() {
                let x = rect.left() + 8.0 + k as f32 * (sw_w + 2.0);
                ui.painter().rect_filled(
                    Rect::from_min_size(egui::pos2(x, rect.top() + 9.0), egui::vec2(sw_w, 12.0)),
                    CornerRadius::ZERO,
                    col,
                );
            }

            ui.painter().text(
                egui::pos2(rect.center().x, rect.bottom() - 12.0),
                Align2::CENTER_CENTER,
                p.name.to_lowercase(),
                FontId::monospace(9.5),
                if active { p.text } else { p.dim },
            );

            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !active {
                set_theme(i);
                changed = true;
            }
        }
    });
    changed
}

/// Уровень в положение на шкале: линейно по децибелам, от -60 дБ до нуля.
/// По амплитуде шкала была бы бесполезной — весь разговор жался бы к левому краю.
pub fn level_to_pos(level: f32) -> f32 {
    if level <= 1e-6 {
        return 0.0;
    }
    (((20.0 * level.log10()) + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Сегментный индикатор уровня, как у аппаратного VU.
///
/// Принимает уже готовые положения на шкале (0..1), а не сырой уровень:
/// сглаживанием занимается вызывающий, иначе полоска дёргается.
pub fn meter(ui: &mut Ui, pos: f32, peak_pos: f32) {
    const N: usize = 28;
    const GAP: f32 = 2.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 16.0), Sense::hover());
    let seg_w = (rect.width() - GAP * (N as f32 - 1.0)) / N as f32;

    let lit = (pos.clamp(0.0, 1.0) * N as f32).round() as usize;
    let peak_i = ((peak_pos.clamp(0.0, 1.0) * N as f32).round() as usize).min(N - 1);

    for i in 0..N {
        let x = rect.left() + i as f32 * (seg_w + GAP);
        let seg = Rect::from_min_size(
            egui::pos2(x, rect.top()),
            egui::vec2(seg_w, rect.height()),
        );
        let color = if peak_pos > 0.02 && i == peak_i {
            Color32::WHITE
        } else if i < lit {
            ACCENT()
        } else if i >= N - 2 {
            METER_HOT()
        } else {
            LINE()
        };
        ui.painter().rect_filled(seg, CornerRadius::ZERO, color);
    }
}

/// Шкала под индикатором. Деления ровные, потому что шкала в децибелах.
pub fn meter_scale(ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let marks = ["-60", "-48", "-36", "-24", "-12", "0"];
        let w = ui.available_width();
        for (i, m) in marks.iter().enumerate() {
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(w / marks.len() as f32, 11.0),
                Sense::hover(),
            );
            let anchor = if i == 0 {
                Align2::LEFT_CENTER
            } else if i == marks.len() - 1 {
                Align2::RIGHT_CENTER
            } else {
                Align2::CENTER_CENTER
            };
            let pos = match anchor {
                Align2::LEFT_CENTER => rect.left_center(),
                Align2::RIGHT_CENTER => rect.right_center(),
                _ => rect.center(),
            };
            ui.painter()
                .text(pos, anchor, *m, FontId::monospace(8.5), FAINT());
        }
    });
}

/// Строка-переключатель: квадрат, название, метка справа и пояснение под ними.
pub fn toggle_row(ui: &mut Ui, on: &mut bool, title: &str, tag: &str, note: &str) -> bool {
    let mut changed = false;
    let resp = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.horizontal_top(|ui| {
                let (box_rect, _) =
                    ui.allocate_exact_size(egui::vec2(13.0, 13.0), Sense::hover());
                if *on {
                    ui.painter()
                        .rect_filled(box_rect, CornerRadius::ZERO, ACCENT());
                } else {
                    ui.painter().rect_stroke(
                        box_rect,
                        CornerRadius::ZERO,
                        Stroke::new(1.0, DIMMER()),
                        StrokeKind::Inside,
                    );
                }
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        mono(ui, spaced(title), 11.5, if *on { TEXT() } else { TEXT_2() });
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                mono(ui, spaced(tag), 9.0, FAINT());
                            },
                        );
                    });
                    ui.label(
                        egui::RichText::new(note)
                            .size(10.0)
                            .color(DIM())
                            .monospace(),
                    );
                });
            });
        })
        .response
        .interact(Sense::click());

    if resp.clicked() {
        *on = !*on;
        changed = true;
    }
    changed
}

/// Ползунок: волосяной трек, прямоугольная ручка, подписи по краям.
pub fn slider(
    ui: &mut Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    title: &str,
    readout: &str,
    left: &str,
    right: &str,
) {
    ui.horizontal(|ui| {
        micro(ui, title, DIM());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            mono(ui, readout, 11.0, ACCENT());
        });
    });
    ui.add_space(6.0);

    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 14.0), Sense::click_and_drag());
    let (lo, hi) = (*range.start(), *range.end());

    if resp.dragged() || resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            let t = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            *value = lo + t * (hi - lo);
        }
    }

    let t = ((*value - lo) / (hi - lo)).clamp(0.0, 1.0);
    let y = rect.top() + 6.0;
    let track = Rect::from_min_size(egui::pos2(rect.left(), y), egui::vec2(rect.width(), 1.0));
    ui.painter().rect_filled(track, CornerRadius::ZERO, LINE());
    let filled = Rect::from_min_size(
        egui::pos2(rect.left(), y),
        egui::vec2(rect.width() * t, 1.0),
    );
    ui.painter().rect_filled(filled, CornerRadius::ZERO, ACCENT());
    let handle = Rect::from_min_size(
        egui::pos2(rect.left() + rect.width() * t - 1.5, rect.top()),
        egui::vec2(3.0, 14.0),
    );
    ui.painter().rect_filled(handle, CornerRadius::ZERO, ACCENT());

    ui.add_space(3.0);
    ui.horizontal(|ui| {
        mono(ui, spaced(left), 8.0, FAINT());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            mono(ui, spaced(right), 8.0, FAINT());
        });
    });
}

/// Заголовок сворачиваемой секции: значок, название с разрядкой и
/// необязательная метка справа. Кликается вся строка во всю ширину.
///
/// Возвращает состояние — тело рисуется через `show_body_unindented`,
/// который сам анимирует раскрытие.
pub fn section(
    ui: &mut Ui,
    id: &str,
    title: &str,
    right: Option<(&str, Color32)>,
) -> egui::collapsing_header::CollapsingState {
    use egui::collapsing_header::CollapsingState;

    let id = ui.make_persistent_id(id);
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, false);
    let openness = state.openness(ui.ctx());

    hairline(ui, LINE());
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), Sense::click());
    if resp.clicked() {
        state.toggle(ui);
        state.store(ui.ctx());
    }
    let hovered = resp.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let open = openness > 0.5;
    let mark = if open || hovered { ACCENT() } else { DIM() };
    let fg = if open || hovered { TEXT() } else { TEXT_2() };

    let p = ui.painter();
    // Плюс перетекает в минус: вертикальная палочка укорачивается по мере
    // раскрытия. Дешевле анимации значка и читается сразу.
    let cx = rect.left() + 5.0;
    let cy = rect.center().y;
    let arm = 4.5;
    p.line_segment(
        [egui::pos2(cx - arm, cy), egui::pos2(cx + arm, cy)],
        Stroke::new(1.0, mark),
    );
    let v = arm * (1.0 - openness);
    if v > 0.2 {
        p.line_segment(
            [egui::pos2(cx, cy - v), egui::pos2(cx, cy + v)],
            Stroke::new(1.0, mark),
        );
    }
    p.text(
        egui::pos2(rect.left() + 18.0, cy),
        Align2::LEFT_CENTER,
        spaced(title),
        FontId::monospace(10.5),
        fg,
    );
    if let Some((r, c)) = right {
        p.text(
            egui::pos2(rect.right(), cy),
            Align2::RIGHT_CENTER,
            spaced(r),
            FontId::monospace(9.5),
            c,
        );
    }

    state
}

/// Компактный фейдер громкости собеседника — как канальный на пульте.
/// Диапазон 0..2, единица посередине помечена засечкой.
pub fn mini_fader(ui: &mut Ui, value: &mut f32, width: f32) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 12.0), Sense::click_and_drag());
    let mut changed = false;

    if resp.dragged() || resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            let t = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            *value = t * 2.0;
            changed = true;
        }
    }
    // Двойной щелчок возвращает единицу — иначе поймать её мышью невозможно.
    if resp.double_clicked() {
        *value = 1.0;
        changed = true;
    }

    let t = (*value / 2.0).clamp(0.0, 1.0);
    let y = rect.center().y;
    let p = ui.painter();
    p.line_segment(
        [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
        Stroke::new(1.0, LINE()),
    );
    // Засечка единицы.
    let mid = rect.left() + rect.width() * 0.5;
    p.line_segment(
        [egui::pos2(mid, y - 3.0), egui::pos2(mid, y + 3.0)],
        Stroke::new(1.0, LINE()),
    );
    p.line_segment(
        [
            egui::pos2(rect.left(), y),
            egui::pos2(rect.left() + rect.width() * t, y),
        ],
        Stroke::new(1.0, if resp.hovered() { ACCENT() } else { DIMMER() }),
    );
    let hx = rect.left() + rect.width() * t;
    p.rect_filled(
        Rect::from_min_size(egui::pos2(hx - 1.0, rect.top() + 1.0), egui::vec2(2.0, 10.0)),
        CornerRadius::ZERO,
        if resp.hovered() { ACCENT() } else { DIM() },
    );

    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    changed
}

/// Схема тракта: что включено и во что это обходится по задержке.
pub fn chain(ui: &mut Ui, aec: bool, dfn: bool, gate: bool) {
    let stages: [(&str, &str, bool); 5] = [
        ("MIC", "48K", false),
        ("AEC", "0 MS", aec),
        ("DFN3", "10 MS", dfn),
        ("GATE", "40 MS", gate),
        ("OPUS", "20 MS", false),
    ];

    let total = 20 + if dfn { 10 } else { 0 } + if gate { 40 } else { 0 };
    let gap = 8.0;
    let w = (ui.available_width() - gap * 4.0) / 5.0;

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (i, (name, cost, on)) in stages.iter().enumerate() {
            ui.vertical(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 28.0), Sense::hover());
                let active = *on;
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::ZERO,
                    if active { ACCENT_BG() } else { PANEL() },
                );
                ui.painter().rect_stroke(
                    rect,
                    CornerRadius::ZERO,
                    Stroke::new(1.0, if active { DIMMER() } else { LINE() }),
                    StrokeKind::Inside,
                );
                if active {
                    corner_ticks(ui, rect, ACCENT());
                }
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    spaced(name),
                    FontId::monospace(9.0),
                    if active {
                        ACCENT()
                    } else if i == 0 || i == 4 {
                        DIM()
                    } else {
                        FAINT()
                    },
                );
                ui.add_space(4.0);
                let (crect, _) = ui.allocate_exact_size(egui::vec2(w, 10.0), Sense::hover());
                ui.painter().text(
                    crect.center(),
                    Align2::CENTER_CENTER,
                    *cost,
                    FontId::monospace(8.0),
                    if active { FAINT() } else { LINE() },
                );
            });
        }
    });

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            mono(ui, spaced(&format!("ЗАДЕРЖКА ТРАКТА · {total} MS")), 9.0, DIM());
        });
    });
}
