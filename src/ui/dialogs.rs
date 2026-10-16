//! Sign-in and delete modals, and the settings and password drawers.

use egui::{Align, Color32, CornerRadius, Layout, RichText, Ui};
use egui_phosphor::regular as icon;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::credentials as credential_ui;
use super::tab::TabId;
use super::theme::{self, Palette, radius, size};
use super::widgets;
use crate::store::credentials::CredentialStore;
use crate::store::profiles::HostProfile;
use crate::store::settings::{AppSettings, ShortcutCapture, ThemePreference};

pub enum Dialog {
    Credentials(CredentialsDialog),
    ConfirmDelete { id: Uuid, name: String },
    Settings(DrawerState),
    Passwords(DrawerState),
}

#[derive(Default)]
pub struct DrawerState {
    closing: bool,
    animation_initialized: bool,
}

pub struct CredentialsDialog {
    profile: HostProfile,
    saved: bool,
    tab: Option<TabId>,
    password: Zeroizing<String>,
    remember: bool,
    update_profile_password: bool,
    use_stored_password: bool,
    /// Why we're asking again (e.g. the last attempt was rejected).
    reason: Option<String>,
    focus_pending: bool,
    busy: bool,
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
            update_profile_password: false,
            use_stored_password: false,
            reason,
            focus_pending: true,
            busy: false,
        }
    }

    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    pub fn password_save_failed(&mut self, password: Zeroizing<String>, reason: String) {
        self.password = password;
        self.update_profile_password = true;
        self.reason = Some(reason);
        self.busy = false;
    }
}

pub enum Outcome {
    Open,
    Close,
    Connect {
        profile: HostProfile,
        saved: bool,
        tab: Option<TabId>,
        password: Zeroizing<String>,
        remember: bool,
        update_profile_password: bool,
        use_stored_password: bool,
    },
    Delete(Uuid),
    Credential(credential_ui::Action),
}

fn modal_frame(p: &Palette, ctx: &egui::Context) -> egui::Frame {
    egui::Frame::new()
        .fill(p.surface)
        .stroke(egui::Stroke::new(1.0, p.hairline))
        .corner_radius(CornerRadius::same(radius::PANEL))
        .inner_margin(egui::Margin::same(24))
        .shadow(ctx.global_style().visuals.window_shadow)
}

pub fn show(
    ctx: &egui::Context,
    dialog: &mut Dialog,
    settings: &mut AppSettings,
    credential_store: &CredentialStore,
    hosts: &[HostProfile],
    credential_manager: &mut credential_ui::Manager,
) -> Outcome {
    let passwords = matches!(dialog, Dialog::Passwords(_));
    if let Dialog::Settings(state) | Dialog::Passwords(state) = dialog {
        return side_drawer(
            ctx,
            state,
            settings,
            credential_store,
            hosts,
            credential_manager,
            passwords,
        );
    }

    let p = Palette::of_ctx(ctx);
    let backdrop = Color32::from_black_alpha(if p.dark { 150 } else { 90 });

    let modal = egui::Modal::new(egui::Id::new("dialog"))
        .frame(modal_frame(&p, ctx))
        .backdrop_color(backdrop);

    let response = modal.show(ctx, |ui| match dialog {
        Dialog::Credentials(state) => {
            ui.set_width(380.0);
            let height = (ctx.content_rect().height() - 50.0).max(100.0);
            ui.set_max_height(height);
            egui::ScrollArea::vertical()
                .id_salt("sign-in-body")
                .max_height(height)
                .auto_shrink([false, true])
                .show(ui, |ui| credentials(ui, state, credential_store))
                .inner
        }
        Dialog::ConfirmDelete { id, name } => confirm_delete(ui, *id, name),
        Dialog::Settings(_) | Dialog::Passwords(_) => unreachable!("drawers are handled above"),
    });

    match response.inner {
        Outcome::Open
            if !matches!(dialog, Dialog::Credentials(state) if state.busy)
                && response.should_close() =>
        {
            Outcome::Close
        }
        other => other,
    }
}

fn credentials(ui: &mut Ui, state: &mut CredentialsDialog, store: &CredentialStore) -> Outcome {
    let p = Palette::of(ui);
    ui.set_width(380.0);
    if state.busy {
        ui.disable();
        ui.label("Updating profile password…");
    }

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
    if credential_ui::selector(ui, &mut state.profile.credential_profile_id, store) {
        state.password.clear();
        state.update_profile_password = false;
        state.use_stored_password = state.profile.credential_profile_id.is_some();
        state.focus_pending = true;
    }
    ui.add_space(6.0);
    let linked = state.profile.credential_profile_id;
    let user = if let Some(id) = linked {
        if let Some(profile) = store.get(id) {
            ui.label(profile.account_label());
        } else {
            ui.colored_label(p.poor, "Choose an available profile or Manual.");
        }
        None
    } else {
        let response = widgets::field(
            ui,
            "User name",
            &mut state.profile.username,
            "jdoe or CORP\\jdoe",
        );
        ui.add_space(6.0);
        widgets::field(ui, "Domain", &mut state.profile.domain, "Optional");
        Some(response)
    };
    ui.add_space(6.0);
    ui.label(RichText::new("Password").size(size::SMALL).color(p.muted));
    let hint = if linked.is_some() && state.use_stored_password {
        "Leave blank to use profile password"
    } else {
        "Enter password"
    };
    let password = widgets::text_input(ui, &mut state.password, hint, true);

    if state.focus_pending {
        state.focus_pending = false;
        if state.profile.username.is_empty()
            && let Some(user) = user
        {
            user.request_focus();
        } else {
            password.request_focus();
        }
    }

    if linked.is_some() {
        if state.password.is_empty() {
            state.update_profile_password = false;
        }
        ui.add_enabled_ui(!state.password.is_empty(), |ui| {
            ui.checkbox(
                &mut state.update_profile_password,
                "Update profile password",
            );
        });
        ui.label(
            RichText::new("Updates future connections for every assigned host.")
                .size(size::SMALL)
                .color(p.muted),
        );
    } else if state.saved {
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
    let can_connect = match linked {
        Some(id) => store.get(id).is_some_and(|p| !p.username.trim().is_empty()),
        None => !state.profile.username.trim().is_empty(),
    };
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
                update_profile_password: state.update_profile_password,
                use_stored_password: state.use_stored_password && state.password.is_empty(),
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

const SETTINGS_DRAWER_WIDTH: f32 = 488.0;
const SETTINGS_TRANSITION_TIME: f32 = 0.22;

fn side_drawer(
    ctx: &egui::Context,
    state: &mut DrawerState,
    settings: &mut AppSettings,
    credential_store: &CredentialStore,
    hosts: &[HostProfile],
    credential_manager: &mut credential_ui::Manager,
    passwords: bool,
) -> Outcome {
    let p = Palette::of_ctx(ctx);
    let id = egui::Id::new(if passwords {
        "passwords-drawer"
    } else {
        "settings-drawer"
    });
    let animation = id.with("animation");
    if !state.animation_initialized {
        // Seed every opening, including the first one, at the offscreen position.
        ctx.animate_value_with_time(animation, 0.0, 0.0);
        state.animation_initialized = true;
    }
    let progress = ctx.animate_value_with_time(
        animation,
        if state.closing { 0.0 } else { 1.0 },
        if settings.reduce_motion {
            0.0
        } else {
            SETTINGS_TRANSITION_TIME
        },
    );
    if state.closing && progress <= 0.0 {
        return Outcome::Close;
    }
    let progress = egui::emath::easing::cubic_in_out(progress);
    let screen = ctx.content_rect();
    let width = SETTINGS_DRAWER_WIDTH.min(screen.width());
    let frame = egui::Frame::new()
        .fill(p.surface)
        .stroke(egui::Stroke::new(1.0, p.hairline))
        .inner_margin(egui::Margin::same(24))
        .shadow(egui::epaint::Shadow {
            offset: [-8, 0],
            ..ctx.global_style().visuals.window_shadow
        });
    let content_size =
        (egui::vec2(width, screen.height()) - frame.total_margin().sum()).max(egui::Vec2::ZERO);
    let area = egui::Area::new(id)
        .kind(egui::UiKind::Modal)
        .order(egui::Order::Foreground)
        .sense(egui::Sense::hover())
        .fixed_pos(egui::pos2(screen.right() - width * progress, screen.top()))
        .default_size(egui::vec2(width, screen.height()))
        .constrain(false)
        .fade_in(false);
    let response = egui::Modal::new(id)
        .area(area)
        .frame(frame)
        .backdrop_color(Color32::from_black_alpha(
            ((if p.dark { 150.0 } else { 90.0 }) * progress).round() as u8,
        ))
        .show(ctx, |ui| {
            ui.set_min_size(content_size);
            ui.set_max_size(content_size);
            // Freeze controls during dismissal, while the modal still blocks the app.
            if state.closing {
                ui.disable();
            }
            let mut close = false;
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(if passwords { "Passwords" } else { "Settings" })
                        .font(theme::font_semibold(size::HEADING)),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = widgets::icon_button(ui, icon::X, "Close").clicked();
                });
            });
            ui.add_space(14.0);
            let mut action = None;
            egui::ScrollArea::vertical()
                .id_salt(id.with("body"))
                .max_height(ui.available_height())
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if passwords {
                        action = credential_manager.show(ui, credential_store, hosts);
                    } else {
                        settings_contents(ui, settings);
                    }
                });
            (close, action)
        });

    if response.inner.0 || response.should_close() {
        state.closing = true;
        ctx.request_repaint();
    }
    if let Some(action) = response.inner.1 {
        Outcome::Credential(action)
    } else if state.closing && settings.reduce_motion {
        Outcome::Close
    } else {
        Outcome::Open
    }
}

fn settings_contents(ui: &mut Ui, settings: &mut AppSettings) {
    let p = Palette::of(ui);
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
    settings_row(
        ui,
        "Reduce motion",
        Some("Turn off interface animations."),
        |ui| {
            widgets::toggle(ui, &mut settings.reduce_motion);
        },
    );
    settings_row(
        ui,
        "Desktop previews",
        Some(
            "Show a snapshot of each host's screen on its tile. Saved unencrypted in Relay's data folder; turning this off deletes them.",
        ),
        |ui| {
            widgets::toggle(ui, &mut settings.desktop_previews);
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
    widgets::section_label(ui, "Touchpad");
    ui.add_space(2.0);
    settings_row(
        ui,
        "Scroll momentum",
        Some("Keep scrolling briefly after you lift your fingers, as local apps do."),
        |ui| {
            widgets::toggle(ui, &mut settings.touchpad_momentum);
        },
    );
    settings_row(
        ui,
        "Scroll speed",
        Some("How far the remote page scrolls for each swipe."),
        |ui| {
            egui::ComboBox::from_id_salt("scroll-speed")
                .selected_text(speed_label(settings.touchpad_scroll_speed))
                .show_ui(ui, |ui| {
                    for speed in SCROLL_SPEEDS {
                        ui.selectable_value(
                            &mut settings.touchpad_scroll_speed,
                            speed,
                            speed_label(speed),
                        );
                    }
                });
        },
    );

    ui.add_space(16.0);
    widgets::section_label(ui, "Performance");
    ui.add_space(2.0);
    settings_row(
        ui,
        "Lowest latency",
        Some("Queue one frame instead of two. Applies after restarting Relay."),
        |ui| {
            widgets::toggle(ui, &mut settings.low_latency);
        },
    );

    settings_row(
        ui,
        "Hardware video decoding",
        Some(
            "Use the GPU for H.264 video and to draw the desktop. Falls back automatically. Applies to new connections and reconnects.",
        ),
        |ui| {
            widgets::toggle(ui, &mut settings.hardware_video_decoding);
        },
    );
}

fn settings_row(
    ui: &mut Ui,
    label: &str,
    description: Option<&str>,
    control: impl FnOnce(&mut Ui),
) {
    let p = Palette::of(ui);
    ui.horizontal(|ui| {
        // Reserve the control column before wrapping descriptions, so long rows
        // cannot expand the drawer beyond its fixed width.
        let label_width = (ui.available_width() - 100.0 - ui.spacing().item_spacing.x).max(0.0);
        ui.allocate_ui_with_layout(
            egui::vec2(label_width, 0.0),
            Layout::top_down(Align::Min),
            |ui| {
                ui.set_width(label_width);
                ui.label(RichText::new(label).font(theme::font_medium(size::BODY)));
                if let Some(description) = description {
                    ui.label(RichText::new(description).size(size::SMALL).color(p.muted));
                }
            },
        );
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
}

const SCROLL_SPEEDS: [f32; 6] = [0.5, 0.75, 1.0, 1.5, 2.0, 3.0];

fn speed_label(speed: f32) -> String {
    format!("{:.0}%", speed * 100.0)
}

fn capture_label(capture: ShortcutCapture) -> &'static str {
    match capture {
        ShortcutCapture::Fullscreen => "In fullscreen",
        ShortcutCapture::Always => "Always",
        ShortcutCapture::Never => "Never",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Context, Event, Key, Modifiers, PointerButton, Pos2, Rect, pos2, vec2};

    struct DrawerFrame {
        outcome: Outcome,
        rect: Rect,
        shapes: Vec<egui::epaint::ClippedShape>,
    }

    impl DrawerFrame {
        fn text_rect(&self, label: &str) -> Rect {
            self.find_text_rect(label)
                .unwrap_or_else(|| panic!("missing text: {label}"))
        }

        fn find_text_rect(&self, label: &str) -> Option<Rect> {
            self.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(Rect::from_min_size(text.pos, text.galley.size()))
                }
                _ => None,
            })
        }

        fn backdrop_alpha(&self, screen: Rect) -> u8 {
            self.shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Rect(rect)
                        if rect.rect == screen && rect.fill.r() == 0 && rect.fill.g() == 0 =>
                    {
                        Some(rect.fill.a())
                    }
                    _ => None,
                })
                .expect("modal backdrop")
        }
    }

    struct Harness {
        ctx: Context,
        dialog: Dialog,
        settings: AppSettings,
        credential_store: CredentialStore,
        credential_manager: credential_ui::Manager,
        screen: Rect,
        time: f64,
        background_clicks: usize,
    }

    impl Harness {
        fn new(reduce_motion: bool) -> Self {
            let ctx = Context::default();
            theme::install_fonts(&ctx);
            Self {
                ctx,
                dialog: Dialog::Settings(DrawerState::default()),
                settings: AppSettings {
                    reduce_motion,
                    ..Default::default()
                },
                credential_store: CredentialStore::default(),
                credential_manager: credential_ui::Manager::default(),
                screen: Rect::from_min_size(Pos2::ZERO, vec2(720.0, 480.0)),
                time: 0.0,
                background_clicks: 0,
            }
        }

        fn frame(&mut self, events: Vec<Event>) -> DrawerFrame {
            self.time += 1.0 / 60.0;
            let mut outcome = Outcome::Open;
            let mut output = self.ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(self.screen),
                    time: Some(self.time),
                    events,
                    ..Default::default()
                },
                |root| {
                    let palette = if self.settings.theme == ThemePreference::Light {
                        theme::LIGHT
                    } else {
                        theme::DARK
                    };
                    theme::apply(root.ctx(), &palette, self.settings.reduce_motion);
                    egui::CentralPanel::default().show(root, |ui| {
                        if ui
                            .put(
                                Rect::from_min_size(pos2(20.0, 20.0), vec2(160.0, 36.0)),
                                egui::Button::new("Background"),
                            )
                            .clicked()
                        {
                            self.background_clicks += 1;
                        }
                    });
                    outcome = show(
                        root.ctx(),
                        &mut self.dialog,
                        &mut self.settings,
                        &self.credential_store,
                        &[],
                        &mut self.credential_manager,
                    );
                },
            );
            output.textures_delta.clear();
            DrawerFrame {
                outcome,
                rect: egui::containers::AreaState::load(
                    &self.ctx,
                    egui::Id::new("settings-drawer"),
                )
                .map_or(self.screen, |state| state.rect()),
                shapes: output.shapes,
            }
        }

        fn settle(&mut self) -> DrawerFrame {
            for _ in 0..20 {
                self.frame(vec![]);
            }
            self.frame(vec![])
        }

        fn click(&mut self, pos: Pos2) -> DrawerFrame {
            self.frame(vec![Event::PointerMoved(pos)]);
            self.frame(vec![pointer_button(pos, true)]);
            self.frame(vec![pointer_button(pos, false)])
        }

        fn state(&self) -> &DrawerState {
            let Dialog::Settings(state) = &self.dialog else {
                unreachable!()
            };
            state
        }
    }

    fn pointer_button(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    fn escape() -> Vec<Event> {
        [true, false]
            .into_iter()
            .map(|pressed| Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: Modifiers::NONE,
            })
            .collect()
    }

    #[test]
    fn drawer_animates_first_open_close_and_reopen() {
        let mut h = Harness::new(false);
        let first = h.frame(vec![]);
        assert_eq!(first.rect.left(), h.screen.right());
        let opening = h.frame(vec![]);
        assert!(opening.rect.left() < first.rect.left());
        assert!(opening.rect.left() > h.screen.right() - SETTINGS_DRAWER_WIDTH);
        assert!(matches!(opening.outcome, Outcome::Open));
        let open = h.settle();
        assert_eq!(
            open.rect.size(),
            vec2(SETTINGS_DRAWER_WIDTH, h.screen.height())
        );
        assert_eq!(open.rect.right(), h.screen.right());
        assert_eq!(open.backdrop_alpha(h.screen), 150);

        let dismiss = h.frame(escape());
        assert!(h.state().closing);
        assert!(matches!(dismiss.outcome, Outcome::Open));
        h.frame(vec![]);
        let closing = h.frame(vec![]);
        assert!(closing.rect.left() > open.rect.left());
        assert!(closing.rect.left() < h.screen.right());
        assert!(closing.backdrop_alpha(h.screen) < 150);
        assert!(matches!(closing.outcome, Outcome::Open));
        assert!(matches!(h.settle().outcome, Outcome::Close));

        h.dialog = Dialog::Settings(DrawerState::default());
        assert_eq!(h.frame(vec![]).rect.left(), h.screen.right());
        assert_eq!(h.settle().rect.right(), h.screen.right());
    }

    #[test]
    fn reduced_motion_opens_and_dismisses_immediately() {
        for close_with_button in [false, true] {
            let mut h = Harness::new(true);
            if close_with_button {
                h.settings.theme = ThemePreference::Light;
            }
            let first = h.frame(vec![]);
            assert_eq!(first.rect.right(), h.screen.right());
            let open = h.settle();
            assert_eq!(open.rect.right(), h.screen.right());
            assert_eq!(
                open.backdrop_alpha(h.screen),
                if close_with_button { 90 } else { 150 }
            );
            let closed = if close_with_button {
                h.click(open.text_rect(icon::X).center())
            } else {
                h.frame(escape())
            };
            assert!(matches!(closed.outcome, Outcome::Close));
        }
    }

    #[test]
    fn backdrop_dismisses_without_clicking_background_during_transitions() {
        let mut h = Harness::new(false);
        h.frame(vec![]);
        // The background button is also under the backdrop during the first opening.
        let dismissed = h.click(pos2(80.0, 38.0));
        assert!(h.state().closing);
        assert!(matches!(dismissed.outcome, Outcome::Open));
        h.click(pos2(80.0, 38.0));
        assert_eq!(h.background_clicks, 0);
        assert!(matches!(h.settle().outcome, Outcome::Close));
    }

    #[test]
    fn escape_dismisses_dropdown_before_drawer() {
        let mut h = Harness::new(true);
        let open = h.settle();
        h.click(open.text_rect("In fullscreen").center());
        assert!(egui::Popup::is_any_open(&h.ctx));
        assert!(matches!(h.frame(escape()).outcome, Outcome::Open));
        assert!(!egui::Popup::is_any_open(&h.ctx));
        assert!(!h.state().closing);
        assert!(matches!(h.frame(escape()).outcome, Outcome::Close));
    }

    #[test]
    fn header_stays_fixed_while_body_scrolls_and_drawer_tracks_resize() {
        let mut h = Harness::new(true);
        let open = h.settle();
        let header = open.text_rect("Settings");
        let close = open.text_rect(icon::X);
        let appearance = open.text_rect("Appearance");
        h.frame(vec![
            Event::PointerMoved(pos2(600.0, 300.0)),
            Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: vec2(0.0, -1000.0),
                phase: egui::TouchPhase::Move,
                modifiers: Modifiers::NONE,
            },
        ]);
        let scrolled = h.settle();
        assert_eq!(scrolled.text_rect("Settings"), header);
        assert_eq!(scrolled.text_rect(icon::X), close);
        assert!(
            scrolled
                .find_text_rect("Appearance")
                .is_none_or(|rect| rect.top() < appearance.top())
        );
        assert!(scrolled.text_rect("Hardware video decoding").bottom() < h.screen.bottom());

        for size in [vec2(1280.0, 800.0), vec2(400.0, 480.0)] {
            h.screen = Rect::from_min_size(Pos2::ZERO, size);
            let resized = h.settle();
            assert_eq!(resized.rect.right(), size.x);
            assert_eq!(resized.rect.top(), 0.0);
            assert_eq!(resized.rect.height(), size.y);
            assert_eq!(resized.rect.width(), SETTINGS_DRAWER_WIDTH.min(size.x));
        }
    }

    #[test]
    fn setting_change_is_retained_when_dismissed_in_same_frame() {
        let mut h = Harness::new(true);
        let open = h.settle();
        let light = open.text_rect("Light").center();
        h.frame(vec![
            Event::PointerMoved(light),
            pointer_button(light, true),
        ]);
        let before = h.settings.clone();
        let mut events = vec![pointer_button(light, false)];
        events.extend(escape());
        let closed = h.frame(events);
        assert_ne!(h.settings, before);
        assert_eq!(h.settings.theme, ThemePreference::Light);
        assert!(matches!(closed.outcome, Outcome::Close));
    }

    #[test]
    fn password_management_opens_in_its_own_drawer() {
        let mut h = Harness::new(true);
        let settings = h.settle();
        assert!(settings.find_text_rect("Credential profiles").is_none());
        assert!(settings.find_text_rect("Add profile").is_none());

        h.dialog = Dialog::Passwords(DrawerState::default());
        let passwords = h.settle();
        assert!(passwords.find_text_rect("Passwords").is_some());
        assert!(passwords.find_text_rect("Credential profiles").is_some());
        assert!(passwords.find_text_rect("Appearance").is_none());
        let rect = egui::containers::AreaState::load(&h.ctx, egui::Id::new("passwords-drawer"))
            .unwrap()
            .rect();
        assert_eq!(rect.right(), h.screen.right());
        assert_eq!(rect.height(), h.screen.height());
        h.click(passwords.text_rect("Add profile").center());
        assert!(h.credential_manager.draft.is_some());
        assert!(matches!(h.frame(escape()).outcome, Outcome::Close));
    }

    fn sign_in_harness() -> (Harness, Uuid) {
        let mut h = Harness::new(true);
        h.screen = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 800.0));
        let credential = crate::store::credentials::CredentialProfile {
            name: "Office".into(),
            username: "alice".into(),
            domain: "CORP".into(),
            ..Default::default()
        };
        let id = credential.id;
        h.credential_store.upsert(credential).unwrap();
        h.dialog = Dialog::Credentials(CredentialsDialog::new(
            HostProfile::new("host", 3389),
            true,
            None,
            None,
        ));
        let open = h.settle();
        h.click(open.text_rect("Manual").center());
        let menu = h.frame(vec![]);
        h.click(menu.text_rect("Office").center());
        h.settle();
        (h, id)
    }

    #[test]
    fn sign_in_selects_a_shared_profile_and_uses_its_stored_password() {
        let (mut h, id) = sign_in_harness();
        let open = h.settle();
        assert!(open.find_text_rect("CORP\\alice").is_some());
        assert!(open.find_text_rect("User name").is_none());
        let connected = h.click(open.text_rect("Connect").center());
        assert!(matches!(connected.outcome, Outcome::Connect {
            profile, saved: true, use_stored_password: true, update_profile_password: false, ..
        } if profile.credential_profile_id == Some(id)));
    }

    #[test]
    fn sign_in_password_updates_require_explicit_selection_and_remain_masked() {
        for update in [false, true] {
            let (mut h, _) = sign_in_harness();
            let open = h.frame(vec![Event::Text("replacement-secret".into())]);
            assert!(open.find_text_rect("replacement-secret").is_none());
            let Dialog::Credentials(state) = &h.dialog else {
                unreachable!()
            };
            assert_eq!(&**state.password, "replacement-secret");
            assert!(!state.update_profile_password);
            if update {
                h.click(open.text_rect("Update profile password").center());
            }
            let open = h.settle();
            let connected = h.click(open.text_rect("Connect").center());
            assert!(matches!(connected.outcome, Outcome::Connect {
                update_profile_password, use_stored_password: false, password, ..
            } if update_profile_password == update && &**password == "replacement-secret"));
        }
    }

    #[test]
    fn sign_in_retains_password_after_a_failed_shared_update() {
        let mut dialog = CredentialsDialog::new(HostProfile::new("host", 3389), true, None, None);
        dialog.set_busy(true);
        dialog.password_save_failed(
            Zeroizing::new("retry-secret".into()),
            "keyring locked".into(),
        );
        assert!(!dialog.busy);
        assert_eq!(&**dialog.password, "retry-secret");
        assert!(dialog.update_profile_password);
        assert_eq!(dialog.reason.as_deref(), Some("keyring locked"));
    }

    #[test]
    fn sign_in_controls_remain_reachable_at_minimum_window_height() {
        let mut h = Harness::new(true);
        let mut host = HostProfile::new("host", 3389);
        host.username = "alice".into();
        h.dialog = Dialog::Credentials(CredentialsDialog::new(host, true, None, None));
        h.settle();
        let rect = egui::containers::AreaState::load(&h.ctx, egui::Id::new("dialog"))
            .unwrap()
            .rect();
        assert!(rect.height() <= h.screen.height());
        h.frame(vec![
            Event::PointerMoved(rect.center()),
            Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: vec2(0.0, -1000.0),
                phase: egui::TouchPhase::Move,
                modifiers: Modifiers::NONE,
            },
        ]);
        let scrolled = h.settle();
        let connect = scrolled.text_rect("Connect");
        assert!(h.screen.contains(connect.center()));
        assert!(matches!(
            h.click(connect.center()).outcome,
            Outcome::Connect { .. }
        ));
    }
}
