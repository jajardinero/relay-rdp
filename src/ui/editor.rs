//! The host editor: a drawer on the right of the host library.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::{
    Anchor, AnyElement, App, AppContext as _, Context, Entity, Focusable as _, FontWeight,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Window, div, prelude::FluentBuilder as _, px, transparent_black,
};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::RelayApp;
use super::assets::{icon_svg, icons};
use super::credentials as credential_ui;
use super::theme::{FONT_MONO, Palette, radius, size};
use super::widgets;
use crate::store::credentials::CredentialStore;
use crate::store::profiles::{
    AudioMode, GraphicsMode, HostProfile, OsChoice, ProfileStore, Quality, ResolutionMode,
    SharedFolder,
};

pub enum PasswordChange {
    Keep,
    Set(Zeroizing<String>),
    Forget,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Connection,
    Display,
    Experience,
    Devices,
    Advanced,
}

impl Section {
    const ALL: [Section; 5] = [
        Section::Connection,
        Section::Display,
        Section::Experience,
        Section::Devices,
        Section::Advanced,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Connection => "Connection",
            Section::Display => "Display",
            Section::Experience => "Experience",
            Section::Devices => "Devices",
            Section::Advanced => "Advanced",
        }
    }
}

pub struct SharedFolderInputs {
    pub name: Entity<InputState>,
    pub path: Entity<InputState>,
}

pub struct EditorState {
    pub draft: HostProfile,
    pub is_new: bool,
    /// The user asked to forget the stored password.
    pub forget_password: bool,
    pub section: Section,
    pub error: Option<String>,

    pub address_input: Entity<InputState>,
    pub name_input: Entity<InputState>,
    pub port_input: Entity<InputState>,
    pub username_input: Entity<InputState>,
    pub domain_input: Entity<InputState>,
    pub password_input: Entity<InputState>,
    pub group_input: Entity<InputState>,
    pub fixed_width_input: Entity<InputState>,
    pub fixed_height_input: Entity<InputState>,
    pub load_balance_input: Entity<InputState>,
    pub folder_inputs: Vec<SharedFolderInputs>,
    _subs: Vec<Subscription>,
}

impl EditorState {
    pub fn new(profile: HostProfile, window: &mut Window, cx: &mut Context<RelayApp>) -> Self {
        Self::build(profile, true, window, cx)
    }

    pub fn edit(profile: HostProfile, window: &mut Window, cx: &mut Context<RelayApp>) -> Self {
        Self::build(profile, false, window, cx)
    }

    fn build(
        profile: HostProfile,
        is_new: bool,
        window: &mut Window,
        cx: &mut Context<RelayApp>,
    ) -> Self {
        let make_input = |val: String, placeholder: &'static str, masked: bool, window: &mut Window, cx: &mut Context<RelayApp>| {
            cx.new(|cx| {
                let mut s = InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(masked);
                if !val.is_empty() {
                    s.set_value(val, window, cx);
                }
                s
            })
        };

        let address_input = make_input(
            profile.address.clone(),
            "dc01.corp.example.com or 10.0.0.5",
            false,
            window,
            cx,
        );
        let name_input = make_input(
            profile.name.clone(),
            "Shown in your host list",
            false,
            window,
            cx,
        );
        let port_input = make_input(profile.port.to_string(), "3389", false, window, cx);
        let username_input = make_input(profile.username.clone(), "jdoe", false, window, cx);
        let domain_input = make_input(profile.domain.clone(), "Optional", false, window, cx);
        let had_saved = profile.save_password && !is_new;
        let pw_hint = if had_saved {
            "Saved in your keyring — type to replace"
        } else {
            "Asked when you connect"
        };
        let password_input = make_input(String::new(), pw_hint, true, window, cx);
        let group_input = make_input(
            profile.group.clone(),
            "e.g. Office, Plant, Lab",
            false,
            window,
            cx,
        );
        let fixed_width_input = make_input(
            profile.display.fixed_width.to_string(),
            "1920",
            false,
            window,
            cx,
        );
        let fixed_height_input = make_input(
            profile.display.fixed_height.to_string(),
            "1080",
            false,
            window,
            cx,
        );
        let load_balance_input = make_input(
            profile.advanced.load_balance_info.clone(),
            "From the .rdp file of an RDS farm (loadbalanceinfo)",
            false,
            window,
            cx,
        );

        let folder_inputs = profile
            .devices
            .shared_folders
            .iter()
            .map(|f| SharedFolderInputs {
                name: make_input(f.name.clone(), "Drive name", false, window, cx),
                path: make_input(
                    f.path.to_string_lossy().into_owned(),
                    "/home/you/Shared",
                    false,
                    window,
                    cx,
                ),
            })
            .collect();

        address_input.update(cx, |s, cx| s.focus(window, cx));

        let mut subs = Vec::new();
        for inp in [
            &address_input,
            &name_input,
            &port_input,
            &username_input,
            &domain_input,
            &password_input,
            &group_input,
            &fixed_width_input,
            &fixed_height_input,
            &load_balance_input,
        ] {
            subs.push(cx.subscribe_in(inp, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::Change)
                    && let Some(editor) = &mut this.editor
                {
                    editor.error = None;
                    cx.notify();
                }
            }));
        }

        Self {
            draft: profile,
            is_new,
            forget_password: false,
            section: Section::Connection,
            error: None,
            address_input,
            name_input,
            port_input,
            username_input,
            domain_input,
            password_input,
            group_input,
            fixed_width_input,
            fixed_height_input,
            load_balance_input,
            folder_inputs,
            _subs: subs,
        }
    }

    pub fn add_shared_folder(
        &mut self,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<RelayApp>,
    ) {
        let name = path
            .file_name()
            .map_or_else(|| "Shared".to_owned(), |n| n.to_string_lossy().into_owned());
        if !self
            .draft
            .devices
            .shared_folders
            .iter()
            .any(|f| f.path == path)
        {
            let name_input = cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder("Drive name");
                s.set_value(name.clone(), window, cx);
                s
            });
            let path_str = path.to_string_lossy().into_owned();
            let path_input = cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder("/home/you/Shared");
                s.set_value(path_str, window, cx);
                s
            });
            self.draft
                .devices
                .shared_folders
                .push(SharedFolder { name, path });
            self.folder_inputs.push(SharedFolderInputs {
                name: name_input,
                path: path_input,
            });
        }
        self.section = Section::Devices;
    }

    pub fn remove_shared_folder(&mut self, index: usize) {
        if index < self.draft.devices.shared_folders.len() {
            self.draft.devices.shared_folders.remove(index);
            self.folder_inputs.remove(index);
        }
    }

    pub fn profile_id(&self) -> Uuid {
        self.draft.id
    }

    pub fn set_group(&mut self, group: String, window: &mut Window, cx: &mut App) {
        self.draft.group = group.clone();
        self.group_input.update(cx, |s, cx| {
            s.set_value(group, window, cx);
        });
    }

    pub fn set_fixed_size(&mut self, w: u16, h: u16, window: &mut Window, cx: &mut App) {
        self.draft.display.fixed_width = w;
        self.draft.display.fixed_height = h;
        self.fixed_width_input
            .update(cx, |s, cx| s.set_value(w.to_string(), window, cx));
        self.fixed_height_input
            .update(cx, |s, cx| s.set_value(h.to_string(), window, cx));
    }

    pub fn validate(&mut self, cx: &App) -> Option<(HostProfile, PasswordChange)> {
        let mut profile = self.draft.clone();
        profile.address = self.address_input.read(cx).value().trim().to_owned();
        profile.name = self.name_input.read(cx).value().trim().to_owned();
        profile.group = self.group_input.read(cx).value().trim().to_owned();
        profile.username = self.username_input.read(cx).value().to_string();
        profile.domain = self.domain_input.read(cx).value().to_string();
        profile.advanced.load_balance_info = self.load_balance_input.read(cx).value().to_string();

        profile.devices.shared_folders = self
            .folder_inputs
            .iter()
            .map(|fi| SharedFolder {
                name: fi.name.read(cx).value().to_string(),
                path: fi.path.read(cx).value().to_string().into(),
            })
            .collect();

        if profile.address.is_empty() {
            self.error = Some("Enter the computer's address.".to_owned());
            self.section = Section::Connection;
            return None;
        }
        let port_str = self.port_input.read(cx).value();
        match port_str.trim().parse::<u16>() {
            Ok(port) if port > 0 => profile.port = port,
            _ => {
                self.error = Some("The port must be a number from 1 to 65535.".to_owned());
                self.section = Section::Connection;
                return None;
            }
        }
        if profile.name.is_empty() {
            profile.name = profile.address.clone();
        }
        if profile.display.resolution == ResolutionMode::Fixed {
            let w = self
                .fixed_width_input
                .read(cx)
                .value()
                .trim()
                .parse::<u16>()
                .unwrap_or(0);
            let h = self
                .fixed_height_input
                .read(cx)
                .value()
                .trim()
                .parse::<u16>()
                .unwrap_or(0);
            if !(200..=8192).contains(&w) || !(200..=8192).contains(&h) {
                self.error = Some("A fixed resolution must be between 200 × 200 and 8192 × 8192.".to_owned());
                self.section = Section::Display;
                return None;
            }
            profile.display.fixed_width = w;
            profile.display.fixed_height = h;
        }

        let pw_text = self.password_input.read(cx).value().to_string();
        let pw_change = if profile.credential_profile_id.is_some() {
            PasswordChange::Keep
        } else if profile.save_password && !pw_text.is_empty() {
            PasswordChange::Set(Zeroizing::new(pw_text))
        } else if self.forget_password {
            PasswordChange::Forget
        } else {
            PasswordChange::Keep
        };

        Some((profile, pw_change))
    }

    pub fn unassign_credentials(&mut self, id: Uuid, window: &mut Window, cx: &mut App) {
        if self.draft.credential_profile_id == Some(id) {
            self.draft.credential_profile_id = None;
            self.draft.save_password = false;
            self.password_input
                .update(cx, |s, cx| s.set_value("", window, cx));
        }
    }

    pub fn any_input_focused(&self, window: &Window, cx: &App) -> bool {
        [
            &self.address_input,
            &self.name_input,
            &self.port_input,
            &self.username_input,
            &self.domain_input,
            &self.password_input,
            &self.group_input,
            &self.fixed_width_input,
            &self.fixed_height_input,
            &self.load_balance_input,
        ]
        .into_iter()
        .any(|i| i.read(cx).focus_handle(cx).is_focused(window))
            || self.folder_inputs.iter().any(|fi| {
                fi.name.read(cx).focus_handle(cx).is_focused(window)
                    || fi.path.read(cx).focus_handle(cx).is_focused(window)
            })
    }
}

/// Outer width of the editor drawer.
pub const DRAWER_WIDTH: f32 = 468.0;

pub fn render_editor(
    state: &EditorState,
    profiles: &ProfileStore,
    credentials: &CredentialStore,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let title = if state.is_new {
        "New host"
    } else {
        "Edit host"
    };

    let tabs = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(2.0))
        .children(Section::ALL.into_iter().map(|section| {
            let selected = state.section == section;
            let hover = p.hover;
            div()
                .id(SharedString::from(format!("editor-tab-{}", section.label())))
                .flex()
                .items_center()
                .h(px(28.0))
                .px(px(9.0))
                .rounded(radius::CONTROL)
                .bg(if selected {
                    p.raised
                } else {
                    transparent_black()
                })
                .text_size(px(13.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if selected { p.text } else { p.muted })
                .cursor_pointer()
                .when(!selected, |el| el.hover(move |s| s.bg(hover)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(ed) = &mut this.editor {
                        ed.section = section;
                        cx.notify();
                    }
                }))
                .child(section.label())
        }));

    let body = match state.section {
        Section::Connection => render_connection(state, profiles, credentials, p, cx),
        Section::Display => render_display(state, p, cx),
        Section::Experience => render_experience(state, p, cx),
        Section::Devices => render_devices(state, p, cx),
        Section::Advanced => render_advanced(state, p, cx),
    };

    div()
        .flex()
        .flex_col()
        .w(px(DRAWER_WIDTH))
        .h_full()
        .flex_shrink_0()
        .px(px(24.0))
        .py(px(20.0))
        .bg(p.surface)
        .border_l_1()
        .border_color(p.hairline)
        // Header
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .mb(px(12.0))
                .child(
                    div()
                        .text_size(size::SUBHEAD)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text)
                        .child(title),
                )
                .child(widgets::icon_button(
                    "editor-close",
                    icons::X,
                    "Close",
                    p,
                    cx.listener(|this, _, _, cx| {
                        this.editor = None;
                        cx.notify();
                    }),
                )),
        )
        // Section tabs
        .child(div().mb(px(14.0)).child(tabs))
        // Scrollable body
        .child(
            div()
                .id("editor-body-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(div().pr(px(8.0)).pb(px(16.0)).child(body)),
        )
        // Footer
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .pt(px(12.0))
                .border_t_1()
                .border_color(p.hairline.opacity(0.6))
                .when_some(state.error.as_ref(), |el, err| {
                    el.child(
                        div()
                            .text_size(size::BODY)
                            .text_color(p.poor)
                            .child(err.clone()),
                    )
                })
                .child(
                    widgets::button_row()
                        .child(widgets::secondary_button(
                            "editor-save",
                            "Save",
                            p,
                            cx.listener(|this, _, window, cx| {
                                this.save_editor(false, window, cx);
                            }),
                        ))
                        .child(widgets::primary_button(
                            "editor-save-connect",
                            "Save and connect",
                            p,
                            false,
                            cx.listener(|this, _, window, cx| {
                                this.save_editor(true, window, cx);
                            }),
                        )),
                ),
        )
        .into_any_element()
}

fn render_connection(
    state: &EditorState,
    profiles: &ProfileStore,
    credentials: &CredentialStore,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let entity = cx.entity().clone();
    let palette = *p;

    let e_cred = entity.clone();
    let cred_selector = credential_ui::selector(
        "editor-cred-select",
        state.draft.credential_profile_id,
        credentials,
        p,
        move |selected, window, cx| {
            e_cred.update(cx, |app, cx| {
                if let Some(ed) = &mut app.editor {
                    ed.draft.credential_profile_id = selected;
                    ed.password_input
                        .update(cx, |s, cx| s.set_value("", window, cx));
                    cx.notify();
                }
            });
        },
    );

    let sign_in_fields: AnyElement = if let Some(id) = state.draft.credential_profile_id {
        if let Some(prof) = credentials.get(id) {
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(size::BODY)
                        .text_color(p.text)
                        .child(prof.account_label()),
                )
                .child(
                    div()
                        .text_size(size::SMALL)
                        .text_color(p.muted)
                        .child("Manage this account in Passwords in the sidebar."),
                )
                .into_any_element()
        } else {
            div()
                .text_size(size::BODY)
                .text_color(p.poor)
                .child("This profile is missing. Choose another profile or Manual.")
                .into_any_element()
        }
    } else {
        let e_rem = entity.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(widgets::field_label("User name", p))
                            .child(Input::new(&state.username_input)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(widgets::field_label("Domain", p))
                            .child(Input::new(&state.domain_input)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(widgets::field_label("Password", p))
                    .child(Input::new(&state.password_input)),
            )
            .child(widgets::setting_row(
                "Remember password",
                Some("Stored in the system keyring, never in a file."),
                p,
                widgets::toggle(
                    "editor-remember-pw",
                    state.draft.save_password,
                    p,
                    move |on, _, cx| {
                        let on = *on;
                        e_rem.update(cx, |app, cx| {
                            if let Some(ed) = &mut app.editor {
                                let before = ed.draft.save_password;
                                ed.draft.save_password = on;
                                if before && !on {
                                    ed.forget_password = true;
                                }
                                if !before && on {
                                    ed.forget_password = false;
                                }
                                cx.notify();
                            }
                        });
                    },
                ),
            ))
            .into_any_element()
    };

    let groups = profiles.groups();
    let e_grp = entity.clone();
    let group_row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .child(div().flex_1().min_w_0().child(Input::new(&state.group_input)))
        .when(!groups.is_empty(), |el| {
            let groups = groups.clone();
            let raised = p.raised;
            let surface = p.surface;
            let hairline = p.hairline;
            let muted = p.muted;
            let shadow = p.popup_shadow();
            el.child(
                Popover::new("editor-groups-pop")
                    .anchor(Anchor::TopRight)
                    .appearance(false)
                    .trigger(widgets::PopoverTrigger::new(
                        "editor-groups-trigger",
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(34.0))
                            .rounded(radius::CONTROL)
                            .bg(raised)
                            .border_1()
                            .border_color(hairline)
                            .cursor_pointer()
                            .child(icon_svg(icons::CARET_DOWN, px(14.0), muted)),
                    ))
                    .content(move |_state, _window, cx| {
                        let se = cx.entity().clone();
                        let mut list = div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .min_w(px(160.0))
                            .p(px(6.0))
                            .rounded(radius::ROW)
                            .bg(surface)
                            .border_1()
                            .border_color(hairline)
                            .shadow(shadow.clone());
                        for grp in &groups {
                            let g = grp.clone();
                            let e = e_grp.clone();
                            let se = se.clone();
                            list = list.child(widgets::menu_item(
                                SharedString::from(format!("grp-pick-{g}")),
                                g.clone(),
                                None,
                                false,
                                false,
                                &palette,
                                move |_, window, cx| {
                                    e.update(cx, |app, cx| {
                                        if let Some(ed) = &mut app.editor {
                                            ed.set_group(g.clone(), window, cx);
                                            cx.notify();
                                        }
                                    });
                                    se.update(cx, |s, cx| s.dismiss(window, cx));
                                },
                            ));
                        }
                        list
                    }),
            )
        });

    let automatic = match state.draft.detected_os {
        Some(os) => format!("Automatic ({})", os.label()),
        None => "Automatic (not detected yet)".to_owned(),
    };
    let os_label = {
        let automatic = automatic.clone();
        move |choice: OsChoice| match choice {
            OsChoice::Automatic => automatic.clone(),
            OsChoice::Windows => "Windows".to_owned(),
            OsChoice::Linux => "Linux".to_owned(),
            OsChoice::MacOs => "macOS".to_owned(),
        }
    };
    let cur_os = state.draft.os;
    let e_os = entity.clone();
    let os_dropdown = widgets::dropdown_select(
        "editor-os-select",
        os_label(cur_os),
        p,
        move |_state, _window, cx| {
            let se = cx.entity().clone();
            let mut list = div().flex().flex_col().gap(px(2.0)).w_full();
            for choice in [
                OsChoice::Automatic,
                OsChoice::Windows,
                OsChoice::Linux,
                OsChoice::MacOs,
            ] {
                let lbl = os_label(choice);
                let e = e_os.clone();
                let se = se.clone();
                list = list.child(widgets::menu_item(
                    SharedString::from(format!("os-opt-{lbl}")),
                    lbl,
                    None,
                    cur_os == choice,
                    false,
                    &palette,
                    move |_, window, cx| {
                        e.update(cx, |app, cx| {
                            if let Some(ed) = &mut app.editor {
                                ed.draft.os = choice;
                                cx.notify();
                            }
                        });
                        se.update(cx, |s, cx| s.dismiss(window, cx));
                    },
                ));
            }
            list
        },
    );

    let e_fav = entity.clone();

    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .flex()
                .flex_col()
                .child(widgets::field_label("Address", p))
                .child(Input::new(&state.address_input)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(10.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(widgets::field_label("Name", p))
                        .child(Input::new(&state.name_input)),
                )
                .child(
                    div()
                        .w(px(100.0))
                        .child(widgets::field_label("Port", p))
                        .child(Input::new(&state.port_input)),
                ),
        )
        .child(div().mt(px(8.0)).child(widgets::section_label("Sign-in", p)))
        .child(cred_selector)
        .child(sign_in_fields)
        .child(div().mt(px(8.0)).child(widgets::section_label("Organize", p)))
        .child(
            div()
                .flex()
                .flex_col()
                .child(widgets::field_label("Group", p))
                .child(group_row),
        )
        .child(widgets::setting_row(
            "Favorite",
            None,
            p,
            widgets::toggle("editor-fav", state.draft.favorite, p, move |on, _, cx| {
                let on = *on;
                e_fav.update(cx, |app, cx| {
                    if let Some(ed) = &mut app.editor {
                        ed.draft.favorite = on;
                        cx.notify();
                    }
                });
            }),
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .child(widgets::field_label("Operating system", p))
                .child(os_dropdown),
        )
        .into_any_element()
}

fn render_display(
    state: &EditorState,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let display = &state.draft.display;
    let entity = cx.entity().clone();
    let palette = *p;

    let scale_label = |s: Option<u32>| match s {
        None => "Same as this display".to_owned(),
        Some(v) => format!("{v}%"),
    };
    let cur_scale = display.scale_percent;
    let e_scale = entity.clone();
    let scale_dropdown = widgets::dropdown_select(
        "editor-scale-select",
        scale_label(cur_scale),
        p,
        move |_state, _window, cx| {
            let se = cx.entity().clone();
            let mut list = div().flex().flex_col().gap(px(2.0)).w_full();
            for opt in [
                None,
                Some(100),
                Some(125),
                Some(150),
                Some(175),
                Some(200),
                Some(250),
                Some(300),
            ] {
                let lbl = scale_label(opt);
                let e = e_scale.clone();
                let se = se.clone();
                list = list.child(widgets::menu_item(
                    SharedString::from(format!("scale-opt-{lbl}")),
                    lbl,
                    None,
                    cur_scale == opt,
                    false,
                    &palette,
                    move |_, window, cx| {
                        e.update(cx, |app, cx| {
                            if let Some(ed) = &mut app.editor {
                                ed.draft.display.scale_percent = opt;
                                cx.notify();
                            }
                        });
                        se.update(cx, |s, cx| s.dismiss(window, cx));
                    },
                ));
            }
            list
        },
    );

    let e_fs = entity.clone();

    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(widgets::section_label("Resolution", p))
        .child(choice_card(
            "res-match",
            display.resolution == ResolutionMode::MatchWindow,
            "Match the window",
            "The remote desktop resizes with this window, pixel for pixel.",
            p,
            cx.listener(|this, _, _, cx| {
                if let Some(ed) = &mut this.editor {
                    ed.draft.display.resolution = ResolutionMode::MatchWindow;
                    cx.notify();
                }
            }),
        ))
        .child(choice_card(
            "res-fixed",
            display.resolution == ResolutionMode::Fixed,
            "Fixed size",
            "Always use one resolution, scaled down to fit when needed.",
            p,
            cx.listener(|this, _, _, cx| {
                if let Some(ed) = &mut this.editor {
                    ed.draft.display.resolution = ResolutionMode::Fixed;
                    cx.notify();
                }
            }),
        ))
        .when(display.resolution == ResolutionMode::Fixed, |el| {
            el.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .mt(px(4.0))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(div().w(px(100.0)).child(Input::new(&state.fixed_width_input)))
                            .child(div().text_color(p.muted).child("×"))
                            .child(div().w(px(100.0)).child(Input::new(&state.fixed_height_input)))
                            .child(div().text_size(size::SMALL).text_color(p.muted).child("px")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .children([(1920, 1080), (2560, 1440), (1280, 800)].into_iter().map(
                                |(w, h)| {
                                    let hover = p.hover;
                                    div()
                                        .id(("preset", w as usize * 10000 + h as usize))
                                        .px(px(8.0))
                                        .py(px(4.0))
                                        .rounded(radius::CONTROL)
                                        .bg(p.raised)
                                        .text_size(size::SMALL)
                                        .text_color(p.text)
                                        .cursor_pointer()
                                        .hover(move |s| s.bg(hover))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            if let Some(ed) = &mut this.editor {
                                                ed.set_fixed_size(w, h, window, cx);
                                                cx.notify();
                                            }
                                        }))
                                        .child(format!("{w}×{h}"))
                                },
                            )),
                    ),
            )
        })
        .child(div().mt(px(10.0)).child(widgets::section_label("Scaling", p)))
        .child(scale_dropdown)
        .child(div().mt(px(10.0)).child(widgets::setting_row(
            "Start in fullscreen",
            Some("Leave fullscreen with Ctrl+Alt+Enter."),
            p,
            widgets::toggle(
                "editor-start-fs",
                display.start_fullscreen,
                p,
                move |on, _, cx| {
                    let on = *on;
                    e_fs.update(cx, |app, cx| {
                        if let Some(ed) = &mut app.editor {
                            ed.draft.display.start_fullscreen = on;
                            cx.notify();
                        }
                    });
                },
            ),
        )))
        .into_any_element()
}

fn render_experience(
    state: &EditorState,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let exp = &state.draft.experience;
    let entity = cx.entity().clone();
    let e_wp = entity.clone();
    let e_anim = entity.clone();
    let e_font = entity.clone();

    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(widgets::section_label("Quality", p))
        .children(Quality::ALL.into_iter().map(|quality| {
            choice_card(
                SharedString::from(format!("quality-{}", quality.label())),
                exp.quality == quality,
                quality.label(),
                quality.description(),
                p,
                cx.listener(move |this, _, _, cx| {
                    if let Some(ed) = &mut this.editor {
                        ed.draft.experience.quality = quality;
                        quality.apply_visuals(&mut ed.draft.experience);
                        cx.notify();
                    }
                }),
            )
        }))
        .child(
            div()
                .text_size(size::SMALL)
                .text_color(p.muted)
                .child("Also sets the options below. You can still change them."),
        )
        .child(div().mt(px(10.0)).child(widgets::section_label("Graphics", p)))
        .children(GraphicsMode::ALL.into_iter().map(|mode| {
            choice_card(
                SharedString::from(format!("gfx-{}", mode.label())),
                exp.graphics == mode,
                mode.label(),
                mode.description(),
                p,
                cx.listener(move |this, _, _, cx| {
                    if let Some(ed) = &mut this.editor {
                        ed.draft.experience.graphics = mode;
                        cx.notify();
                    }
                }),
            )
        }))
        .child(
            div()
                .mt(px(10.0))
                .child(widgets::section_label("Remote desktop", p)),
        )
        .child(widgets::setting_row(
            "Desktop wallpaper",
            Some("Off saves bandwidth."),
            p,
            widgets::toggle("exp-wallpaper", exp.wallpaper, p, move |on, _, cx| {
                let on = *on;
                e_wp.update(cx, |app, cx| {
                    if let Some(ed) = &mut app.editor {
                        ed.draft.experience.wallpaper = on;
                        cx.notify();
                    }
                });
            }),
        ))
        .child(widgets::setting_row(
            "Window animations",
            None,
            p,
            widgets::toggle("exp-anim", exp.animations, p, move |on, _, cx| {
                let on = *on;
                e_anim.update(cx, |app, cx| {
                    if let Some(ed) = &mut app.editor {
                        ed.draft.experience.animations = on;
                        cx.notify();
                    }
                });
            }),
        ))
        .child(widgets::setting_row(
            "Font smoothing",
            Some("ClearType text on the remote computer."),
            p,
            widgets::toggle("exp-font", exp.font_smoothing, p, move |on, _, cx| {
                let on = *on;
                e_font.update(cx, |app, cx| {
                    if let Some(ed) = &mut app.editor {
                        ed.draft.experience.font_smoothing = on;
                        cx.notify();
                    }
                });
            }),
        ))
        .into_any_element()
}

fn render_devices(
    state: &EditorState,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let devices = &state.draft.devices;
    let entity = cx.entity().clone();
    let palette = *p;
    let e_cb = entity.clone();
    let e_audio = entity.clone();
    let cur_audio = devices.audio;

    let audio_dropdown = widgets::dropdown_select(
        "editor-audio-select",
        cur_audio.label(),
        p,
        move |_state, _window, cx| {
            let se = cx.entity().clone();
            let mut list = div().flex().flex_col().gap(px(2.0)).w_full();
            for mode in AudioMode::ALL {
                let e = e_audio.clone();
                let se = se.clone();
                list = list.child(widgets::menu_item(
                    SharedString::from(format!("audio-opt-{}", mode.label())),
                    mode.label(),
                    None,
                    cur_audio == mode,
                    false,
                    &palette,
                    move |_, window, cx| {
                        e.update(cx, |app, cx| {
                            if let Some(ed) = &mut app.editor {
                                ed.draft.devices.audio = mode;
                                cx.notify();
                            }
                        });
                        se.update(cx, |s, cx| s.dismiss(window, cx));
                    },
                ));
            }
            list
        },
    );

    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(widgets::setting_row(
            "Clipboard",
            Some("Copy and paste text, images and files both ways."),
            p,
            widgets::toggle("dev-clip", devices.clipboard, p, move |on, _, cx| {
                let on = *on;
                e_cb.update(cx, |app, cx| {
                    if let Some(ed) = &mut app.editor {
                        ed.draft.devices.clipboard = on;
                        cx.notify();
                    }
                });
            }),
        ))
        .child(div().mt(px(8.0)).child(widgets::section_label("Sound", p)))
        .child(audio_dropdown)
        .child(
            div()
                .mt(px(8.0))
                .child(widgets::section_label("Shared folders", p)),
        )
        .child(
            div()
                .text_size(size::SMALL)
                .text_color(p.muted)
                .child("These appear as drives under This PC on the remote computer."),
        )
        .children(state.folder_inputs.iter().enumerate().map(|(i, fi)| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .p(px(10.0))
                .rounded(radius::ROW)
                .bg(p.canvas)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(px(6.0))
                        .child(Input::new(&fi.name))
                        .child(div().font_family(FONT_MONO).child(Input::new(&fi.path))),
                )
                .child(widgets::icon_button(
                    ("folder-rm", i),
                    icons::TRASH,
                    "Remove",
                    p,
                    cx.listener(move |this, _, _, cx| {
                        if let Some(ed) = &mut this.editor {
                            ed.remove_shared_folder(i);
                            cx.notify();
                        }
                    }),
                ))
        }))
        .child(
            div()
                .id("editor-add-folder")
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .h(px(34.0))
                .px(px(14.0))
                .rounded(radius::CONTROL)
                .bg(p.raised)
                .text_size(size::BODY)
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.text)
                .cursor_pointer()
                .hover({
                    let hover = p.control_hover;
                    move |s| s.bg(hover)
                })
                .on_click(cx.listener(|this, _, _, _cx| {
                    this.picker.shared_folder();
                }))
                .child(icon_svg(icons::FOLDER_PLUS, px(16.0), p.text))
                .child("Add folder…"),
        )
        .into_any_element()
}

fn render_advanced(
    state: &EditorState,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let adv = &state.draft.advanced;
    let entity = cx.entity().clone();
    let palette = *p;
    let cur_klid = adv.keyboard_layout;
    let current_kb = KEYBOARD_LAYOUTS
        .iter()
        .find(|(id, _)| *id == cur_klid)
        .map_or_else(
            || format!("Custom ({:#06x})", cur_klid),
            |(_, name)| (*name).to_owned(),
        );

    let e_kb = entity.clone();
    let kb_dropdown = widgets::dropdown_select(
        "editor-kb-select",
        current_kb,
        p,
        move |_state, _window, cx| {
            let se = cx.entity().clone();
            let mut list = div()
                .id("kb-layout-scroll")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .w_full()
                .max_h(px(260.0))
                .overflow_y_scrollbar();
            for &(klid, name) in KEYBOARD_LAYOUTS {
                let e = e_kb.clone();
                let se = se.clone();
                list = list.child(widgets::menu_item(
                    ("klid", klid as usize),
                    name,
                    None,
                    cur_klid == klid,
                    false,
                    &palette,
                    move |_, window, cx| {
                        e.update(cx, |app, cx| {
                            if let Some(ed) = &mut app.editor {
                                ed.draft.advanced.keyboard_layout = klid;
                                cx.notify();
                            }
                        });
                        se.update(cx, |s, cx| s.dismiss(window, cx));
                    },
                ));
            }
            div().w_full().child(list)
        },
    );

    let e_rec = entity.clone();
    let e_tls = entity.clone();

    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            div()
                .flex()
                .flex_col()
                .child(widgets::field_label(
                    "Keyboard layout on the remote computer",
                    p,
                ))
                .child(kb_dropdown),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .child(widgets::field_label("Load balancing info", p))
                .child(Input::new(&state.load_balance_input)),
        )
        .child(widgets::setting_row(
            "Reconnect automatically",
            Some("If the network drops, keep trying for up to five minutes."),
            p,
            widgets::toggle("adv-reconn", adv.auto_reconnect, p, move |on, _, cx| {
                let on = *on;
                e_rec.update(cx, |app, cx| {
                    if let Some(ed) = &mut app.editor {
                        ed.draft.advanced.auto_reconnect = on;
                        cx.notify();
                    }
                });
            }),
        ))
        .child(widgets::setting_row(
            "Allow legacy TLS",
            Some("Insecure. For old Windows computers that only offer TLS 1.0. The connection may lack forward secrecy; use only on networks you trust."),
            p,
            widgets::toggle("adv-tls", adv.allow_legacy_tls, p, move |on, _, cx| {
                let on = *on;
                e_tls.update(cx, |app, cx| {
                    if let Some(ed) = &mut app.editor {
                        ed.draft.advanced.allow_legacy_tls = on;
                        cx.notify();
                    }
                });
            }),
        ))
        .into_any_element()
}

fn choice_card(
    id: impl Into<gpui_kit::ElementId>,
    selected: bool,
    title: &str,
    description: &str,
    p: &Palette,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let hover = p.hover;
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_start()
        .gap(px(10.0))
        .w_full()
        .px(px(12.0))
        .py(px(10.0))
        .rounded(radius::ROW)
        .bg(if selected {
            p.accent_soft()
        } else {
            transparent_black()
        })
        .border_1()
        .border_color(if selected { p.accent } else { p.hairline })
        .cursor_pointer()
        .when(!selected, |el| el.hover(move |s| s.bg(hover)))
        .on_click(on_click)
        .child(div().mt(px(2.0)).child(icon_svg(
            if selected {
                icons::RADIO_ON
            } else {
                icons::RADIO_OFF
            },
            px(16.0),
            if selected { p.accent } else { p.muted },
        )))
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
                        .child(title.to_owned()),
                )
                .child(
                    div()
                        .text_size(size::SMALL)
                        .text_color(p.muted)
                        .child(description.to_owned()),
                ),
        )
        .into_any_element()
}

/// Common Windows keyboard layout identifiers (KLID).
const KEYBOARD_LAYOUTS: &[(u32, &str)] = &[
    (0x0409, "English (United States)"),
    (0x0809, "English (United Kingdom)"),
    (0x1009, "English (Canada)"),
    (0x0464, "Filipino"),
    (0x0407, "German"),
    (0x040C, "French"),
    (0x080C, "French (Belgium)"),
    (0x100C, "French (Switzerland)"),
    (0x0410, "Italian"),
    (0x040A, "Spanish"),
    (0x080A, "Spanish (Latin America)"),
    (0x0816, "Portuguese (Portugal)"),
    (0x0416, "Portuguese (Brazil)"),
    (0x0413, "Dutch"),
    (0x041D, "Swedish"),
    (0x0414, "Norwegian"),
    (0x0406, "Danish"),
    (0x040B, "Finnish"),
    (0x0415, "Polish"),
    (0x0405, "Czech"),
    (0x0419, "Russian"),
    (0x0422, "Ukrainian"),
    (0x041F, "Turkish"),
    (0x0411, "Japanese"),
    (0x0412, "Korean"),
    (0x0804, "Chinese (Simplified)"),
    (0x0404, "Chinese (Traditional)"),
];
