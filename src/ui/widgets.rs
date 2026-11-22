//! Small custom widgets shared across screens.

use std::time::Duration;

use gpui_kit::component::popover::{Popover, PopoverState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::Selectable;
use gpui_kit::{
    Anchor, AnyElement, App, ClickEvent, Context, Div, ElementId, FontWeight, Hsla,
    InteractiveElement, IntoElement, ParentElement, PathBuilder, Pixels, RenderOnce, Stateful,
    StatefulInteractiveElement, StyleRefinement, Styled, Window, canvas, div, point,
    prelude::FluentBuilder as _, px, transparent_black,
};

use super::assets::{icon_svg, icons};
use super::theme::{Palette, radius, size};

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

    pub fn color(self, p: &Palette) -> Hsla {
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

/// Four-bar link quality glyph, with a red cross over the corner when offline.
pub fn signal_bars(id: impl Into<ElementId>, signal: Signal, p: &Palette) -> Stateful<Div> {
    let lit = signal.bars();
    let color = signal.color(p);
    let dim = p.hairline;
    let poor = p.poor;
    let offline = signal == Signal::Offline;
    let label = signal.label();

    div()
        .id(id)
        .w(px(16.0))
        .h(px(14.0))
        .flex_shrink_0()
        .relative()
        .tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
        .child(
            div()
                .flex()
                .items_end()
                .gap(px(1.5))
                .w(px(16.0))
                .h(px(14.0))
                .children((0..4).map(|i| {
                    let h = 4.0 + i as f32 * 3.0;
                    div()
                        .w(px(3.0))
                        .h(px(h))
                        .rounded(px(1.0))
                        .bg(if i < lit { color } else { dim })
                })),
        )
        .when(offline, |el| {
            el.child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let c = point(
                            bounds.origin.x + bounds.size.width - px(3.0),
                            bounds.origin.y + px(3.0),
                        );
                        let mut p1 = PathBuilder::stroke(px(1.5));
                        p1.move_to(point(c.x - px(2.5), c.y - px(2.5)));
                        p1.line_to(point(c.x + px(2.5), c.y + px(2.5)));
                        if let Ok(path) = p1.build() {
                            window.paint_path(path, poor);
                        }
                        let mut p2 = PathBuilder::stroke(px(1.5));
                        p2.move_to(point(c.x - px(2.5), c.y + px(2.5)));
                        p2.line_to(point(c.x + px(2.5), c.y - px(2.5)));
                        if let Ok(path) = p2.build() {
                            window.paint_path(path, poor);
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
        })
}

/// The Relay mark: four ascending bars in accent color.
pub fn relay_logo(p: &Palette) -> Div {
    div()
        .flex()
        .items_end()
        .gap(px(1.5))
        .w(px(20.0))
        .h(px(18.0))
        .flex_shrink_0()
        .children((0..4).map(|i| {
            let h = 6.0 + i as f32 * 4.0;
            div()
                .w(px(3.5))
                .h(px(h))
                .rounded(px(1.0))
                .bg(p.accent)
        }))
}

/// A minimal line chart of recent values (e.g. RTT over the last minute).
pub fn sparkline(values: Vec<f32>, width: Pixels, height: Pixels, color: Hsla) -> AnyElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            if values.len() < 2 {
                return;
            }
            let max = values.iter().copied().fold(1.0_f32, f32::max) * 1.15;
            let n = (values.len() - 1) as f32;
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            let mut builder = PathBuilder::stroke(px(1.5));
            for (i, v) in values.iter().enumerate() {
                let pt = point(
                    bounds.origin.x + px(w * i as f32 / n),
                    bounds.origin.y + px(h - h * (v / max).clamp(0.0, 1.0)),
                );
                if i == 0 {
                    builder.move_to(pt);
                } else {
                    builder.line_to(pt);
                }
            }
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        },
    )
    .w(width)
    .h(height)
    .into_any_element()
}

pub fn primary_button(
    id: impl Into<ElementId>,
    text: impl Into<String>,
    p: &Palette,
    disabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let fill = if disabled {
        p.accent.opacity(0.5)
    } else {
        p.accent
    };
    let hover_fill = p.accent.opacity(0.9);
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(34.0))
        .px(px(14.0))
        .rounded(radius::CONTROL)
        .bg(fill)
        .text_color(p.on_accent)
        .text_size(size::BODY)
        .font_weight(FontWeight::MEDIUM)
        .when(!disabled, |el| {
            el.cursor_pointer()
                .hover(move |s| s.bg(hover_fill))
                .on_click(on_click)
        })
        .child(text.into())
}

pub fn secondary_button(
    id: impl Into<ElementId>,
    text: impl Into<String>,
    p: &Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let hover_fill = p.control_hover;
    let border = p.hairline;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(34.0))
        .px(px(14.0))
        .rounded(radius::CONTROL)
        .bg(p.raised)
        .border_1()
        .border_color(transparent_black())
        .text_color(p.text)
        .text_size(size::BODY)
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_fill).border_color(border))
        .on_click(on_click)
        .child(text.into())
}

pub fn danger_button(
    id: impl Into<ElementId>,
    text: impl Into<String>,
    p: &Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let poor = p.poor;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(34.0))
        .px(px(14.0))
        .rounded(radius::CONTROL)
        .bg(transparent_black())
        .border_1()
        .border_color(poor.opacity(0.6))
        .text_color(poor)
        .text_size(size::BODY)
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .hover(move |s| s.bg(poor.opacity(0.12)).border_color(poor))
        .on_click(on_click)
        .child(text.into())
}

/// An icon-only button that shows a raised background only on hover.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon_path: &'static str,
    tooltip: impl Into<String>,
    p: &Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let raised = p.raised;
    let tip = tooltip.into();
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(30.0))
        .rounded(radius::CONTROL)
        .cursor_pointer()
        .hover(move |s| s.bg(raised))
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .on_click(on_click)
        .child(icon_svg(icon_path, px(16.0), p.muted))
}

/// An on/off switch matching Relay's palette.
pub fn toggle(
    id: impl Into<ElementId>,
    checked: bool,
    p: &Palette,
    on_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Switch {
    Switch::new(id)
        .checked(checked)
        .color(p.accent)
        .on_change(on_change)
}

/// A right-aligned row of buttons.
pub fn button_row() -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(8.0))
        .w_full()
}

/// A setting row: label (and optional description) on the left, control on the right.
pub fn setting_row(
    label: impl Into<String>,
    description: Option<&str>,
    p: &Palette,
    control: impl IntoElement,
) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .w_full()
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(size::BODY)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.text)
                        .child(label.into()),
                )
                .when_some(description, |el, desc| {
                    el.child(
                        div()
                            .text_size(size::SMALL)
                            .text_color(p.muted)
                            .child(desc.to_owned()),
                    )
                }),
        )
        .child(div().flex_shrink_0().child(control))
}

/// A small rounded chip with a short label, e.g. "GPU".
pub fn tag(text: impl Into<String>, fill: Hsla, color: Hsla) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .px(px(5.0))
        .py(px(2.0))
        .rounded(px(4.0))
        .bg(fill)
        .text_color(color)
        .text_size(px(11.0))
        .font_weight(FontWeight::MEDIUM)
        .child(text.into())
}

/// Muted heading for a group of rows.
pub fn section_label(text: impl Into<String>, p: &Palette) -> Div {
    div()
        .text_size(size::SMALL)
        .font_weight(FontWeight::MEDIUM)
        .text_color(p.muted)
        .child(text.into())
}

/// Labelled form field wrapper.
pub fn field_label(label: impl Into<String>, p: &Palette) -> Div {
    div()
        .text_size(size::SMALL)
        .text_color(p.muted)
        .mb(px(4.0))
        .child(label.into())
}

/// Custom Selectable trigger wrapper for GPUI Kit Popovers so triggers keep Relay's exact styling.
#[derive(IntoElement)]
pub struct PopoverTrigger {
    id: ElementId,
    selected: bool,
    open: bool,
    child: AnyElement,
}

impl PopoverTrigger {
    pub fn new(id: impl Into<ElementId>, child: impl IntoElement) -> Self {
        Self {
            id: id.into(),
            selected: false,
            open: false,
            child: child.into_any_element(),
        }
    }
}

impl Selectable for PopoverTrigger {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }

    fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    fn is_open(&self) -> bool {
        self.open
    }
}

impl RenderOnce for PopoverTrigger {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div().id(self.id).child(self.child)
    }
}

/// A full-width Relay-styled dropdown selector backed by a GPUI Kit `Popover`.
pub fn dropdown_select(
    id: impl Into<ElementId>,
    selected_label: impl Into<String>,
    p: &Palette,
    content: impl Fn(&mut PopoverState, &mut Window, &mut Context<PopoverState>) -> Div + 'static,
) -> Popover {
    let id = id.into();
    let label = selected_label.into();
    let raised = p.raised;
    let hover = p.control_hover;
    let hairline = p.hairline;
    let text = p.text;
    let muted = p.muted;
    let surface = p.surface;
    let shadow = p.popup_shadow();

    let trigger_el = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .w_full()
        .h(px(34.0))
        .px(px(10.0))
        .rounded(radius::CONTROL)
        .bg(raised)
        .border_1()
        .border_color(hairline)
        .text_size(size::BODY)
        .text_color(text)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(div().flex_1().truncate().child(label))
        .child(icon_svg(icons::CARET_DOWN, px(14.0), muted));

    let mut trigger_style = StyleRefinement::default();
    trigger_style.size.width = Some(gpui_kit::relative(1.0).into());

    Popover::new(id.clone())
        .anchor(Anchor::TopLeft)
        .appearance(false)
        .trigger_style(trigger_style)
        .trigger(PopoverTrigger::new((id, "trigger"), trigger_el))
        .content(move |state, window, cx| {
            div()
                .min_w(px(180.0))
                .max_h(px(280.0))
                .overflow_y_hidden()
                .p(px(6.0))
                .rounded(radius::ROW)
                .bg(surface)
                .border_1()
                .border_color(hairline)
                .shadow(shadow.clone())
                .child(content(state, window, cx))
        })
}

/// One option inside a dropdown or popup menu.
pub fn menu_item(
    id: impl Into<ElementId>,
    label: impl Into<String>,
    icon_path: Option<&'static str>,
    selected: bool,
    danger: bool,
    p: &Palette,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let text_color = if danger { p.poor } else { p.text };
    let icon_color = if danger {
        p.poor
    } else if selected {
        p.accent
    } else {
        p.muted
    };
    let hover = p.hover;
    let bg = if selected {
        p.accent_soft()
    } else {
        transparent_black()
    };

    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .w_full()
        .px(px(10.0))
        .py(px(6.0))
        .rounded(radius::CONTROL)
        .bg(bg)
        .text_size(size::BODY)
        .text_color(text_color)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .on_click(on_click)
        .when_some(icon_path, |el, path| {
            el.child(icon_svg(path, px(15.0), icon_color))
        })
        .child(div().flex_1().truncate().child(label.into()))
        .when(selected, |el| {
            el.child(icon_svg(icons::CHECK, px(14.0), p.accent))
        })
}

/// Hairline separator for menus and panels.
pub fn menu_separator(p: &Palette) -> Div {
    div().w_full().h(px(1.0)).my(px(4.0)).bg(p.hairline)
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
