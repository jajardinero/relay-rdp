//! Short-lived notifications at the bottom of the window.

use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, RichText};
use egui_phosphor::regular as icon;

use super::theme::{self, Palette, size};

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Info,
    Error,
}

struct Toast {
    text: String,
    kind: Kind,
    until: Instant,
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    pub fn info(&mut self, text: impl Into<String>) {
        self.push(text.into(), Kind::Info, Duration::from_secs(3));
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.push(text.into(), Kind::Error, Duration::from_secs(6));
    }

    fn push(&mut self, text: String, kind: Kind, ttl: Duration) {
        self.items.retain(|t| t.text != text);
        self.items.push(Toast {
            text,
            kind,
            until: Instant::now() + ttl,
        });
        if self.items.len() > 3 {
            self.items.remove(0);
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.items.retain(|t| t.until > now);
        if self.items.is_empty() {
            return;
        }
        let p = Palette::of_ctx(ctx);

        egui::Area::new(egui::Id::new("toasts"))
            .order(egui::Order::Tooltip)
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -24.0))
            .interactable(false)
            .show(ctx, |ui| {
                for toast in &self.items {
                    let (glyph, color) = match toast.kind {
                        Kind::Info => (icon::CHECK_CIRCLE, p.good),
                        Kind::Error => (icon::WARNING_CIRCLE, p.poor),
                    };
                    egui::Frame::new()
                        .fill(if p.dark {
                            p.raised
                        } else {
                            Color32::from_rgb(0x1C, 0x22, 0x32)
                        })
                        .corner_radius(CornerRadius::same(theme::radius::ROW))
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .shadow(ui.visuals().popup_shadow)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(glyph).size(16.0).color(color));
                                let text_color = if p.dark {
                                    p.text
                                } else {
                                    Color32::from_rgb(0xE6, 0xE9, 0xF2)
                                };
                                ui.label(
                                    RichText::new(&toast.text)
                                        .size(size::BODY)
                                        .color(text_color),
                                );
                            });
                        });
                    ui.add_space(6.0);
                }
            });

        if let Some(next) = self.items.iter().map(|t| t.until).min() {
            ctx.request_repaint_after(next.saturating_duration_since(now));
        }
    }
}
