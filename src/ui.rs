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

pub const BG: Color32 = Color32::from_rgb(0, 0, 0);
pub const PANEL: Color32 = Color32::from_rgb(6, 7, 8);
pub const LINE: Color32 = Color32::from_rgb(26, 29, 31);
pub const LINE_DIM: Color32 = Color32::from_rgb(16, 19, 20);
pub const TEXT: Color32 = Color32::from_rgb(233, 236, 238);
pub const TEXT_2: Color32 = Color32::from_rgb(182, 188, 192);
pub const DIM: Color32 = Color32::from_rgb(106, 112, 117);
pub const DIMMER: Color32 = Color32::from_rgb(58, 64, 69);
pub const FAINT: Color32 = Color32::from_rgb(46, 52, 56);
pub const ACCENT: Color32 = Color32::from_rgb(255, 149, 0);
pub const ACCENT_BG: Color32 = Color32::from_rgb(13, 9, 4);
pub const DANGER: Color32 = Color32::from_rgb(255, 69, 58);
pub const DANGER_LINE: Color32 = Color32::from_rgb(51, 25, 26);
pub const METER_HOT: Color32 = Color32::from_rgb(42, 20, 8);

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
pub fn micro(ui: &mut Ui, text: &str, color: Color32) {
    ui.label(
        egui::RichText::new(spaced(text))
            .size(9.5)
            .color(color)
            .monospace(),
    );
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

/// Уровень в положение на шкале: линейно по децибелам, от -60 дБ до нуля.
/// По амплитуде шкала была бы бесполезной — весь разговор жался бы к левому краю.
pub fn level_to_pos(level: f32) -> f32 {
    if level <= 1e-6 {
        return 0.0;
    }
    (((20.0 * level.log10()) + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Сегментный индикатор уровня, как у аппаратного VU.
pub fn meter(ui: &mut Ui, level: f32, peak: f32) {
    const N: usize = 28;
    const GAP: f32 = 2.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 16.0), Sense::hover());
    let seg_w = (rect.width() - GAP * (N as f32 - 1.0)) / N as f32;

    let lit = (level_to_pos(level) * N as f32).round() as usize;
    let peak_pos = level_to_pos(peak);
    let peak_i = ((peak_pos * N as f32).round() as usize).min(N - 1);

    for i in 0..N {
        let x = rect.left() + i as f32 * (seg_w + GAP);
        let seg = Rect::from_min_size(
            egui::pos2(x, rect.top()),
            egui::vec2(seg_w, rect.height()),
        );
        let color = if peak_pos > 0.02 && i == peak_i {
            Color32::WHITE
        } else if i < lit {
            ACCENT
        } else if i >= N - 2 {
            METER_HOT
        } else {
            LINE
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
                .text(pos, anchor, *m, FontId::monospace(8.5), FAINT);
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
                        .rect_filled(box_rect, CornerRadius::ZERO, ACCENT);
                } else {
                    ui.painter().rect_stroke(
                        box_rect,
                        CornerRadius::ZERO,
                        Stroke::new(1.0, DIMMER),
                        StrokeKind::Inside,
                    );
                }
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        mono(ui, spaced(title), 11.5, if *on { TEXT } else { TEXT_2 });
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                mono(ui, spaced(tag), 9.0, FAINT);
                            },
                        );
                    });
                    ui.label(
                        egui::RichText::new(note)
                            .size(10.0)
                            .color(DIM)
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
        micro(ui, title, DIM);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            mono(ui, readout, 11.0, ACCENT);
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
    ui.painter().rect_filled(track, CornerRadius::ZERO, LINE);
    let filled = Rect::from_min_size(
        egui::pos2(rect.left(), y),
        egui::vec2(rect.width() * t, 1.0),
    );
    ui.painter().rect_filled(filled, CornerRadius::ZERO, ACCENT);
    let handle = Rect::from_min_size(
        egui::pos2(rect.left() + rect.width() * t - 1.5, rect.top()),
        egui::vec2(3.0, 14.0),
    );
    ui.painter().rect_filled(handle, CornerRadius::ZERO, ACCENT);

    ui.add_space(3.0);
    ui.horizontal(|ui| {
        mono(ui, spaced(left), 8.0, FAINT);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            mono(ui, spaced(right), 8.0, FAINT);
        });
    });
}

/// Заголовок сворачиваемой секции: «+» или «−», название с разрядкой,
/// необязательная метка справа. Кликается вся строка.
pub fn section(ui: &mut Ui, open: &mut bool, title: &str, right: Option<(&str, Color32)>) {
    hairline(ui, LINE);
    let resp = ui
        .scope(|ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                mono(ui, if *open { "−" } else { "+" }, 10.0, if *open { ACCENT } else { DIM });
                ui.add_space(4.0);
                mono(ui, spaced(title), 10.5, if *open { TEXT } else { TEXT_2 });
                if let Some((r, c)) = right {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        mono(ui, spaced(r), 9.5, c);
                    });
                }
            });
            ui.add_space(4.0);
        })
        .response
        .interact(Sense::click());

    if resp.clicked() {
        *open = !*open;
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
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
                    if active { ACCENT_BG } else { PANEL },
                );
                ui.painter().rect_stroke(
                    rect,
                    CornerRadius::ZERO,
                    Stroke::new(1.0, if active { DIMMER } else { LINE }),
                    StrokeKind::Inside,
                );
                if active {
                    corner_ticks(ui, rect, ACCENT);
                }
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    spaced(name),
                    FontId::monospace(9.0),
                    if active {
                        ACCENT
                    } else if i == 0 || i == 4 {
                        DIM
                    } else {
                        FAINT
                    },
                );
                ui.add_space(4.0);
                let (crect, _) = ui.allocate_exact_size(egui::vec2(w, 10.0), Sense::hover());
                ui.painter().text(
                    crect.center(),
                    Align2::CENTER_CENTER,
                    *cost,
                    FontId::monospace(8.0),
                    if active { FAINT } else { LINE },
                );
            });
        }
    });

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            mono(ui, spaced(&format!("ЗАДЕРЖКА ТРАКТА · {total} MS")), 9.0, DIM);
        });
    });
}
