//! A session tab: the remote desktop, the floating session bar, and the connect / reconnect /
//! disconnected states drawn over it.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::popover::Popover;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    Anchor, AnyElement, App, Bounds, Context, Corners, CursorStyle, Element, ElementId,
    FontWeight, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, InteractiveElement,
    IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, PathBuilder, Pixels, ScrollEvent, SharedString,
    StatefulInteractiveElement, Style, Styled, Window, canvas, div, fill, point,
    prelude::FluentBuilder as _, px, relative, rgba, size as gpui_size, white,
};
use ironrdp_input::Scancode;

use super::RelayApp;
use super::assets::{icon_svg, icons};
use super::tab::{Phase, SessionTab, StageState, TabId};
use super::theme::{FONT_MONO, Palette, radius, size};
use super::widgets::{self, Signal};
use crate::app::gpu::FrameImage;
use crate::app::input::RemoteViewport;
use crate::app::keymap::codes;
use crate::session::errors::{Failure, FailureKind};
use crate::session::{CertificateInfo, Ended, Engine, Stage};

/// Height of the strip at the top of the desktop that reveals the session bar.
pub const EDGE_ZONE: f32 = 4.0;
pub const EDGE_DWELL: Duration = Duration::from_millis(300);
pub const BAR_LINGER: Duration = Duration::from_millis(1500);

type ViewportRecord = Option<(Bounds<Pixels>, Option<RemoteViewport>)>;

/// Shared slot populated by `RemoteDesktopElement::prepaint` with the latest laid-out bounds.
#[derive(Clone, Default)]
pub struct ViewportSlot(pub Rc<RefCell<ViewportRecord>>);

/// Custom GPUI Element that paints the remote desktop `RenderImage` (pixel-exact 1:1 when it
/// fits, aspect-fitted otherwise), dims under overlays, and draws any remote bitmap cursor.
struct RemoteDesktopElement {
    frame: Option<FrameImage>,
    live: bool,
    dimmed: bool,
    pointer_over_remote: bool,
    viewport_slot: ViewportSlot,
}

impl IntoElement for RemoteDesktopElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RemoteDesktopElement {
    type RequestLayoutState = ();
    type PrepaintState = (Hitbox, Option<Bounds<Pixels>>);

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name("remote-desktop-canvas".into()))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: gpui_size(relative(1.0).into(), relative(1.0).into()),
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let scale = window.scale_factor();
        let img_bounds = self
            .frame
            .as_ref()
            .map(|f| place_image(bounds, f.width, f.height, scale));
        let vp = if self.live {
            self.frame.as_ref().zip(img_bounds).map(|(f, rect)| RemoteViewport {
                rect,
                remote_width: f.width as u16,
                remote_height: f.height as u16,
            })
        } else {
            None
        };
        *self.viewport_slot.0.borrow_mut() = Some((bounds, vp));
        (hitbox, img_bounds)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let (hitbox, img_bounds) = prepaint;

        if self.live && self.pointer_over_remote {
            window.set_cursor_style(CursorStyle::Arrow, hitbox);
        }

        if let (Some(frame), Some(rect)) = (&self.frame, *img_bounds) {
            let _ = window.paint_image(
                rect,
                rect,
                Corners::all(px(0.0)),
                frame.image.clone(),
                0,
                false,
            );
            if self.dimmed {
                window.paint_quad(fill(bounds, rgba(0x000000B8)));
            }
        }
    }
}

/// 1:1 (pixel-exact) when the desktop fits, otherwise scaled down to fit.
/// The origin is snapped to the physical pixel grid so 1:1 stays crisp.
pub fn place_image(area: Bounds<Pixels>, width: u32, height: u32, scale: f32) -> Bounds<Pixels> {
    let area_w = f32::from(area.size.width);
    let area_h = f32::from(area.size.height);
    let native_w = width as f32 / scale;
    let native_h = height as f32 / scale;
    let fits = native_w <= area_w + 0.5 && native_h <= area_h + 0.5;
    let (w, h) = if fits {
        (native_w, native_h)
    } else {
        let factor = (area_w / native_w).min(area_h / native_h);
        (native_w * factor, native_h * factor)
    };
    let ox = f32::from(area.origin.x) + (area_w - w) / 2.0;
    let oy = f32::from(area.origin.y) + (area_h - h) / 2.0;
    let snapped_x = (ox * scale).round() / scale;
    let snapped_y = (oy * scale).round() / scale;
    Bounds::new(point(px(snapped_x), px(snapped_y)), gpui_size(px(w), px(h)))
}

pub fn render_session(
    this: &mut RelayApp,
    tab_id: TabId,
    p: &Palette,
    window: &mut Window,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let fullscreen = window.is_fullscreen();
    let others: Vec<(TabId, String)> = this
        .tabs
        .iter()
        .map(|t| (t.id, t.title().to_owned()))
        .collect();

    let Some(tab_idx) = this.tabs.iter().position(|t| t.id == tab_id) else {
        return div().flex_1().bg(p.canvas).into_any_element();
    };

    let session_id = this.tabs[tab_idx].handle.id;
    let is_live = this.tabs[tab_idx].is_live();
    let was_live = this.tabs[tab_idx].was_live;
    let show_picture = was_live && !matches!(this.tabs[tab_idx].phase, Phase::Connecting);
    let frame = if show_picture {
        this.textures.get(session_id)
    } else {
        None
    };

    let pointer_over_remote = this.pointer_over_remote;
    let viewport_slot = this.viewport_slot.clone();

    let mut container = div()
        .id(("session-surface", tab_id.raw()))
        .relative()
        .flex_1()
        .min_h_0()
        .w_full()
        .h_full()
        .bg(p.canvas)
        .overflow_hidden()
        .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
            this.on_session_mouse_move(tab_id, event.position, cx);
        }))
        .on_scroll(cx.listener(move |this, event: &ScrollEvent, _window, _cx| {
            this.on_session_scroll(tab_id, event);
        }));

    for btn in MouseButton::all() {
        container = container
            .on_mouse_down(
                btn,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.on_session_mouse_button(tab_id, event.button, event.position, true, window, cx);
                }),
            )
            .on_mouse_up(
                btn,
                cx.listener(move |this, event: &MouseUpEvent, window, cx| {
                    this.on_session_mouse_button(tab_id, event.button, event.position, false, window, cx);
                }),
            );
    }

    // Remote desktop picture layer
    container = container.child(
        div()
            .absolute()
            .inset_0()
            .child(RemoteDesktopElement {
                frame,
                live: is_live,
                dimmed: !is_live || this.tabs[tab_idx].cert_prompt.is_some(),
                pointer_over_remote,
                viewport_slot,
            }),
    );

    // Overlay cards or floating session bar
    let tab = &this.tabs[tab_idx];
    if tab.cert_prompt.is_some() {
        container = container.child(render_certificate_card(tab_id, tab, p, cx));
    } else {
        match &tab.phase {
            Phase::Connecting => {
                container = container.child(render_connect_card(
                    tab_id,
                    tab,
                    None,
                    this.failure_details_open,
                    p,
                    cx,
                ));
            }
            Phase::Live => {
                if let Some(bar) = render_session_bar(tab_id, tab, fullscreen, &others, p, cx) {
                    container = container.child(bar);
                }
            }
            Phase::Reconnecting {
                attempt,
                retry_at,
                reason,
            } => {
                container = container.child(render_reconnect_card(
                    tab_id, *attempt, *retry_at, reason, p, cx,
                ));
            }
            Phase::Ended(ended) => {
                if tab.was_live {
                    container = container.child(render_ended_card(
                        tab_id,
                        tab,
                        ended,
                        this.failure_details_open,
                        p,
                        cx,
                    ));
                } else {
                    let failure = match ended {
                        Ended::Failed(f) => Some(f.clone()),
                        Ended::ServerEnded(reason) => Some(Failure {
                            kind: FailureKind::Server,
                            title: "The computer ended the connection".to_owned(),
                            hint: Some(reason.clone()),
                            detail: String::new(),
                        }),
                        Ended::Closed => None,
                    };
                    container = container.child(render_connect_card(
                        tab_id,
                        tab,
                        failure.as_ref(),
                        this.failure_details_open,
                        p,
                        cx,
                    ));
                }
            }
        }
    }

    container.into_any_element()
}

fn centered_card(width: f32, p: &Palette, child: impl IntoElement) -> AnyElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .flex()
                .flex_col()
                .w(px(width))
                .p(px(28.0))
                .rounded(radius::PANEL)
                .bg(p.surface)
                .border_1()
                .border_color(p.hairline)
                .shadow(p.window_shadow())
                .child(child),
        )
        .into_any_element()
}

fn host_heading(tab: &SessionTab, p: &Palette) -> AnyElement {
    let account = tab.profile.account_label();
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .text_size(size::HEADING)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text)
                .child(tab.title().to_owned()),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .font_family(FONT_MONO)
                        .text_size(px(13.0))
                        .text_color(p.muted)
                        .child(tab.profile.endpoint()),
                )
                .when(!account.is_empty(), |el| {
                    el.child(
                        div()
                            .text_size(size::SMALL)
                            .text_color(p.muted)
                            .child(format!("as {account}")),
                    )
                }),
        )
        .into_any_element()
}

fn render_connect_card(
    tab_id: TabId,
    tab: &SessionTab,
    failure: Option<&Failure>,
    details_open: bool,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let mut body = div()
        .flex()
        .flex_col()
        .gap(px(20.0))
        .child(host_heading(tab, p))
        .child(render_stage_track(&tab.stages, p));

    match failure {
        None => {
            body = body.child(widgets::button_row().child(widgets::secondary_button(
                "connect-cancel",
                "Cancel",
                p,
                cx.listener(move |this, _, _, cx| {
                    this.disconnect_tab(tab_id);
                    cx.notify();
                }),
            )));
        }
        Some(failure) => {
            body = body
                .child(render_failure_text(failure, details_open, p, cx))
                .child(
                    widgets::button_row()
                        .child(widgets::secondary_button(
                            "connect-close",
                            "Close",
                            p,
                            cx.listener(move |this, _, window, cx| {
                                this.close_tab(tab_id, window);
                                cx.notify();
                            }),
                        ))
                        .when(tab.saved, |el| {
                            el.child(widgets::secondary_button(
                                "connect-edit",
                                "Edit host",
                                p,
                                cx.listener(move |this, _, window, cx| {
                                    this.edit_tab_host(tab_id, window, cx);
                                }),
                            ))
                        })
                        .child(if failure.kind == FailureKind::Credentials {
                            widgets::primary_button(
                                "connect-change-creds",
                                "Change sign-in",
                                p,
                                false,
                                cx.listener(move |this, _, window, cx| {
                                    this.change_tab_credentials(tab_id, window, cx);
                                }),
                            )
                        } else {
                            widgets::primary_button(
                                "connect-retry",
                                "Try again",
                                p,
                                false,
                                cx.listener(move |this, _, window, cx| {
                                    this.reconnect_tab(tab_id, window, cx);
                                }),
                            )
                        }),
                );
        }
    }

    centered_card(576.0, p, body)
}

fn render_failure_text(
    failure: &Failure,
    details_open: bool,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(icon_svg(icons::WARNING_CIRCLE, px(20.0), p.poor))
                .child(
                    div()
                        .text_size(size::SUBHEAD)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text)
                        .child(failure.title.clone()),
                ),
        )
        .when_some(failure.hint.as_ref(), |el, hint| {
            el.child(
                div()
                    .text_size(size::BODY)
                    .text_color(p.muted)
                    .child(hint.clone()),
            )
        })
        .when(!failure.detail.is_empty(), |el| {
            el.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .mt(px(4.0))
                    .child(
                        div()
                            .id("toggle-failure-details")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(4.0))
                            .text_size(size::SMALL)
                            .text_color(p.muted)
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.failure_details_open = !this.failure_details_open;
                                cx.notify();
                            }))
                            .child(icon_svg(icons::CARET_DOWN, px(12.0), p.muted))
                            .child("Technical details"),
                    )
                    .when(details_open, |el| {
                        el.child(
                            div()
                                .p(px(10.0))
                                .rounded(radius::CONTROL)
                                .bg(p.canvas)
                                .font_family(FONT_MONO)
                                .text_size(px(11.5))
                                .text_color(p.muted)
                                .child(failure.detail.clone()),
                        )
                    }),
            )
        })
        .into_any_element()
}

fn render_stage_track(stages: &[StageState; 4], p: &Palette) -> AnyElement {
    let stages = *stages;
    let palette = *p;
    let elapsed_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f32();

    div()
        .relative()
        .w_full()
        .h(px(74.0))
        // Connector lines + active spinner arcs painted via canvas
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let n = Stage::ALL.len();
                    let step = f32::from(bounds.size.width) / n as f32;
                    let r = 14.0_f32;
                    let cy = f32::from(bounds.origin.y) + r + 2.0;

                    for (i, state) in stages.iter().enumerate().take(n) {
                        let cx_pos = f32::from(bounds.origin.x) + step * (i as f32 + 0.5);
                        if i + 1 < n {
                            let next_x = f32::from(bounds.origin.x) + step * (i as f32 + 1.5);
                            let done = matches!(state, StageState::Done(_));
                            let color = if done {
                                palette.good.opacity(0.7)
                            } else {
                                palette.hairline
                            };
                            let mut line = PathBuilder::stroke(px(2.0));
                            line.move_to(point(px(cx_pos + r + 6.0), px(cy)));
                            line.line_to(point(px(next_x - r - 6.0), px(cy)));
                            if let Ok(path) = line.build() {
                                window.paint_path(path, color);
                            }
                        }

                        if matches!(state, StageState::Active(_)) {
                            let start = elapsed_secs * 4.0;
                            let mut arc = PathBuilder::stroke(px(2.5));
                            for k in 0..=16 {
                                let a = start + k as f32 / 16.0 * std::f32::consts::FRAC_PI_2 * 1.5;
                                let pt = point(px(cx_pos + a.cos() * r), px(cy + a.sin() * r));
                                if k == 0 {
                                    arc.move_to(pt);
                                } else {
                                    arc.line_to(pt);
                                }
                            }
                            if let Ok(path) = arc.build() {
                                window.paint_path(path, palette.accent);
                            }
                        }
                    }
                },
            )
            .absolute()
            .inset_0(),
        )
        // Step circles, labels and timings
        .child(
            div()
                .flex()
                .flex_row()
                .w_full()
                .h_full()
                .children(Stage::ALL.iter().enumerate().map(|(i, stage)| {
                    let st = stages[i];
                    let circle = match st {
                        StageState::Pending => div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(28.0))
                            .rounded_full()
                            .border_1()
                            .border_color(p.hairline)
                            .text_size(px(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.muted)
                            .child((i + 1).to_string()),
                        StageState::Active(_) => div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(28.0))
                            .rounded_full()
                            .border_1()
                            .border_color(p.accent.opacity(0.3))
                            .text_size(px(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.accent)
                            .child((i + 1).to_string()),
                        StageState::Done(_) => div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(28.0))
                            .rounded_full()
                            .bg(p.good)
                            .child(icon_svg(icons::CHECK, px(14.0), p.on_accent)),
                        StageState::Failed => div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(28.0))
                            .rounded_full()
                            .bg(p.poor)
                            .child(icon_svg(icons::X, px(14.0), white())),
                    };

                    let label_color = if matches!(st, StageState::Pending) {
                        p.muted
                    } else {
                        p.text
                    };
                    let timing = match st {
                        StageState::Done(d) => format_duration(d),
                        StageState::Active(since) => format_duration(since.elapsed()),
                        _ => String::new(),
                    };

                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .items_center()
                        .pt(px(2.0))
                        .gap(px(6.0))
                        .child(circle)
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(label_color)
                                .child(stage.label()),
                        )
                        .child(
                            div()
                                .h(px(14.0))
                                .font_family(FONT_MONO)
                                .text_size(px(11.5))
                                .text_color(p.muted)
                                .child(timing),
                        )
                })),
        )
        .into_any_element()
}

fn render_certificate_card(
    tab_id: TabId,
    tab: &SessionTab,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let Some(prompt) = &tab.cert_prompt else {
        return div().into_any_element();
    };
    let info: &CertificateInfo = &prompt.info;
    let changed = info.previous_fingerprint.is_some();

    let (icon_path, color, title) = if changed {
        (
            icons::SHIELD_WARNING,
            p.poor,
            "This computer's identity has changed",
        )
    } else {
        (
            icons::SHIELD_CHECK,
            p.accent,
            "Check this computer's identity",
        )
    };
    let explanation = if changed {
        "The certificate is different from the one you trusted before. This happens when the \
         computer was reinstalled or its certificate renewed, but it can also mean someone is \
         intercepting the connection. Continue only if you expected this change."
    } else {
        "This is the first time you're connecting to this computer. Compare the fingerprint \
         with the one your administrator gave you, then trust it to continue. You won't be \
         asked again unless it changes."
    };

    let cert_row = |label: &'static str, value: String, mono: bool| {
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(16.0))
            .child(
                div()
                    .w(px(108.0))
                    .flex_shrink_0()
                    .text_size(size::SMALL)
                    .text_color(p.muted)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .when(mono, |el| el.font_family(FONT_MONO).text_size(size::SMALL))
                    .when(!mono, |el| el.text_size(size::BODY))
                    .text_color(p.text)
                    .child(value),
            )
    };

    let mut rows = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(cert_row(
            "Computer",
            format!("{}:{}", info.host, info.port),
            true,
        ))
        .child(cert_row("Issued to", info.subject.clone(), false))
        .child(cert_row("Issued by", info.issuer.clone(), false))
        .child(cert_row("Expires", info.not_after.clone(), false))
        .child(cert_row(
            "Fingerprint",
            split_fingerprint(&info.fingerprint),
            true,
        ));

    if let Some(prev) = &info.previous_fingerprint {
        rows = rows.child(cert_row("Trusted before", split_fingerprint(prev), true));
    }

    let trust_btn = if changed {
        widgets::danger_button(
            "cert-trust",
            "Trust the new certificate",
            p,
            cx.listener(move |this, _, _, cx| {
                this.answer_tab_certificate(tab_id, true);
                cx.notify();
            }),
        )
    } else {
        widgets::primary_button(
            "cert-trust",
            "Trust and connect",
            p,
            false,
            cx.listener(move |this, _, _, cx| {
                this.answer_tab_certificate(tab_id, true);
                cx.notify();
            }),
        )
    };

    let body = div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .child(icon_svg(icon_path, px(24.0), color))
                .child(
                    div()
                        .text_size(size::SUBHEAD)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text)
                        .child(title),
                ),
        )
        .child(
            div()
                .text_size(size::BODY)
                .text_color(p.muted)
                .child(explanation),
        )
        .child(div().my(px(4.0)).child(rows))
        .child(
            div().mt(px(8.0)).child(
                widgets::button_row()
                    .child(widgets::secondary_button(
                        "cert-cancel",
                        "Cancel",
                        p,
                        cx.listener(move |this, _, _, cx| {
                            this.answer_tab_certificate(tab_id, false);
                            cx.notify();
                        }),
                    ))
                    .child(trust_btn),
            ),
        );

    centered_card(576.0, p, body)
}

fn render_reconnect_card(
    tab_id: TabId,
    attempt: u32,
    retry_at: Instant,
    reason: &str,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let wait = retry_at.saturating_duration_since(Instant::now());
    let next = if wait.is_zero() {
        "Trying now".to_owned()
    } else {
        format!("Next try in {} s", wait.as_secs() + 1)
    };

    let body = div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .child(Spinner::new())
                .child(
                    div()
                        .text_size(size::SUBHEAD)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text)
                        .child("Reconnecting…"),
                ),
        )
        .child(
            div()
                .text_size(size::BODY)
                .text_color(p.muted)
                .child(format!(
                    "{reason}. Your session is kept on the remote computer."
                )),
        )
        .child(
            div()
                .text_size(size::SMALL)
                .text_color(p.muted)
                .child(format!("Attempt {attempt}. {next}.")),
        )
        .child(
            div().mt(px(12.0)).child(
                widgets::button_row()
                    .child(widgets::secondary_button(
                        "reconn-disconnect",
                        "Disconnect",
                        p,
                        cx.listener(move |this, _, _, cx| {
                            this.disconnect_tab(tab_id);
                            cx.notify();
                        }),
                    ))
                    .child(widgets::primary_button(
                        "reconn-retry",
                        "Retry now",
                        p,
                        false,
                        cx.listener(move |this, _, _, _cx| {
                            this.retry_tab_now(tab_id);
                        }),
                    )),
            ),
        );

    centered_card(476.0, p, body)
}

fn render_ended_card(
    tab_id: TabId,
    tab: &SessionTab,
    ended: &Ended,
    details_open: bool,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let header: AnyElement = match ended {
        Ended::ServerEnded(reason) => div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(icon_svg(icons::PLUGS, px(22.0), p.muted))
                    .child(
                        div()
                            .text_size(size::SUBHEAD)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text)
                            .child("Session ended"),
                    ),
            )
            .child(
                div()
                    .text_size(size::BODY)
                    .text_color(p.muted)
                    .child(reason.clone()),
            )
            .into_any_element(),
        Ended::Failed(failure) => render_failure_text(failure, details_open, p, cx),
        Ended::Closed => div().into_any_element(),
    };

    let primary = if tab.failure_kind() == Some(FailureKind::Credentials) {
        widgets::primary_button(
            "ended-change-creds",
            "Change sign-in",
            p,
            false,
            cx.listener(move |this, _, window, cx| {
                this.change_tab_credentials(tab_id, window, cx);
            }),
        )
    } else {
        widgets::primary_button(
            "ended-reconnect",
            "Reconnect",
            p,
            false,
            cx.listener(move |this, _, window, cx| {
                this.reconnect_tab(tab_id, window, cx);
            }),
        )
    };

    let body = div()
        .flex()
        .flex_col()
        .gap(px(18.0))
        .child(header)
        .child(
            widgets::button_row()
                .child(widgets::secondary_button(
                    "ended-close",
                    "Close tab",
                    p,
                    cx.listener(move |this, _, window, cx| {
                        this.close_tab(tab_id, window);
                        cx.notify();
                    }),
                ))
                .child(primary),
        );

    centered_card(496.0, p, body)
}

fn render_session_bar(
    tab_id: TabId,
    tab: &SessionTab,
    fullscreen: bool,
    all_tabs: &[(TabId, String)],
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> Option<AnyElement> {
    let now = Instant::now();
    let visible = tab.bar.engaged || tab.bar.visible_until.is_some_and(|until| now < until);
    if !visible {
        return None;
    }

    let signal = tab.current_rtt().map_or(Signal::Unknown, Signal::Rtt);
    let history: Vec<f32> = tab.rtt_history.iter().copied().collect();
    let stats = tab.stats;
    let palette = *p;
    let entity = cx.entity().clone();

    let sparkline_box = div()
        .id(("bar-sparkline", tab_id.raw()))
        .flex()
        .items_center()
        .tooltip(move |window, cx| {
            Tooltip::element(move |_window, _cx| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(size::SMALL)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(palette.text)
                            .child("Round-trip time, last minute"),
                    )
                    .child(
                        div()
                            .font_family(FONT_MONO)
                            .text_size(px(11.5))
                            .text_color(palette.muted)
                            .child(format!(
                                "{:.0} screen updates/s\n{} in, {} out per second",
                                stats.updates_per_sec,
                                format_rate(stats.bytes_in_per_sec),
                                format_rate(stats.bytes_out_per_sec),
                            )),
                    )
            })
            .build(window, cx)
        })
        .child(widgets::sparkline(
            history,
            px(56.0),
            px(16.0),
            signal.color(p),
        ));

    let rendering_el = tab.rendering.as_ref().map(|rendering| {
        let (fill_c, text_c) = match rendering.engine {
            Engine::Gpu => (p.accent_soft(), p.accent),
            Engine::Cpu => (p.raised, p.muted),
        };
        let details = rendering.details();
        let fallback = rendering.fallback.clone();
        div()
            .id(("bar-render-badge", tab_id.raw()))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .ml(px(4.0))
            .tooltip(move |window, cx| {
                let fallback = fallback.clone();
                Tooltip::element(move |_window, _cx| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(size::SMALL)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(palette.text)
                                .child("Rendering"),
                        )
                        .child(
                            div()
                                .text_size(size::BODY)
                                .text_color(palette.text)
                                .child(details),
                        )
                        .when_some(fallback.clone(), |el, reason| {
                            el.child(
                                div()
                                    .text_size(size::SMALL)
                                    .text_color(palette.muted)
                                    .child(format!("Video decoding: {reason}")),
                            )
                        })
                })
                .build(window, cx)
            })
            .child(widgets::tag(rendering.badge(), fill_c, text_c))
            .child(
                div()
                    .text_size(size::SMALL)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.text)
                    .child(rendering.codec_label()),
            )
    });

    let switch_menu = if fullscreen && !all_tabs.is_empty() {
        let tabs_list = all_tabs.to_vec();
        let e_sw = entity.clone();
        let raised = p.raised;
        let surface = p.surface;
        let hairline = p.hairline;
        let muted = p.muted;
        let shadow = p.popup_shadow();

        Some(
            Popover::new(("bar-switch", tab_id.raw()))
                .anchor(Anchor::TopRight)
                .appearance(false)
                .on_open_change(cx.listener(move |this, open: &bool, _, cx| {
                    this.set_session_bar_menu_open(tab_id, *open);
                    cx.notify();
                }))
                .trigger(widgets::PopoverTrigger::new(
                    ("bar-switch-btn", tab_id.raw()),
                    div()
                        .id(("bar-switch-tip", tab_id.raw()))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(30.0))
                        .rounded(radius::CONTROL)
                        .cursor_pointer()
                        .hover(move |s| s.bg(raised))
                        .tooltip(|window, cx| Tooltip::new("Switch session").build(window, cx))
                        .child(icon_svg(icons::STACK, px(16.0), muted)),
                ))
                .content(move |_state, _window, cx| {
                    let se = cx.entity().clone();
                    let mut list = div()
                        .flex()
                        .flex_col()
                        .min_w(px(200.0))
                        .p(px(6.0))
                        .rounded(radius::ROW)
                        .bg(surface)
                        .border_1()
                        .border_color(hairline)
                        .shadow(shadow.clone());

                    let e_home = e_sw.clone();
                    let se_h = se.clone();
                    list = list
                        .child(widgets::menu_item(
                            "bar-go-home",
                            "Hosts",
                            Some(icons::SQUARES_FOUR),
                            false,
                            false,
                            &palette,
                            move |_, window, cx| {
                                e_home.update(cx, |app, cx| {
                                    app.active = super::ActiveTab::Home;
                                    cx.notify();
                                });
                                se_h.update(cx, |s, cx| s.dismiss(window, cx));
                            },
                        ))
                        .child(widgets::menu_separator(&palette));

                    for (id, title) in &tabs_list {
                        let id = *id;
                        let current = id == tab_id;
                        let e_t = e_sw.clone();
                        let se_t = se.clone();
                        list = list.child(widgets::menu_item(
                            ("bar-tab", id.raw()),
                            title.clone(),
                            None,
                            current,
                            false,
                            &palette,
                            move |_, window, cx| {
                                if !current {
                                    e_t.update(cx, |app, cx| {
                                        app.active = super::ActiveTab::Session(id);
                                        cx.notify();
                                    });
                                }
                                se_t.update(cx, |s, cx| s.dismiss(window, cx));
                            },
                        ));
                    }
                    list
                }),
        )
    } else {
        None
    };

    let keys_menu = {
        let e_keys = entity.clone();
        let raised = p.raised;
        let surface = p.surface;
        let hairline = p.hairline;
        let muted = p.muted;
        let shadow = p.popup_shadow();

        Popover::new(("bar-keys", tab_id.raw()))
            .anchor(Anchor::TopRight)
            .appearance(false)
            .on_open_change(cx.listener(move |this, open: &bool, _, cx| {
                this.set_session_bar_menu_open(tab_id, *open);
                cx.notify();
            }))
            .trigger(widgets::PopoverTrigger::new(
                ("bar-keys-btn", tab_id.raw()),
                div()
                    .id(("bar-keys-tip", tab_id.raw()))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(30.0))
                    .rounded(radius::CONTROL)
                    .cursor_pointer()
                    .hover(move |s| s.bg(raised))
                    .tooltip(|window, cx| Tooltip::new("Send keys").build(window, cx))
                    .child(icon_svg(icons::KEYBOARD, px(16.0), muted)),
            ))
            .content(move |_state, _window, cx| {
                let se = cx.entity().clone();
                let mut list = div()
                    .flex()
                    .flex_col()
                    .min_w(px(190.0))
                    .p(px(6.0))
                    .rounded(radius::ROW)
                    .bg(surface)
                    .border_1()
                    .border_color(hairline)
                    .shadow(shadow.clone());

                let e_cad = e_keys.clone();
                let se_cad = se.clone();
                list = list.child(widgets::menu_item(
                    "keys-cad",
                    "Ctrl+Alt+Del",
                    None,
                    false,
                    false,
                    &palette,
                    move |_, window, cx| {
                        e_cad.update(cx, |app, _| app.send_tab_ctrl_alt_del(tab_id));
                        se_cad.update(cx, |s, cx| s.dismiss(window, cx));
                    },
                ));

                let combos: [(&str, Vec<Scancode>); 4] = [
                    ("Windows key", vec![codes::SUPER_LEFT]),
                    ("Alt+Tab", vec![codes::ALT_LEFT, codes::TAB]),
                    (
                        "Ctrl+Shift+Esc",
                        vec![codes::CONTROL_LEFT, codes::SHIFT_LEFT, codes::ESCAPE],
                    ),
                    ("Print Screen", vec![codes::PRINT_SCREEN]),
                ];
                for (label, combo) in combos {
                    let e_c = e_keys.clone();
                    let se_c = se.clone();
                    list = list.child(widgets::menu_item(
                        SharedString::from(format!("keys-combo-{label}")),
                        label,
                        None,
                        false,
                        false,
                        &palette,
                        move |_, window, cx| {
                            let combo = combo.clone();
                            e_c.update(cx, |app, _| app.send_tab_keys(tab_id, &combo));
                            se_c.update(cx, |s, cx| s.dismiss(window, cx));
                        },
                    ));
                }
                list
            })
    };

    let (fs_icon, fs_tip) = if fullscreen {
        (icons::CORNERS_IN, "Exit fullscreen (Ctrl+Alt+Enter)")
    } else {
        (icons::CORNERS_OUT, "Fullscreen (Ctrl+Alt+Enter)")
    };

    let pill = div()
        .id(("session-bar-pill", tab_id.raw()))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .px(px(14.0))
        .py(px(6.0))
        .rounded(px(22.0))
        .bg(p.surface)
        .border_1()
        .border_color(p.hairline)
        .shadow(p.popup_shadow())
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            this.set_session_bar_hovered(tab_id, *hovered);
            cx.notify();
        }))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_move(|_, _, cx| cx.stop_propagation())
        .child(widgets::signal_bars(("bar-sig", tab_id.raw()), signal, p))
        .child(
            div()
                .text_size(size::BODY)
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.text)
                .child(tab.title().to_owned()),
        )
        .when_some(tab.current_rtt(), |el, rtt| {
            el.child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(size::SMALL)
                    .text_color(p.muted)
                    .child(format!("{} ms", rtt.as_millis().max(1))),
            )
        })
        .child(sparkline_box)
        .when_some(rendering_el, |el, badge| el.child(badge))
        // Vertical separator
        .child(div().w(px(1.0)).h(px(18.0)).mx(px(4.0)).bg(p.hairline))
        .when_some(switch_menu, |el, menu| el.child(menu))
        .child(keys_menu)
        .child(widgets::icon_button(
            ("bar-min", tab_id.raw()),
            icons::MINUS,
            "Minimize to tab",
            p,
            cx.listener(move |this, _, _, cx| {
                this.minimize_tab(tab_id);
                cx.notify();
            }),
        ))
        .child(widgets::icon_button(
            ("bar-fs", tab_id.raw()),
            fs_icon,
            fs_tip,
            p,
            cx.listener(|_this, _, window, _cx| {
                window.toggle_fullscreen();
            }),
        ))
        .child(widgets::icon_button(
            ("bar-disconnect", tab_id.raw()),
            icons::PLUGS,
            "Disconnect",
            p,
            cx.listener(move |this, _, _, cx| {
                this.disconnect_tab(tab_id);
                cx.notify();
            }),
        ));

    Some(
        div()
            .absolute()
            .top(px(10.0))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(pill)
            .into_any_element(),
    )
}

fn format_rate(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.0} kB", bytes as f64 / 1e3),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", d.as_secs_f32())
    }
}

/// SHA-256 fingerprints are long; break them into two lines of 16 bytes.
fn split_fingerprint(fp: &str) -> String {
    let parts: Vec<&str> = fp.split(':').collect();
    if parts.len() <= 16 {
        return fp.to_owned();
    }
    format!("{}\n{}", parts[..16].join(":"), parts[16..].join(":"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_image_fits_1_to_1_and_scales_down_when_larger() {
        let area = Bounds::new(point(px(0.0), px(0.0)), gpui_size(px(1920.0), px(1080.0)));
        let exact = place_image(area, 1280, 720, 1.0);
        assert_eq!(f32::from(exact.size.width), 1280.0);
        assert_eq!(f32::from(exact.size.height), 720.0);

        let scaled = place_image(area, 3840, 2160, 1.0);
        assert!((f32::from(scaled.size.width) - 1920.0).abs() < 0.5);
        assert!((f32::from(scaled.size.height) - 1080.0).abs() < 0.5);
    }
}
