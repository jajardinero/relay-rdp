//! Modal dialogs: sign-in, delete confirmation, settings.

use egui::{Align, Color32, CornerRadius, Layout, RichText, Ui};
use egui_phosphor::regular as icon;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::tab::TabId;
use super::theme::{self, Palette, radius, size};
use super::widgets;
use crate::store::profiles::HostProfile;
use crate::store::settings::{AppSettings, ShortcutCapture, ThemePreference};

pub enum Dialog {
    Credentials(CredentialsDialog),
    ConfirmDelete { id: Uuid, name: String },
    Settings,
}

pub struct CredentialsDialog {
    profile: HostProfile,
    saved: bool,
    tab: Option<TabId>,
    password: Zeroizing<String>,
    remember: bool,
    /// Why we're asking again (e.g. the last attempt was rejected).
    reason: Option<String>,
    focus_pending: bool,
}

impl CredentialsDialog {
    pub fn new(
        profile: HostProfile,
        saved: bool,
        tab: Option<TabId>,
        reason: Option<String>,
    ) -> Self {
        let remember = saved && profile.save_password;
        Self {
            profile,
            saved,
            tab,
            password: Zeroizing::new(String::new()),
            remember,
            reason,
            focus_pending: true,
        }
    }
}

pub enum Outcome {
    Open,
    Close,
    SettingsChanged,
    Connect {
        profile: HostProfile,
        saved: bool,
        tab: Option<TabId>,
        password: Zeroizing<String>,
        remember: bool,
    },
    Delete(Uuid),
}

fn modal_frame(p: &Palette, ctx: &egui::Context) -> egui::Frame {
    egui::Frame::new()
        .fill(p.surface)
        .stroke(egui::Stroke::new(1.0, p.hairline))
        .corner_radius(CornerRadius::same(radius::PANEL))
        .inner_margin(egui::Margin::same(24))
        .shadow(ctx.global_style().visuals.window_shadow)
}

pub fn show(ctx: &egui::Context, dialog: &mut Dialog, settings: &mut AppSettings) -> Outcome {
    let p = Palette::of_ctx(ctx);
    let backdrop = Color32::from_black_alpha(if p.dark { 150 } else { 90 });

    let modal = egui::Modal::new(egui::Id::new("dialog"))
        .frame(modal_frame(&p, ctx))
        .backdrop_color(backdrop);

    let response = modal.show(ctx, |ui| match dialog {
        Dialog::Credentials(state) => credentials(ui, state),
        Dialog::ConfirmDelete { id, name } => confirm_delete(ui, *id, name),
        Dialog::Settings => {
            egui::ScrollArea::vertical()
                .max_height((ctx.content_rect().height() - 96.0).max(160.0))
                .show(ui, |ui| settings_dialog(ui, settings))
                .inner
        }
    });

    match response.inner {
        Outcome::Open if response.should_close() => Outcome::Close,
        other => other,
    }
}

fn credentials(ui: &mut Ui, state: &mut CredentialsDialog) -> Outcome {
    let p = Palette::of(ui);
    ui.set_width(380.0);

    ui.label(RichText::new("Sign in").font(theme::font_semibold(size::HEADING)));
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("to").color(p.muted));
        ui.label(RichText::new(state.profile.endpoint()).font(theme::mono(size::BODY)));
    });

    if let Some(reason) = &state.reason {
        ui.add_space(10.0);
        egui::Frame::new()
            .fill(p.poor.gamma_multiply(0.12))
            .corner_radius(CornerRadius::same(radius::ROW))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::WARNING_CIRCLE).color(p.poor).size(16.0));
                    ui.label(RichText::new(reason).color(p.text));
                });
            });
    }

    ui.add_space(14.0);
    let user = widgets::field(
        ui,
        "User name",
        &mut state.profile.username,
        "jdoe or CORP\\jdoe",
    );
    ui.add_space(6.0);
    widgets::field(ui, "Domain", &mut state.profile.domain, "Optional");
    ui.add_space(6.0);
    ui.label(RichText::new("Password").size(size::SMALL).color(p.muted));
    let password = widgets::text_input(ui, &mut state.password, "", true);

    if state.focus_pending {
        state.focus_pending = false;
        if state.profile.username.is_empty() {
            user.request_focus();
        } else {
            password.request_focus();
        }
    }

    if state.saved {
        ui.add_space(10.0);
        widgets::setting_row(
            ui,
            "Remember password",
            Some("Saved in the system keyring."),
            |ui| {
                widgets::toggle(ui, &mut state.remember);
            },
        );
    }

    ui.add_space(18.0);
    let can_connect = !state.profile.username.trim().is_empty();
    let submitted =
        password.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && can_connect;
    let mut outcome = Outcome::Open;
    widgets::button_row(ui, |ui| {
        let connect = ui
            .add_enabled_ui(can_connect, |ui| widgets::primary_button(ui, "Connect"))
            .inner;
        if connect.clicked() || submitted {
            outcome = Outcome::Connect {
                profile: state.profile.clone(),
                saved: state.saved,
                tab: state.tab,
                password: state.password.clone(),
                remember: state.remember,
            };
        }
        if widgets::secondary_button(ui, "Cancel").clicked() {
            outcome = Outcome::Close;
        }
    });
    outcome
}

fn confirm_delete(ui: &mut Ui, id: Uuid, name: &str) -> Outcome {
    let p = Palette::of(ui);
    ui.set_width(360.0);
    ui.label(RichText::new(format!("Delete {name}?")).font(theme::font_semibold(size::SUBHEAD)));
    ui.add_space(4.0);
    ui.label(
        RichText::new(
            "Its settings and saved password will be removed. Open sessions stay connected.",
        )
        .color(p.muted),
    );
    ui.add_space(18.0);
    let mut outcome = Outcome::Open;
    widgets::button_row(ui, |ui| {
        if widgets::danger_button(ui, "Delete").clicked() {
            outcome = Outcome::Delete(id);
        }
        if widgets::secondary_button(ui, "Cancel").clicked() {
            outcome = Outcome::Close;
        }
    });
    outcome
}

fn settings_dialog(ui: &mut Ui, settings: &mut AppSettings) -> Outcome {
    let p = Palette::of(ui);
    ui.set_width(440.0);
    let before = settings.clone();
    let mut close = false;

    ui.horizontal(|ui| {
        ui.label(RichText::new("Settings").font(theme::font_semibold(size::HEADING)));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            close = widgets::icon_button(ui, icon::X, "Close").clicked();
        });
    });
    ui.add_space(14.0);

    widgets::section_label(ui, "Appearance");
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        for (pref, label) in [
            (ThemePreference::System, "System"),
            (ThemePreference::Dark, "Dark"),
            (ThemePreference::Light, "Light"),
        ] {
            let selected = settings.theme == pref;
            let text = RichText::new(label)
                .font(theme::font_medium(size::BODY))
                .color(if selected { p.on_accent } else { p.text });
            let button = egui::Button::new(text)
                .fill(if selected { p.accent } else { p.raised })
                .corner_radius(CornerRadius::same(radius::CONTROL))
                .min_size(egui::vec2(88.0, 32.0));
            if ui.add(button).clicked() {
                settings.theme = pref;
            }
        }
    });
    ui.add_space(8.0);
    widgets::setting_row(
        ui,
        "Reduce motion",
        Some("Turn off interface animations."),
        |ui| {
            widgets::toggle(ui, &mut settings.reduce_motion);
        },
    );

    ui.add_space(16.0);
    widgets::section_label(ui, "Keyboard");
    ui.add_space(2.0);
    ui.label(
        RichText::new("Send system shortcuts (Super, Alt+Tab) to the remote computer")
            .color(p.text),
    );
    egui::ComboBox::from_id_salt("capture")
        .width(ui.available_width())
        .selected_text(capture_label(settings.capture_shortcuts))
        .show_ui(ui, |ui| {
            for option in [
                ShortcutCapture::Fullscreen,
                ShortcutCapture::Always,
                ShortcutCapture::Never,
            ] {
                ui.selectable_value(
                    &mut settings.capture_shortcuts,
                    option,
                    capture_label(option),
                );
            }
        });
    ui.add_space(8.0);
    egui::Grid::new("shortcuts")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for (keys, what) in [
                ("Ctrl+Alt+Enter", "Toggle fullscreen"),
                ("Ctrl+Alt+Home", "Show the session bar"),
                ("Ctrl+Alt+End", "Send Ctrl+Alt+Del"),
            ] {
                ui.label(RichText::new(keys).font(theme::mono(12.0)).color(p.text));
                ui.label(RichText::new(what).size(size::SMALL).color(p.muted));
                ui.end_row();
            }
        });

    ui.add_space(16.0);
    widgets::section_label(ui, "Performance");
    ui.add_space(2.0);
    widgets::setting_row(
        ui,
        "Lowest latency",
        Some("Queue one frame instead of two. Applies after restarting Relay."),
        |ui| {
            widgets::toggle(ui, &mut settings.low_latency);
        },
    );

    widgets::setting_row(
        ui,
        "Hardware video decoding",
        Some(
            "Use the GPU for H.264 video and to draw the desktop. Falls back automatically. Applies to new connections and reconnects.",
        ),
        |ui| {
            widgets::toggle(ui, &mut settings.hardware_video_decoding);
        },
    );

    if close {
        Outcome::Close
    } else if *settings != before {
        Outcome::SettingsChanged
    } else {
        Outcome::Open
    }
}

fn capture_label(capture: ShortcutCapture) -> &'static str {
    match capture {
        ShortcutCapture::Fullscreen => "In fullscreen",
        ShortcutCapture::Always => "Always",
        ShortcutCapture::Never => "Never",
    }
}
