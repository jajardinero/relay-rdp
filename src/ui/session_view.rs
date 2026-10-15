//! A session tab: the remote desktop, the floating session bar, and the connect / reconnect /
//! disconnected states drawn over it.

use std::time::{Duration, Instant};

use egui::{Align2, Color32, CornerRadius, Pos2, Rect, RichText, Stroke, Ui, pos2, vec2};
use egui_phosphor::regular as icon;
use egui_wgpu::wgpu::FilterMode;
use ironrdp_input::Scancode;

use super::tab::{Phase, SessionTab, StageState, TabId};
use super::theme::{self, Palette, radius, size};
use super::widgets::{self, Signal};
use crate::app::gpu::{RemoteTextures, TextureInfo};
use crate::app::input::RemoteViewport;
use crate::app::keymap::codes;
use crate::session::errors::{Failure, FailureKind};
use crate::session::{CertificateInfo, Ended, Engine, Rendering, Stage};

/// Height of the strip at the top of the desktop that reveals the session bar.
pub const EDGE_ZONE: f32 = 4.0;
const EDGE_DWELL: Duration = Duration::from_millis(300);
const BAR_LINGER: Duration = Duration::from_millis(1500);

pub enum Action {
    ToggleFullscreen,
    Disconnect,
    CloseTab,
    Reconnect,
    RetryNow,
    ChangeCredentials,
    EditHost,
    TrustCertificate(bool),
    SendCtrlAltDel,
    SendKeys(Vec<Scancode>),
    SwitchTo(TabId),
    GoHome,
}

pub struct ViewContext<'a> {
    pub pixels_per_point: f32,
    pub fullscreen: bool,
    pub pointer: Option<Pos2>,
    pub tabs: &'a [(TabId, String)],
}

#[derive(Default)]
pub struct Output {
    pub viewport: Option<RemoteViewport>,
    pub filter: Option<FilterMode>,
    pub actions: Vec<Action>,
}

pub fn show(
    ui: &mut Ui,
    tab: &mut SessionTab,
    textures: &RemoteTextures,
    cx: ViewContext<'_>,
) -> Output {
    let area = ui.max_rect();
    let mut out = Output::default();

    // The picture: live, or dimmed under an overlay once we've had one.
    let show_picture = tab.was_live && !matches!(tab.phase, Phase::Connecting);
    if show_picture && let Some(info) = textures.get(tab.handle.id) {
        let (rect, filter) = place_image(area, &info, cx.pixels_per_point);
        let tint = if tab.is_live() {
            Color32::WHITE
        } else {
            Color32::from_gray(70)
        };
        ui.painter().image(
            info.id,
            rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            tint,
        );
        out.filter = Some(filter);
        if tab.is_live() {
            out.viewport = Some(RemoteViewport {
                rect,
                remote_width: info.width as u16,
                remote_height: info.height as u16,
            });
        }
    }

    if tab.cert_prompt.is_some() {
        certificate_card(ui, area, tab, &mut out.actions);
        return out;
    }

    match &tab.phase {
        Phase::Connecting => connect_card(ui, area, tab, None, &mut out.actions),
        Phase::Live => session_bar(ui, area, tab, &cx, &mut out.actions),
        Phase::Reconnecting {
            attempt,
            retry_at,
            reason,
        } => reconnect_card(ui, area, *attempt, *retry_at, reason, &mut out.actions),
        Phase::Ended(ended) => {
            let ended = ended.clone();
            if tab.was_live {
                ended_card(ui, area, tab, &ended, &mut out.actions);
            } else {
                let failure = match &ended {
                    Ended::Failed(f) => Some(f.clone()),
                    Ended::ServerEnded(reason) => Some(Failure {
                        kind: FailureKind::Server,
                        title: "The computer ended the connection".to_owned(),
                        hint: Some(reason.clone()),
                        detail: String::new(),
                    }),
                    Ended::Closed => None,
                };
                connect_card(ui, area, tab, failure.as_ref(), &mut out.actions);
            }
        }
    }
    out
}

/// 1:1 (pixel-exact, nearest filtering) when the desktop fits, otherwise scaled down to fit.
/// The origin is snapped to the physical pixel grid so 1:1 stays crisp.
fn place_image(area: Rect, info: &TextureInfo, ppp: f32) -> (Rect, FilterMode) {
    let native = vec2(info.width as f32, info.height as f32) / ppp;
    let fits = native.x <= area.width() + 0.5 && native.y <= area.height() + 0.5;
    let size = if fits {
        native
    } else {
        native * (area.width() / native.x).min(area.height() / native.y)
    };
    let min = area.center() - size / 2.0;
    let min = pos2((min.x * ppp).round() / ppp, (min.y * ppp).round() / ppp);
    let filter = if fits {
        FilterMode::Nearest
    } else {
        FilterMode::Linear
    };
    (Rect::from_min_size(min, size), filter)
}

/// A centered card on the canvas.
fn card<R>(ui: &mut Ui, area: Rect, width: f32, id: &str, content: impl FnOnce(&mut Ui) -> R) -> R {
    let p = Palette::of(ui);
    egui::Area::new(egui::Id::new(id))
        .order(egui::Order::Middle)
        .fixed_pos(area.center())
        .pivot(Align2::CENTER_CENTER)
        .constrain_to(area)
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.hairline))
                .corner_radius(CornerRadius::same(radius::PANEL))
                .inner_margin(egui::Margin::same(28))
                .shadow(ui.visuals().window_shadow)
                .show(ui, |ui| {
                    ui.set_width(width);
                    content(ui)
                })
                .inner
        })
        .inner
}

fn host_heading(ui: &mut Ui, tab: &SessionTab) {
    let p = Palette::of(ui);
    ui.label(RichText::new(tab.title()).font(theme::font_semibold(size::HEADING)));
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(tab.profile.endpoint())
                .font(theme::mono(13.0))
                .color(p.muted),
        );
        let account = tab.profile.account_label();
        if !account.is_empty() {
            ui.label(
                RichText::new(format!("as {account}"))
                    .size(size::SMALL)
                    .color(p.muted),
            );
        }
    });
}

fn connect_card(
    ui: &mut Ui,
    area: Rect,
    tab: &SessionTab,
    failure: Option<&Failure>,
    actions: &mut Vec<Action>,
) {
    let p = Palette::of(ui);
    card(ui, area, 520.0, "connect-card", |ui| {
        host_heading(ui, tab);
        ui.add_space(28.0);
        stage_track(ui, &tab.stages);
        ui.add_space(24.0);

        match failure {
            None => {
                widgets::button_row(ui, |ui| {
                    if widgets::secondary_button(ui, "Cancel").clicked() {
                        actions.push(Action::Disconnect);
                    }
                });
            }
            Some(failure) => {
                failure_text(ui, failure, &p);
                ui.add_space(18.0);
                widgets::button_row(ui, |ui| {
                    if failure.kind == FailureKind::Credentials {
                        if widgets::primary_button(ui, "Change sign-in").clicked() {
                            actions.push(Action::ChangeCredentials);
                        }
                    } else if widgets::primary_button(ui, "Try again").clicked() {
                        actions.push(Action::Reconnect);
                    }
                    if tab.saved && widgets::secondary_button(ui, "Edit host").clicked() {
                        actions.push(Action::EditHost);
                    }
                    if widgets::secondary_button(ui, "Close").clicked() {
                        actions.push(Action::CloseTab);
                    }
                });
            }
        }
    });
}

fn failure_text(ui: &mut Ui, failure: &Failure, p: &Palette) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon::WARNING_CIRCLE).size(20.0).color(p.poor));
        ui.label(RichText::new(&failure.title).font(theme::font_semibold(size::SUBHEAD)));
    });
    if let Some(hint) = &failure.hint {
        ui.add_space(2.0);
        ui.label(RichText::new(hint).color(p.muted));
    }
    if !failure.detail.is_empty() {
        ui.add_space(6.0);
        egui::CollapsingHeader::new(
            RichText::new("Technical details")
                .size(size::SMALL)
                .color(p.muted),
        )
        .id_salt("failure-details")
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(&failure.detail)
                        .font(theme::mono(11.5))
                        .color(p.muted),
                )
                .selectable(true)
                .wrap(),
            );
        });
    }
}

/// The four connection steps as a horizontal track. It is a real sequence, so the steps are
/// numbered; each shows how long it took.
fn stage_track(ui: &mut Ui, stages: &[StageState; 4]) {
    let p = Palette::of(ui);
    let height = 72.0;
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), egui::Sense::hover());
    let painter = ui.painter_at(rect.expand(4.0));
    let n = Stage::ALL.len();
    let step = rect.width() / n as f32;
    let radius = 14.0;
    let cy = rect.top() + radius + 2.0;
    let time = ui.input(|i| i.time) as f32;
    let mut animating = false;

    for (i, stage) in Stage::ALL.iter().enumerate() {
        let cx = rect.left() + step * (i as f32 + 0.5);
        let center = pos2(cx, cy);

        if i + 1 < n {
            let next = pos2(rect.left() + step * (i as f32 + 1.5), cy);
            let done = matches!(stages[i], StageState::Done(_));
            painter.line_segment(
                [
                    center + vec2(radius + 6.0, 0.0),
                    next - vec2(radius + 6.0, 0.0),
                ],
                Stroke::new(
                    2.0,
                    if done {
                        p.good.gamma_multiply(0.7)
                    } else {
                        p.hairline
                    },
                ),
            );
        }

        match stages[i] {
            StageState::Pending => {
                painter.circle_stroke(center, radius, Stroke::new(1.5, p.hairline));
                painter.text(
                    center,
                    Align2::CENTER_CENTER,
                    (i + 1).to_string(),
                    theme::font_medium(13.0),
                    p.muted,
                );
            }
            StageState::Active(_) => {
                animating = true;
                painter.circle_stroke(
                    center,
                    radius,
                    Stroke::new(1.5, p.accent.gamma_multiply(0.3)),
                );
                // A quarter arc chasing around the ring.
                let start = time * 4.0;
                let points: Vec<Pos2> = (0..=16)
                    .map(|k| {
                        let a = start + k as f32 / 16.0 * std::f32::consts::FRAC_PI_2 * 1.5;
                        center + vec2(a.cos(), a.sin()) * radius
                    })
                    .collect();
                painter.add(egui::Shape::line(points, Stroke::new(2.5, p.accent)));
                painter.text(
                    center,
                    Align2::CENTER_CENTER,
                    (i + 1).to_string(),
                    theme::font_medium(13.0),
                    p.accent,
                );
            }
            StageState::Done(_) => {
                painter.circle_filled(center, radius, p.good);
                painter.text(
                    center,
                    Align2::CENTER_CENTER,
                    icon::CHECK,
                    theme::font(14.0),
                    p.on_accent,
                );
            }
            StageState::Failed => {
                painter.circle_filled(center, radius, p.poor);
                painter.text(
                    center,
                    Align2::CENTER_CENTER,
                    icon::X,
                    theme::font(14.0),
                    Color32::WHITE,
                );
            }
        }

        let label_color = match stages[i] {
            StageState::Pending => p.muted,
            _ => p.text,
        };
        painter.text(
            pos2(cx, cy + radius + 14.0),
            Align2::CENTER_CENTER,
            stage.label(),
            theme::font_medium(size::SMALL + 1.0),
            label_color,
        );
        let timing = match stages[i] {
            StageState::Done(d) => format_duration(d),
            StageState::Active(since) => format_duration(since.elapsed()),
            _ => String::new(),
        };
        painter.text(
            pos2(cx, cy + radius + 32.0),
            Align2::CENTER_CENTER,
            timing,
            theme::mono(11.5),
            p.muted,
        );
    }

    if animating {
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
}

/// "GPU"/"CPU" badge plus the codec, so you can tell at a glance how the picture is rendered.
fn render_badge(ui: &mut Ui, rendering: &Rendering, p: &Palette) {
    let response = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            let (fill, color) = match rendering.engine {
                Engine::Gpu => (p.accent_soft(), p.accent),
                Engine::Cpu => (p.raised, p.muted),
            };
            widgets::tag(ui, rendering.badge(), fill, color);
            ui.label(
                RichText::new(rendering.codec_label())
                    .font(theme::font_medium(size::SMALL))
                    .color(p.text),
            );
        })
        .response;
    response.on_hover_ui(|ui| {
        ui.label(RichText::new("Rendering").font(theme::font_medium(size::SMALL)));
        ui.label(rendering.details());
        if let Some(reason) = &rendering.fallback {
            ui.label(
                RichText::new(format!("Video decoding: {reason}"))
                    .size(size::SMALL)
                    .color(p.muted),
            );
        }
    });
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

fn certificate_card(ui: &mut Ui, area: Rect, tab: &SessionTab, actions: &mut Vec<Action>) {
    let p = Palette::of(ui);
    let Some(prompt) = &tab.cert_prompt else {
        return;
    };
    let info: &CertificateInfo = &prompt.info;
    let changed = info.previous_fingerprint.is_some();

    card(ui, area, 520.0, "certificate-card", |ui| {
        let (glyph, color, title) = if changed {
            (
                icon::SHIELD_WARNING,
                p.poor,
                "This computer's identity has changed",
            )
        } else {
            (
                icon::SHIELD_CHECK,
                p.accent,
                "Check this computer's identity",
            )
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(glyph).size(26.0).color(color));
            ui.label(RichText::new(title).font(theme::font_semibold(size::SUBHEAD)));
        });
        ui.add_space(6.0);
        let body = if changed {
            "The certificate is different from the one you trusted before. This happens when the \
             computer was reinstalled or its certificate renewed, but it can also mean someone is \
             intercepting the connection. Continue only if you expected this change."
        } else {
            "This is the first time you're connecting to this computer. Compare the fingerprint \
             with the one your administrator gave you, then trust it to continue. You won't be \
             asked again unless it changes."
        };
        ui.label(RichText::new(body).color(p.muted));
        ui.add_space(14.0);

        egui::Grid::new("cert-grid")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                let row = |ui: &mut Ui, label: &str, value: &str, mono: bool| {
                    ui.label(RichText::new(label).size(size::SMALL).color(p.muted));
                    let text = if mono {
                        RichText::new(value).font(theme::mono(12.0))
                    } else {
                        RichText::new(value).size(size::BODY)
                    };
                    ui.add(egui::Label::new(text).wrap());
                    ui.end_row();
                };
                row(
                    ui,
                    "Computer",
                    &format!("{}:{}", info.host, info.port),
                    true,
                );
                row(ui, "Issued to", &info.subject, false);
                row(ui, "Issued by", &info.issuer, false);
                row(ui, "Expires", &info.not_after, false);
                row(
                    ui,
                    "Fingerprint",
                    &split_fingerprint(&info.fingerprint),
                    true,
                );
                if let Some(previous) = &info.previous_fingerprint {
                    row(ui, "Trusted before", &split_fingerprint(previous), true);
                }
            });

        ui.add_space(20.0);
        widgets::button_row(ui, |ui| {
            let trust = if changed {
                widgets::danger_button(ui, "Trust the new certificate")
            } else {
                widgets::primary_button(ui, "Trust and connect")
            };
            if trust.clicked() {
                actions.push(Action::TrustCertificate(true));
            }
            if widgets::secondary_button(ui, "Cancel").clicked() {
                actions.push(Action::TrustCertificate(false));
            }
        });
    });
}

/// SHA-256 fingerprints are long; break them into two lines of 16 bytes.
fn split_fingerprint(fp: &str) -> String {
    let parts: Vec<&str> = fp.split(':').collect();
    if parts.len() <= 16 {
        return fp.to_owned();
    }
    format!("{}\n{}", parts[..16].join(":"), parts[16..].join(":"))
}

fn reconnect_card(
    ui: &mut Ui,
    area: Rect,
    attempt: u32,
    retry_at: Instant,
    reason: &str,
    actions: &mut Vec<Action>,
) {
    let p = Palette::of(ui);
    card(ui, area, 420.0, "reconnect-card", |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(18.0).color(p.fair));
            ui.label(RichText::new("Reconnecting…").font(theme::font_semibold(size::SUBHEAD)));
        });
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!(
                "{reason}. Your session is kept on the remote computer."
            ))
            .color(p.muted),
        );
        let wait = retry_at.saturating_duration_since(Instant::now());
        let next = if wait.is_zero() {
            "Trying now".to_owned()
        } else {
            format!("Next try in {} s", wait.as_secs() + 1)
        };
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!("Attempt {attempt}. {next}."))
                .size(size::SMALL)
                .color(p.muted),
        );
        ui.add_space(18.0);
        widgets::button_row(ui, |ui| {
            if widgets::primary_button(ui, "Retry now").clicked() {
                actions.push(Action::RetryNow);
            }
            if widgets::secondary_button(ui, "Disconnect").clicked() {
                actions.push(Action::Disconnect);
            }
        });
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    });
}

fn ended_card(ui: &mut Ui, area: Rect, tab: &SessionTab, ended: &Ended, actions: &mut Vec<Action>) {
    let p = Palette::of(ui);
    card(ui, area, 440.0, "ended-card", |ui| {
        match ended {
            Ended::ServerEnded(reason) => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::PLUGS).size(22.0).color(p.muted));
                    ui.label(
                        RichText::new("Session ended").font(theme::font_semibold(size::SUBHEAD)),
                    );
                });
                ui.add_space(4.0);
                ui.label(RichText::new(reason).color(p.muted));
            }
            Ended::Failed(failure) => failure_text(ui, failure, &p),
            Ended::Closed => {}
        }
        ui.add_space(18.0);
        widgets::button_row(ui, |ui| {
            if tab.failure_kind() == Some(FailureKind::Credentials) {
                if widgets::primary_button(ui, "Change sign-in").clicked() {
                    actions.push(Action::ChangeCredentials);
                }
            } else if widgets::primary_button(ui, "Reconnect").clicked() {
                actions.push(Action::Reconnect);
            }
            if widgets::secondary_button(ui, "Close tab").clicked() {
                actions.push(Action::CloseTab);
            }
        });
    });
}

/// The floating bar at the top of a live session. Hidden until the pointer rests at the top
/// edge (or Ctrl+Alt+Home), so the remote desktop keeps every pixel.
fn session_bar(
    ui: &mut Ui,
    area: Rect,
    tab: &mut SessionTab,
    cx: &ViewContext<'_>,
    actions: &mut Vec<Action>,
) {
    let ctx = ui.ctx().clone();
    let now = Instant::now();

    let near_edge = cx.pointer.is_some_and(|pos| {
        area.x_range().contains(pos.x)
            && pos.y >= area.top() - 1.0
            && pos.y <= area.top() + EDGE_ZONE
    });
    if near_edge {
        let since = *tab.bar.edge_since.get_or_insert(now);
        let waited = now.duration_since(since);
        if waited >= EDGE_DWELL {
            tab.bar.visible_until = Some(now + BAR_LINGER);
        } else {
            ctx.request_repaint_after(EDGE_DWELL - waited);
        }
    } else {
        tab.bar.edge_since = None;
    }
    if tab.bar.engaged {
        tab.bar.visible_until = Some(now + BAR_LINGER);
    }

    let visible = tab.bar.visible_until.is_some_and(|until| now < until);
    match tab.bar.visible_until {
        Some(until) if now < until => ctx.request_repaint_after(until - now),
        _ => tab.bar.visible_until = None,
    }

    let bar_id = egui::Id::new(("session-bar", tab.id));
    let t = ctx.animate_bool_with_time(bar_id, visible, 0.14);
    if t <= 0.0 {
        tab.bar.engaged = false;
        return;
    }

    let p = Palette::of(ui);
    let offset = (1.0 - t) * -14.0;
    let response = egui::Area::new(bar_id)
        .order(egui::Order::Foreground)
        .fixed_pos(pos2(area.center().x, area.top() + 10.0 + offset))
        .pivot(Align2::CENTER_TOP)
        .show(&ctx, |ui| {
            ui.set_opacity(t);
            egui::Frame::new()
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.hairline))
                .corner_radius(CornerRadius::same(22))
                .inner_margin(egui::Margin::symmetric(14, 6))
                .shadow(ui.visuals().popup_shadow)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        bar_contents(ui, tab, cx, actions);
                    });
                });
        })
        .response;

    let pointer_over = cx
        .pointer
        .is_some_and(|pos| response.rect.expand(8.0).contains(pos));
    tab.bar.engaged = pointer_over || egui::Popup::is_any_open(&ctx);
}

fn bar_contents(
    ui: &mut Ui,
    tab: &mut SessionTab,
    cx: &ViewContext<'_>,
    actions: &mut Vec<Action>,
) {
    let p = Palette::of(ui);
    let signal = tab.current_rtt().map_or(Signal::Unknown, Signal::Rtt);

    widgets::signal_bars(ui, signal);
    ui.label(RichText::new(tab.title()).font(theme::font_medium(size::BODY)));
    if let Some(rtt) = tab.current_rtt() {
        ui.label(
            RichText::new(format!("{} ms", rtt.as_millis().max(1)))
                .font(theme::mono(12.0))
                .color(p.muted),
        );
    }
    let history: Vec<f32> = tab.rtt_history.iter().copied().collect();
    let stats = tab.stats;
    widgets::sparkline(ui, &history, vec2(56.0, 16.0), signal.color(&p)).on_hover_ui(|ui| {
        ui.label(
            RichText::new("Round-trip time, last minute").font(theme::font_medium(size::SMALL)),
        );
        ui.label(
            RichText::new(format!(
                "{:.0} screen updates/s\n{} in, {} out per second",
                stats.updates_per_sec,
                format_rate(stats.bytes_in_per_sec),
                format_rate(stats.bytes_out_per_sec),
            ))
            .font(theme::mono(11.5))
            .color(p.muted),
        );
    });
    if let Some(rendering) = &tab.rendering {
        ui.add_space(4.0);
        render_badge(ui, rendering, &p);
    }

    ui.add_space(4.0);
    ui.separator();

    if cx.fullscreen && !cx.tabs.is_empty() {
        let switch = widgets::icon_button(ui, icon::STACK, "Switch session");
        egui::Popup::menu(&switch).show(|ui| {
            ui.set_min_width(200.0);
            if ui
                .button(format!("{}  Hosts", icon::SQUARES_FOUR))
                .clicked()
            {
                actions.push(Action::GoHome);
            }
            ui.separator();
            for (id, title) in cx.tabs {
                let current = *id == tab.id;
                let label = if current {
                    RichText::new(title).strong()
                } else {
                    RichText::new(title)
                };
                if ui.add_enabled(!current, egui::Button::new(label)).clicked() {
                    actions.push(Action::SwitchTo(*id));
                }
            }
        });
    }

    let keys = widgets::icon_button(ui, icon::KEYBOARD, "Send keys");
    egui::Popup::menu(&keys).show(|ui| {
        ui.set_min_width(190.0);
        if ui.button("Ctrl+Alt+Del").clicked() {
            actions.push(Action::SendCtrlAltDel);
        }
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
            if ui.button(label).clicked() {
                actions.push(Action::SendKeys(combo));
            }
        }
    });

    let (glyph, tip) = if cx.fullscreen {
        (icon::CORNERS_IN, "Exit fullscreen (Ctrl+Alt+Enter)")
    } else {
        (icon::CORNERS_OUT, "Fullscreen (Ctrl+Alt+Enter)")
    };
    if widgets::icon_button(ui, glyph, tip).clicked() {
        actions.push(Action::ToggleFullscreen);
    }

    let disconnect = widgets::icon_button(ui, icon::PLUGS, "Disconnect");
    if disconnect.clicked() {
        actions.push(Action::Disconnect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(w: u32, h: u32) -> TextureInfo {
        TextureInfo {
            id: egui::TextureId::Managed(0),
            width: w,
            height: h,
        }
    }

    #[test]
    fn exact_fit_is_one_to_one() {
        let area = Rect::from_min_size(pos2(0.0, 44.0), vec2(1280.0, 756.0));
        let (rect, filter) = place_image(area, &info(2560, 1512), 2.0);
        assert_eq!(filter, FilterMode::Nearest);
        assert_eq!(rect, area);
    }

    #[test]
    fn oversized_desktops_scale_down_and_center() {
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 1000.0));
        let (rect, filter) = place_image(area, &info(2000, 1000), 1.0);
        assert_eq!(filter, FilterMode::Linear);
        assert_eq!(rect.size(), vec2(1000.0, 500.0));
        assert_eq!(rect.center(), area.center());
    }

    #[test]
    fn fingerprints_wrap_in_two_lines() {
        let fp = (0..32)
            .map(|i| format!("{i:02X}"))
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(split_fingerprint(&fp).lines().count(), 2);
    }
}
