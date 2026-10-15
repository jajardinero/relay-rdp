//! Small custom widgets shared across screens.

use std::time::Duration;

use egui::{Color32, CornerRadius, Response, RichText, Sense, Stroke, Ui, Vec2, pos2, vec2};

use super::theme::{self, Palette, radius, size};

/// Link quality, from round-trip time. Shown as a four-bar signal glyph everywhere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    /// Not measured yet.
    Unknown,
    Offline,
    Rtt(Duration),
}

impl Signal {
    pub fn bars(self) -> usize {
        match self {
            Signal::Unknown | Signal::Offline => 0,
            Signal::Rtt(rtt) => match rtt.as_millis() {
                0..=20 => 4,
                21..=60 => 3,
                61..=150 => 2,
                _ => 1,
            },
        }
    }

    pub fn color(self, p: &Palette) -> Color32 {
        match self.bars() {
            0 if self == Signal::Offline => p.poor,
            0 => p.muted,
            1 => p.poor,
            2 => p.fair,
            _ => p.good,
        }
    }

    pub fn label(self) -> String {
        match self {
            Signal::Unknown => "Checking…".to_owned(),
            Signal::Offline => "Offline".to_owned(),
            Signal::Rtt(rtt) => format!("{} ms", rtt.as_millis().max(1)),
        }
    }
}

pub fn signal_bars(ui: &mut Ui, signal: Signal) -> Response {
    let p = Palette::of(ui);
    let (rect, response) = ui.allocate_exact_size(vec2(16.0, 14.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let lit = signal.bars();
    let color = signal.color(&p);
    let bar_w = 3.0;
    let gap = 1.5;
    for i in 0..4 {
        let h = 4.0 + i as f32 * 3.0;
        let x = rect.left() + i as f32 * (bar_w + gap);
        let bar =
            egui::Rect::from_min_max(pos2(x, rect.bottom() - h), pos2(x + bar_w, rect.bottom()));
        let fill = if i < lit { color } else { p.hairline };
        painter.rect_filled(bar, CornerRadius::same(1), fill);
    }
    if signal == Signal::Offline {
        // A small cross over the empty bars.
        let c = rect.right_top() + vec2(-3.0, 3.0);
        let s = Stroke::new(1.5, p.poor);
        painter.line_segment([c + vec2(-2.5, -2.5), c + vec2(2.5, 2.5)], s);
        painter.line_segment([c + vec2(-2.5, 2.5), c + vec2(2.5, -2.5)], s);
    }
    response
}

/// A minimal line chart of recent values (e.g. RTT over the last minute).
pub fn sparkline(ui: &mut Ui, values: &[f32], desired: Vec2, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(desired, Sense::hover());
    if values.len() < 2 {
        return response;
    }
    let max = values.iter().copied().fold(1.0_f32, f32::max) * 1.15;
    let n = values.len() as f32 - 1.0;
    let points: Vec<egui::Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            pos2(
                rect.left() + rect.width() * i as f32 / n,
                rect.bottom() - rect.height() * (v / max).clamp(0.0, 1.0),
            )
        })
        .collect();
    ui.painter()
        .add(egui::Shape::line(points, Stroke::new(1.5, color)));
    response
}

pub fn primary_button(ui: &mut Ui, text: impl Into<String>) -> Response {
    let p = Palette::of(ui);
    let label = RichText::new(text.into())
        .font(theme::font_medium(size::BODY))
        .color(p.on_accent);
    let fill = if ui.is_enabled() {
        p.accent
    } else {
        p.accent.gamma_multiply(0.5)
    };
    ui.add(
        egui::Button::new(label)
            .fill(fill)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(radius::CONTROL))
            .min_size(vec2(0.0, 34.0)),
    )
}

pub fn secondary_button(ui: &mut Ui, text: impl Into<String>) -> Response {
    let label = RichText::new(text.into()).font(theme::font_medium(size::BODY));
    ui.add(
        egui::Button::new(label)
            .corner_radius(CornerRadius::same(radius::CONTROL))
            .min_size(vec2(0.0, 34.0)),
    )
}

pub fn danger_button(ui: &mut Ui, text: impl Into<String>) -> Response {
    let p = Palette::of(ui);
    let label = RichText::new(text.into())
        .font(theme::font_medium(size::BODY))
        .color(p.poor);
    ui.add(
        egui::Button::new(label)
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::new(1.0, p.poor.gamma_multiply(0.6)))
            .corner_radius(CornerRadius::same(radius::CONTROL))
            .min_size(vec2(0.0, 34.0)),
    )
}

/// An icon-only button that shows a background only on hover.
pub fn icon_button(ui: &mut Ui, icon: &str, tooltip: &str) -> Response {
    let p = Palette::of(ui);
    let size = 30.0;
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    if ui.is_rect_visible(rect) {
        if response.hovered() || response.has_focus() {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(radius::CONTROL), p.raised);
        }
        let color = if response.hovered() { p.text } else { p.muted };
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            icon,
            theme::font(17.0),
            color,
        );
    }
    response.on_hover_text(tooltip)
}

/// An on/off switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let p = Palette::of(ui);
    let desired = vec2(36.0, 20.0);
    let (rect, mut response) = ui.allocate_exact_size(desired, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        let track = if *on { p.accent } else { p.hairline };
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), track);
        let knob_x = egui::lerp(rect.left() + 10.0..=rect.right() - 10.0, t);
        let knob = if *on { p.on_accent } else { p.text };
        ui.painter()
            .circle_filled(pos2(knob_x, rect.center().y), 7.0, knob);
    }
    response
}

/// A right-aligned row of buttons with a fixed height. (`with_layout(right_to_left)` straight
/// inside a container of unbounded height would claim all of it and center the buttons.)
pub fn button_row(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 36.0),
        egui::Layout::right_to_left(egui::Align::Center),
        add,
    );
}

/// A setting row: label (and optional description) on the left, control on the right.
pub fn setting_row(
    ui: &mut Ui,
    label: &str,
    description: Option<&str>,
    control: impl FnOnce(&mut Ui),
) {
    let p = Palette::of(ui);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(label).font(theme::font_medium(size::BODY)));
            if let Some(description) = description {
                ui.label(RichText::new(description).size(size::SMALL).color(p.muted));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
}

/// A labelled single-line text field.
pub fn field(ui: &mut Ui, label: &str, value: &mut String, hint: &str) -> Response {
    let p = Palette::of(ui);
    ui.label(RichText::new(label).size(size::SMALL).color(p.muted));
    text_input(ui, value, hint, false)
}

pub fn text_input(ui: &mut Ui, value: &mut String, hint: &str, password: bool) -> Response {
    let p = Palette::of(ui);
    ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(RichText::new(hint).color(p.muted))
            .password(password)
            .margin(egui::Margin::symmetric(10, 8))
            .desired_width(f32::INFINITY),
    )
}

/// A small rounded chip with a short label, e.g. "GPU".
pub fn tag(ui: &mut Ui, text: &str, fill: Color32, color: Color32) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), theme::font_medium(11.0), color);
    let size = galley.size() + vec2(10.0, 4.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, CornerRadius::same(4), fill);
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, color);
    }
    response
}

/// Muted heading for a group of rows.
pub fn section_label(ui: &mut Ui, text: &str) {
    let p = Palette::of(ui);
    ui.label(
        RichText::new(text)
            .font(theme::font_medium(size::SMALL))
            .color(p.muted),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_bars_by_latency() {
        let ms = |v| Signal::Rtt(Duration::from_millis(v));
        assert_eq!(ms(3).bars(), 4);
        assert_eq!(ms(45).bars(), 3);
        assert_eq!(ms(120).bars(), 2);
        assert_eq!(ms(400).bars(), 1);
        assert_eq!(Signal::Offline.bars(), 0);
    }
}
