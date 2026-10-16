//! Sign-in and delete modals, and the settings drawer.

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
    Settings(SettingsDrawer),
}

#[derive(Default)]
pub struct SettingsDrawer {
    closing: bool,
    animation_initialized: bool,
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
    if let Dialog::Settings(state) = dialog {
        return settings_drawer(ctx, state, settings);
    }

    let p = Palette::of_ctx(ctx);
    let backdrop = Color32::from_black_alpha(if p.dark { 150 } else { 90 });

    let modal = egui::Modal::new(egui::Id::new("dialog"))
        .frame(modal_frame(&p, ctx))
        .backdrop_color(backdrop);

    let response = modal.show(ctx, |ui| match dialog {
        Dialog::Credentials(state) => credentials(ui, state),
        Dialog::ConfirmDelete { id, name } => confirm_delete(ui, *id, name),
        Dialog::Settings(_) => unreachable!("settings use the drawer"),
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

const SETTINGS_DRAWER_WIDTH: f32 = 488.0;
const SETTINGS_TRANSITION_TIME: f32 = 0.22;

fn settings_drawer(
    ctx: &egui::Context,
    state: &mut SettingsDrawer,
    settings: &mut AppSettings,
) -> Outcome {
    let p = Palette::of_ctx(ctx);
    let animation = egui::Id::new("settings-drawer-animation");
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
    let area = egui::Area::new(egui::Id::new("settings-drawer"))
        .kind(egui::UiKind::Modal)
        .order(egui::Order::Foreground)
        .sense(egui::Sense::hover())
        .fixed_pos(egui::pos2(screen.right() - width * progress, screen.top()))
        .default_size(egui::vec2(width, screen.height()))
        .constrain(false)
        .fade_in(false);
    let response = egui::Modal::new(egui::Id::new("settings-drawer"))
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
                ui.label(RichText::new("Settings").font(theme::font_semibold(size::HEADING)));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = widgets::icon_button(ui, icon::X, "Close").clicked();
                });
            });
            ui.add_space(14.0);
            egui::ScrollArea::vertical()
                .id_salt("settings-body")
                .max_height(ui.available_height())
                .auto_shrink([false, false])
                .show(ui, |ui| settings_contents(ui, settings));
            close
        });

    if response.inner || response.should_close() {
        state.closing = true;
        ctx.request_repaint();
    }
    if state.closing && settings.reduce_motion {
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
                dialog: Dialog::Settings(SettingsDrawer::default()),
                settings: AppSettings {
                    reduce_motion,
                    ..Default::default()
                },
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
                    outcome = show(root.ctx(), &mut self.dialog, &mut self.settings);
                },
            );
            output.textures_delta.clear();
            DrawerFrame {
                outcome,
                rect: egui::containers::AreaState::load(
                    &self.ctx,
                    egui::Id::new("settings-drawer"),
                )
                .expect("drawer area")
                .rect(),
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

        fn state(&self) -> &SettingsDrawer {
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

        h.dialog = Dialog::Settings(SettingsDrawer::default());
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
}
