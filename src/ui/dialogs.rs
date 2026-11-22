//! Sign-in and delete modals, and the settings and password drawers.

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, InteractiveElement, IntoElement,
    MouseButton, ParentElement, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Window, div, prelude::FluentBuilder as _, px,
};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::RelayApp;
use super::assets::{icon_svg, icons};
use super::credentials as credential_ui;
use super::tab::TabId;
use super::theme::{FONT_MONO, Palette, radius, size};
use super::widgets;
use crate::store::credentials::CredentialStore;
use crate::store::profiles::HostProfile;
use crate::store::settings::{AppSettings, ShortcutCapture, ThemePreference};

pub enum Dialog {
    Credentials(Box<CredentialsDialog>),
    ConfirmDelete { id: Uuid, name: String },
    Settings,
    Passwords,
}

pub struct CredentialsDialog {
    pub profile: HostProfile,
    pub saved: bool,
    pub tab: Option<TabId>,
    pub remember: bool,
    pub update_profile_password: bool,
    pub use_stored_password: bool,
    /// Why we're asking again (e.g. the last attempt was rejected).
    pub reason: Option<String>,
    pub busy: bool,
    pub user_input: Entity<InputState>,
    pub domain_input: Entity<InputState>,
    pub password_input: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl CredentialsDialog {
    pub fn new(
        profile: HostProfile,
        saved: bool,
        tab: Option<TabId>,
        reason: Option<String>,
        window: &mut Window,
        cx: &mut Context<RelayApp>,
    ) -> Self {
        let remember = saved && profile.save_password;
        let username_val = profile.username.clone();
        let domain_val = profile.domain.clone();
        let linked = profile.credential_profile_id.is_some();

        let user_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("jdoe or CORP\\jdoe");
            s.set_value(username_val, window, cx);
            s
        });
        let domain_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("Optional");
            s.set_value(domain_val, window, cx);
            s
        });
        let password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Enter password")
                .masked(true)
        });

        // Focus username if empty and manual, otherwise focus password.
        if !linked && profile.username.is_empty() {
            user_input.update(cx, |s, cx| s.focus(window, cx));
        } else {
            password_input.update(cx, |s, cx| s.focus(window, cx));
        }

        let mut subs = Vec::new();
        subs.push(
            cx.subscribe_in(&user_input, window, |this, input, event, _window, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some(Dialog::Credentials(dlg)) = &mut this.dialog
                {
                    dlg.profile.username = input.read(cx).value().to_string();
                    cx.notify();
                }
            }),
        );
        subs.push(
            cx.subscribe_in(&domain_input, window, |this, input, event, _window, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some(Dialog::Credentials(dlg)) = &mut this.dialog
                {
                    dlg.profile.domain = input.read(cx).value().to_string();
                    cx.notify();
                }
            }),
        );
        subs.push(
            cx.subscribe_in(&password_input, window, |this, input, event, window, cx| {
                match event {
                    InputEvent::Change => {
                        if let Some(Dialog::Credentials(dlg)) = &mut this.dialog
                            && input.read(cx).value().is_empty()
                        {
                            dlg.update_profile_password = false;
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => {
                        this.submit_credentials_dialog(window, cx);
                    }
                    _ => {}
                }
            }),
        );

        Self {
            profile,
            saved,
            tab,
            remember,
            update_profile_password: false,
            use_stored_password: false,
            reason,
            busy: false,
            user_input,
            domain_input,
            password_input,
            _subs: subs,
        }
    }

    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    pub fn password_save_failed(
        &mut self,
        password: Zeroizing<String>,
        reason: String,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.password_input.update(cx, |s, cx| {
            s.set_value((*password).clone(), window, cx);
        });
        self.update_profile_password = true;
        self.reason = Some(reason);
        self.busy = false;
    }

    pub fn password_value(&self, cx: &App) -> Zeroizing<String> {
        Zeroizing::new(self.password_input.read(cx).value().to_string())
    }

    pub fn can_connect(&self, store: &CredentialStore) -> bool {
        match self.profile.credential_profile_id {
            Some(id) => store.get(id).is_some_and(|p| !p.username.trim().is_empty()),
            None => !self.profile.username.trim().is_empty(),
        }
    }
}

const SETTINGS_DRAWER_WIDTH: f32 = 488.0;
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

pub fn render_dialog(
    this: &RelayApp,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> Option<AnyElement> {
    let dialog = this.dialog.as_ref()?;
    let busy = matches!(dialog, Dialog::Credentials(c) if c.busy);

    match dialog {
        Dialog::Settings => Some(render_drawer(
            "settings-drawer",
            "Settings",
            p,
            render_settings_contents(&this.settings.settings, p, cx),
            cx,
        )),
        Dialog::Passwords => {
            let entity = cx.entity().clone();
            let e_edit = entity.clone();
            let e_save = entity.clone();
            let e_cancel = entity.clone();
            let e_del_confirm = entity.clone();
            let e_del_exec = entity.clone();
            let body = credential_ui::render_manager(
                &this.credential_manager,
                &this.credentials,
                this.profiles.hosts(),
                p,
                move |profile, is_new, window, cx| {
                    e_edit.update(cx, |app, cx| {
                        app.credential_manager.draft =
                            Some(credential_ui::Draft::new(profile, is_new, window, cx));
                        app.credential_manager.error = None;
                        cx.notify();
                    });
                },
                move |_window, cx| {
                    e_save.update(cx, |app, cx| {
                        let collected = app
                            .credential_manager
                            .draft
                            .as_mut()
                            .map(|d| d.collect(cx));
                        if let Some((profile, password)) = collected {
                            app.handle_credential_action(credential_ui::Action::Save {
                                profile,
                                password,
                            });
                            cx.notify();
                        }
                    });
                },
                move |_window, cx| {
                    e_cancel.update(cx, |app, cx| {
                        app.credential_manager.draft = None;
                        app.credential_manager.deleting = None;
                        app.credential_manager.error = None;
                        cx.notify();
                    });
                },
                move |id, _window, cx| {
                    e_del_confirm.update(cx, |app, cx| {
                        app.credential_manager.deleting = Some(id);
                        app.credential_manager.error = None;
                        cx.notify();
                    });
                },
                move |id, _window, cx| {
                    e_del_exec.update(cx, |app, cx| {
                        app.handle_credential_action(credential_ui::Action::Delete(id));
                        cx.notify();
                    });
                },
                cx,
            );
            Some(render_drawer("passwords-drawer", "Passwords", p, body, cx))
        }
        Dialog::Credentials(state) => {
            let card = render_credentials_modal(state, &this.credentials, p, cx);
            Some(modal_backdrop("credentials-modal", busy, p, card, cx))
        }
        Dialog::ConfirmDelete { id, name } => {
            let card = render_confirm_delete_modal(*id, name, p, cx);
            Some(modal_backdrop("confirm-delete-modal", false, p, card, cx))
        }
    }
}

fn modal_backdrop(
    id: &'static str,
    busy: bool,
    p: &Palette,
    card: AnyElement,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    div()
        .id(id)
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(p.backdrop())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _window, cx| {
                if !busy {
                    this.dialog = None;
                    cx.notify();
                }
            }),
        )
        .child(
            div()
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .child(card),
        )
        .into_any_element()
}

fn render_credentials_modal(
    state: &CredentialsDialog,
    store: &CredentialStore,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let linked = state.profile.credential_profile_id;
    let pw_empty = state.password_input.read(cx).value().is_empty();
    let can_connect = !state.busy && state.can_connect(store);
    let entity = cx.entity().clone();

    let mut card = div()
        .flex()
        .flex_col()
        .w(px(428.0))
        .max_h(px(620.0))
        .p(px(24.0))
        .rounded(radius::PANEL)
        .bg(p.surface)
        .border_1()
        .border_color(p.hairline)
        .shadow(p.window_shadow())
        .gap(px(10.0));

    if state.busy {
        card = card.child(
            div()
                .text_size(size::SMALL)
                .text_color(p.muted)
                .child("Updating profile password…"),
        );
    }

    card = card
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(size::HEADING)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text)
                        .child("Sign in"),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(div().text_size(size::BODY).text_color(p.muted).child("to"))
                        .child(
                            div()
                                .text_size(size::BODY)
                                .font_family(FONT_MONO)
                                .text_color(p.text)
                                .child(state.profile.endpoint()),
                        ),
                ),
        )
        .when_some(state.reason.as_ref(), |el, reason| {
            el.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .w_full()
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(radius::ROW)
                    .bg(p.poor.opacity(0.12))
                    .child(icon_svg(icons::WARNING_CIRCLE, px(16.0), p.poor))
                    .child(
                        div()
                            .flex_1()
                            .text_size(size::BODY)
                            .text_color(p.text)
                            .child(reason.clone()),
                    ),
            )
        });

    let e_sel = entity.clone();
    card = card.child(div().mt(px(4.0)).child(credential_ui::selector(
        "signin-cred-select",
        linked,
        store,
        p,
        move |selected, window, cx| {
            e_sel.update(cx, |app, cx| {
                if let Some(Dialog::Credentials(dlg)) = &mut app.dialog {
                    dlg.profile.credential_profile_id = selected;
                    dlg.password_input.update(cx, |s, cx| {
                        s.set_value("", window, cx);
                    });
                    dlg.update_profile_password = false;
                    dlg.use_stored_password = selected.is_some();
                    dlg.password_input.update(cx, |s, cx| s.focus(window, cx));
                    cx.notify();
                }
            });
        },
    )));

    if let Some(id) = linked {
        if let Some(prof) = store.get(id) {
            card = card.child(
                div()
                    .text_size(size::BODY)
                    .text_color(p.text)
                    .child(prof.account_label()),
            );
        } else {
            card = card.child(
                div()
                    .text_size(size::BODY)
                    .text_color(p.poor)
                    .child("Choose an available profile or Manual."),
            );
        }
    } else {
        card = card
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(widgets::field_label("User name", p))
                    .child(Input::new(&state.user_input).disabled(state.busy)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(widgets::field_label("Domain", p))
                    .child(Input::new(&state.domain_input).disabled(state.busy)),
            );
    }

    card = card.child(
        div()
            .flex()
            .flex_col()
            .child(widgets::field_label("Password", p))
            .child(Input::new(&state.password_input).disabled(state.busy)),
    );

    if linked.is_some() {
        let e_cb = entity.clone();
        card = card.child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    Checkbox::new("update-profile-pw")
                        .label("Update profile password")
                        .checked(state.update_profile_password)
                        .disabled(pw_empty || state.busy)
                        .on_click(move |checked, _window, cx| {
                            let checked = *checked;
                            e_cb.update(cx, |app, cx| {
                                if let Some(Dialog::Credentials(dlg)) = &mut app.dialog {
                                    dlg.update_profile_password = checked;
                                    cx.notify();
                                }
                            });
                        }),
                )
                .child(
                    div()
                        .text_size(size::SMALL)
                        .text_color(p.muted)
                        .child("Updates future connections for every assigned host."),
                ),
        );
    } else if state.saved {
        let e_rem = entity.clone();
        card = card.child(div().mt(px(4.0)).child(widgets::setting_row(
            "Remember password",
            Some("Saved in the system keyring."),
            p,
            widgets::toggle("signin-remember", state.remember, p, move |on, _, cx| {
                let on = *on;
                e_rem.update(cx, |app, cx| {
                    if let Some(Dialog::Credentials(dlg)) = &mut app.dialog {
                        dlg.remember = on;
                        cx.notify();
                    }
                });
            }),
        )));
    }

    let busy = state.busy;
    card = card.child(
        div().mt(px(8.0)).child(
            widgets::button_row()
                .child(widgets::secondary_button(
                    "signin-cancel",
                    "Cancel",
                    p,
                    cx.listener(move |this, _, _, cx| {
                        if !busy {
                            this.dialog = None;
                            cx.notify();
                        }
                    }),
                ))
                .child(widgets::primary_button(
                    "signin-connect",
                    "Connect",
                    p,
                    !can_connect,
                    cx.listener(|this, _, window, cx| {
                        this.submit_credentials_dialog(window, cx);
                    }),
                )),
        ),
    );

    card.into_any_element()
}

fn render_confirm_delete_modal(
    id: Uuid,
    name: &str,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .w(px(408.0))
        .p(px(24.0))
        .rounded(radius::PANEL)
        .bg(p.surface)
        .border_1()
        .border_color(p.hairline)
        .shadow(p.window_shadow())
        .gap(px(8.0))
        .child(
            div()
                .text_size(size::SUBHEAD)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text)
                .child(format!("Delete {name}?")),
        )
        .child(
            div()
                .text_size(size::BODY)
                .text_color(p.muted)
                .child("Its settings and saved password will be removed. Open sessions stay connected."),
        )
        .child(
            div().mt(px(12.0)).child(
                widgets::button_row()
                    .child(widgets::secondary_button(
                        "delete-cancel",
                        "Cancel",
                        p,
                        cx.listener(|this, _, _, cx| {
                            this.dialog = None;
                            cx.notify();
                        }),
                    ))
                    .child(widgets::danger_button(
                        "delete-confirm",
                        "Delete",
                        p,
                        cx.listener(move |this, _, _, cx| {
                            this.delete_host(id);
                            cx.notify();
                        }),
                    )),
            ),
        )
        .into_any_element()
}

fn render_drawer(
    id: &'static str,
    title: &'static str,
    p: &Palette,
    body: AnyElement,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    div()
        .id(id)
        .absolute()
        .inset_0()
        .flex()
        .flex_row()
        .justify_end()
        .bg(p.backdrop())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.dialog = None;
                cx.notify();
            }),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .w(px(SETTINGS_DRAWER_WIDTH))
                .h_full()
                .p(px(24.0))
                .bg(p.surface)
                .border_l_1()
                .border_color(p.hairline)
                .shadow(p.window_shadow())
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .mb(px(16.0))
                        .child(
                            div()
                                .text_size(size::HEADING)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.text)
                                .child(title),
                        )
                        .child(widgets::icon_button(
                            SharedString::from(format!("{id}-close")),
                            icons::X,
                            "Close",
                            p,
                            cx.listener(|this, _, _, cx| {
                                this.dialog = None;
                                cx.notify();
                            }),
                        )),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("{id}-scroll")))
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scrollbar()
                        .child(body),
                ),
        )
        .into_any_element()
}

fn render_settings_contents(
    settings: &AppSettings,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let entity = cx.entity().clone();
    let palette = *p;

    let theme_buttons = div()
        .flex()
        .flex_row()
        .gap(px(8.0))
        .children(
            [
                (ThemePreference::System, "System"),
                (ThemePreference::Dark, "Dark"),
                (ThemePreference::Light, "Light"),
            ]
            .into_iter()
            .map(|(pref, label)| {
                let selected = settings.theme == pref;
                let hover = p.control_hover;
                div()
                    .id(SharedString::from(format!("theme-pref-{label}")))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(88.0))
                    .h(px(32.0))
                    .rounded(radius::CONTROL)
                    .bg(if selected { p.accent } else { p.raised })
                    .text_color(if selected { p.on_accent } else { p.text })
                    .text_size(size::BODY)
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .when(!selected, |el| el.hover(move |s| s.bg(hover)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_settings(cx, |s| s.theme = pref);
                    }))
                    .child(label)
            }),
        );

    let e_cap = entity.clone();
    let cur_cap = settings.capture_shortcuts;
    let capture_dropdown = widgets::dropdown_select(
        "settings-capture",
        capture_label(cur_cap),
        p,
        move |_state, _window, cx| {
            let state_entity = cx.entity().clone();
            let mut list = div().flex().flex_col().gap(px(2.0)).w_full();
            for option in [
                ShortcutCapture::Fullscreen,
                ShortcutCapture::Always,
                ShortcutCapture::Never,
            ] {
                let e = e_cap.clone();
                let se = state_entity.clone();
                list = list.child(widgets::menu_item(
                    SharedString::from(format!("cap-opt-{}", capture_label(option))),
                    capture_label(option),
                    None,
                    cur_cap == option,
                    false,
                    &palette,
                    move |_, window, cx| {
                        e.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.capture_shortcuts = option);
                        });
                        se.update(cx, |s, cx| s.dismiss(window, cx));
                    },
                ));
            }
            list
        },
    );

    let e_spd = entity.clone();
    let cur_spd = settings.touchpad_scroll_speed;
    let speed_dropdown = div().w(px(108.0)).child(widgets::dropdown_select(
        "settings-scroll-speed",
        speed_label(cur_spd),
        p,
        move |_state, _window, cx| {
            let state_entity = cx.entity().clone();
            let mut list = div().flex().flex_col().gap(px(2.0)).w_full();
            for speed in SCROLL_SPEEDS {
                let e = e_spd.clone();
                let se = state_entity.clone();
                let lbl = speed_label(speed);
                list = list.child(widgets::menu_item(
                    SharedString::from(format!("spd-opt-{lbl}")),
                    lbl,
                    None,
                    (cur_spd - speed).abs() < f32::EPSILON,
                    false,
                    &palette,
                    move |_, window, cx| {
                        e.update(cx, |app, cx| {
                            app.update_settings(cx, |s| s.touchpad_scroll_speed = speed);
                        });
                        se.update(cx, |s, cx| s.dismiss(window, cx));
                    },
                ));
            }
            list
        },
    ));

    let e_rm = entity.clone();
    let e_dp = entity.clone();
    let e_tm = entity.clone();
    let e_ll = entity.clone();
    let e_hw = entity.clone();

    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .w_full()
        .pr(px(8.0))
        // Appearance
        .child(widgets::section_label("Appearance", p))
        .child(theme_buttons)
        .child(widgets::setting_row(
            "Reduce motion",
            Some("Turn off interface animations."),
            p,
            widgets::toggle("set-reduce-motion", settings.reduce_motion, p, move |on, _, cx| {
                let on = *on;
                e_rm.update(cx, |app, cx| app.update_settings(cx, |s| s.reduce_motion = on));
            }),
        ))
        .child(widgets::setting_row(
            "Desktop previews",
            Some("Show a snapshot of each host's screen on its tile. Saved unencrypted in Relay's data folder; turning this off deletes them."),
            p,
            widgets::toggle("set-desktop-previews", settings.desktop_previews, p, move |on, _, cx| {
                let on = *on;
                e_dp.update(cx, |app, cx| app.update_settings(cx, |s| s.desktop_previews = on));
            }),
        ))
        // Keyboard
        .child(div().mt(px(8.0)).child(widgets::section_label("Keyboard", p)))
        .child(
            div()
                .text_size(size::BODY)
                .text_color(p.text)
                .child("Send system shortcuts (Super, Alt+Tab) to the remote computer"),
        )
        .child(capture_dropdown)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .mt(px(4.0))
                .children(
                    [
                        ("Ctrl+Alt+Enter", "Toggle fullscreen"),
                        ("Ctrl+Alt+Home", "Show the session bar"),
                        ("Ctrl+Alt+End", "Send Ctrl+Alt+Del"),
                    ]
                    .into_iter()
                    .map(|(keys, what)| {
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(16.0))
                            .child(
                                div()
                                    .w(px(130.0))
                                    .font_family(FONT_MONO)
                                    .text_size(size::SMALL)
                                    .text_color(p.text)
                                    .child(keys),
                            )
                            .child(div().text_size(size::SMALL).text_color(p.muted).child(what))
                    }),
                ),
        )
        // Touchpad
        .child(div().mt(px(8.0)).child(widgets::section_label("Touchpad", p)))
        .child(widgets::setting_row(
            "Scroll momentum",
            Some("Keep scrolling briefly after you lift your fingers, as local apps do."),
            p,
            widgets::toggle("set-momentum", settings.touchpad_momentum, p, move |on, _, cx| {
                let on = *on;
                e_tm.update(cx, |app, cx| app.update_settings(cx, |s| s.touchpad_momentum = on));
            }),
        ))
        .child(widgets::setting_row(
            "Scroll speed",
            Some("How far the remote page scrolls for each swipe."),
            p,
            speed_dropdown,
        ))
        // Performance
        .child(div().mt(px(8.0)).child(widgets::section_label("Performance", p)))
        .child(widgets::setting_row(
            "Lowest latency",
            Some("Queue one frame instead of two. Applies after restarting Relay."),
            p,
            widgets::toggle("set-low-latency", settings.low_latency, p, move |on, _, cx| {
                let on = *on;
                e_ll.update(cx, |app, cx| app.update_settings(cx, |s| s.low_latency = on));
            }),
        ))
        .child(widgets::setting_row(
            "Hardware video decoding",
            Some("Use the GPU for H.264 video and to draw the desktop. Falls back automatically. Applies to new connections and reconnects."),
            p,
            widgets::toggle("set-hw-video", settings.hardware_video_decoding, p, move |on, _, cx| {
                let on = *on;
                e_hw.update(cx, |app, cx| {
                    app.update_settings(cx, |s| s.hardware_video_decoding = on)
                });
            }),
        ))
        .into_any_element()
}
