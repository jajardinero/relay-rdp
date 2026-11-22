//! Short-lived notifications at the bottom of the window.

use std::time::{Duration, Instant};

use gpui_kit::{AnyElement, IntoElement, ParentElement, Styled, div, px, rgb};

use super::assets::{icon_svg, icons};
use super::theme::{Palette, radius, size};

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

    pub fn prune(&mut self) -> Option<Instant> {
        let now = Instant::now();
        self.items.retain(|t| t.until > now);
        self.items.iter().map(|t| t.until).min()
    }

    pub fn render(&self, p: &Palette) -> Option<AnyElement> {
        if self.items.is_empty() {
            return None;
        }
        let bg = if p.dark { p.raised } else { rgb(0x1C2232).into() };
        let text_color = if p.dark { p.text } else { rgb(0xE6E9F2).into() };

        Some(
            div()
                .absolute()
                .bottom(px(24.0))
                .left_0()
                .right_0()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.0))
                .children(self.items.iter().map(|toast| {
                    let (icon_path, color) = match toast.kind {
                        Kind::Info => (icons::CHECK_CIRCLE, p.good),
                        Kind::Error => (icons::WARNING_CIRCLE, p.poor),
                    };
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(14.0))
                        .py(px(10.0))
                        .rounded(radius::ROW)
                        .bg(bg)
                        .shadow(p.popup_shadow())
                        .child(icon_svg(icon_path, px(16.0), color))
                        .child(
                            div()
                                .text_size(size::BODY)
                                .text_color(text_color)
                                .child(toast.text.clone()),
                        )
                }))
                .into_any_element(),
        )
    }
}
