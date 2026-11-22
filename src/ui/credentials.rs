//! Credential profile management and account selectors for the host editor and sign-in.

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement,
    Styled, Window, div, prelude::FluentBuilder as _, px,
};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::theme::{Palette, size};
use super::widgets;
use crate::store::credentials::{CredentialProfile, CredentialStore};
use crate::store::profiles::HostProfile;

pub enum Action {
    Save {
        profile: CredentialProfile,
        password: Option<Zeroizing<String>>,
    },
    Delete(Uuid),
}

pub struct Draft {
    pub profile: CredentialProfile,
    pub name_input: Entity<InputState>,
    pub user_input: Entity<InputState>,
    pub domain_input: Entity<InputState>,
    pub password_input: Entity<InputState>,
}

impl Draft {
    pub fn new(profile: CredentialProfile, is_new: bool, window: &mut Window, cx: &mut App) -> Self {
        let name_val = profile.name.clone();
        let user_val = profile.username.clone();
        let domain_val = profile.domain.clone();
        let pw_placeholder = if is_new {
            "Optional — asked when you connect"
        } else {
            "Leave blank to keep saved password"
        };

        let name_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("Office account");
            s.set_value(name_val, window, cx);
            s
        });
        let user_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("jdoe or CORP\\jdoe");
            s.set_value(user_val, window, cx);
            s
        });
        let domain_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("Optional");
            s.set_value(domain_val, window, cx);
            s
        });
        let password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(pw_placeholder)
                .masked(true)
        });

        Self {
            profile,
            name_input,
            user_input,
            domain_input,
            password_input,
        }
    }

    pub fn collect(&mut self, cx: &App) -> (CredentialProfile, Option<Zeroizing<String>>) {
        self.profile.name = self.name_input.read(cx).value().to_string();
        self.profile.username = self.user_input.read(cx).value().to_string();
        self.profile.domain = self.domain_input.read(cx).value().to_string();
        let pw = self.password_input.read(cx).value().to_string();
        let password = (!pw.is_empty()).then(|| Zeroizing::new(pw));
        (self.profile.clone(), password)
    }
}

#[derive(Default)]
pub struct Manager {
    pub draft: Option<Draft>,
    pub deleting: Option<Uuid>,
    pub busy: bool,
    pub error: Option<String>,
}

impl Manager {
    pub fn finish(&mut self, result: anyhow::Result<()>) {
        self.busy = false;
        match result {
            Ok(()) => {
                self.draft = None;
                self.deleting = None;
                self.error = None;
            }
            Err(error) => self.error = Some(format!("Couldn't save credential changes: {error}")),
        }
    }
}

/// Renders the credential profile management UI inside the Passwords drawer.
#[allow(clippy::too_many_arguments)]
pub fn render_manager<V: 'static>(
    manager: &Manager,
    store: &CredentialStore,
    hosts: &[HostProfile],
    p: &Palette,
    on_edit: impl Fn(CredentialProfile, bool, &mut Window, &mut App) + Clone + 'static,
    on_save: impl Fn(&mut Window, &mut App) + Clone + 'static,
    on_cancel: impl Fn(&mut Window, &mut App) + Clone + 'static,
    on_confirm_delete: impl Fn(Uuid, &mut Window, &mut App) + Clone + 'static,
    on_execute_delete: impl Fn(Uuid, &mut Window, &mut App) + Clone + 'static,
    _cx: &mut Context<V>,
) -> AnyElement {
    let busy = manager.busy;
    let mut root = div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .w_full()
        .child(widgets::section_label("Credential profiles", p))
        .child(
            div()
                .text_size(size::SMALL)
                .text_color(p.muted)
                .child("Reuse a sign-in across your hosts. Passwords stay in the system keyring."),
        );

    if let Some(draft) = &manager.draft {
        let on_save = on_save.clone();
        let on_cancel = on_cancel.clone();
        root = root
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .mt(px(6.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(widgets::field_label("Profile name", p))
                            .child(Input::new(&draft.name_input).disabled(busy)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(widgets::field_label("User name", p))
                            .child(Input::new(&draft.user_input).disabled(busy)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(widgets::field_label("Domain", p))
                            .child(Input::new(&draft.domain_input).disabled(busy)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(widgets::field_label("Password", p))
                            .child(Input::new(&draft.password_input).disabled(busy)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .mt(px(4.0))
                            .child(widgets::primary_button(
                                "cred-save",
                                "Save",
                                p,
                                busy,
                                move |_, window, cx| on_save(window, cx),
                            ))
                            .child(widgets::secondary_button(
                                "cred-cancel",
                                "Cancel",
                                p,
                                move |_, window, cx| {
                                    if !busy {
                                        on_cancel(window, cx);
                                    }
                                },
                            )),
                    ),
            );
    } else if let Some(id) = manager.deleting {
        let name = store.get(id).map_or("this profile", |p| p.name.as_str());
        let count = hosts
            .iter()
            .filter(|h| h.credential_profile_id == Some(id))
            .count();
        let on_execute = on_execute_delete.clone();
        let on_cancel = on_cancel.clone();
        root = root.child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .mt(px(6.0))
                .child(
                    div()
                        .text_size(size::BODY)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.text)
                        .child(format!("Delete {name}?")),
                )
                .child(
                    div()
                        .text_size(size::BODY)
                        .text_color(p.muted)
                        .child(format!(
                            "{count} assigned host(s) will be unassigned and ask for credentials next time. Open sessions stay connected."
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .mt(px(4.0))
                        .child(widgets::danger_button(
                            "cred-delete-confirm",
                            "Delete profile",
                            p,
                            move |_, window, cx| {
                                if !busy {
                                    on_execute(id, window, cx);
                                }
                            },
                        ))
                        .child(widgets::secondary_button(
                            "cred-delete-cancel",
                            "Cancel",
                            p,
                            move |_, window, cx| {
                                if !busy {
                                    on_cancel(window, cx);
                                }
                            },
                        )),
                ),
        );
    } else {
        if store.profiles().is_empty() {
            root = root.child(
                div()
                    .mt(px(4.0))
                    .text_size(size::BODY)
                    .text_color(p.muted)
                    .child("No credential profiles yet."),
            );
        }
        for profile in store.profiles() {
            let count = hosts
                .iter()
                .filter(|h| h.credential_profile_id == Some(profile.id))
                .count();
            let prof_clone = profile.clone();
            let id = profile.id;
            let on_edit = on_edit.clone();
            let on_confirm_delete = on_confirm_delete.clone();
            root = root.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .py(px(8.0))
                    .border_b_1()
                    .border_color(p.hairline.opacity(0.6))
                    .child(
                        div()
                            .text_size(size::BODY)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.text)
                            .child(profile.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(size::SMALL)
                            .text_color(p.muted)
                            .child(format!("{} · {count} host(s)", profile.account_label())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .mt(px(4.0))
                            .child(widgets::secondary_button(
                                ("cred-edit", id.as_u128() as usize),
                                "Edit",
                                p,
                                move |_, window, cx| {
                                    if !busy {
                                        on_edit(prof_clone.clone(), false, window, cx);
                                    }
                                },
                            ))
                            .child(widgets::secondary_button(
                                ("cred-del", id.as_u128() as usize),
                                "Delete",
                                p,
                                move |_, window, cx| {
                                    if !busy {
                                        on_confirm_delete(id, window, cx);
                                    }
                                },
                            )),
                    ),
            );
        }
        let on_add = on_edit.clone();
        root = root.child(div().mt(px(6.0)).child(widgets::secondary_button(
            "cred-add",
            "Add profile",
            p,
            move |_, window, cx| {
                if !busy {
                    on_add(CredentialProfile::default(), true, window, cx);
                }
            },
        )));
    }

    root.when(busy, |el| {
        el.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .mt(px(6.0))
                .child(Spinner::new())
                .child(
                    div()
                        .text_size(size::BODY)
                        .text_color(p.muted)
                        .child("Saving credential changes…"),
                ),
        )
    })
    .when_some(manager.error.as_ref(), |el, error| {
        el.child(
            div()
                .mt(px(4.0))
                .text_size(size::BODY)
                .text_color(p.poor)
                .child(error.clone()),
        )
    })
    .into_any_element()
}

/// Credential profile dropdown selector shared by the Host Editor and Sign-in Dialog.
pub fn selector(
    id: impl Into<gpui_kit::ElementId>,
    selected: Option<Uuid>,
    store: &CredentialStore,
    p: &Palette,
    on_select: impl Fn(Option<Uuid>, &mut Window, &mut App) + Clone + 'static,
) -> AnyElement {
    let label = match selected {
        None => "Manual".to_owned(),
        Some(id) => store
            .get(id)
            .map_or("Missing credential profile", |prof| prof.name.as_str())
            .to_owned(),
    };
    let profiles: Vec<(Uuid, String)> = store
        .profiles()
        .iter()
        .map(|prof| (prof.id, prof.name.clone()))
        .collect();
    let palette = *p;

    div()
        .flex()
        .flex_col()
        .w_full()
        .child(widgets::field_label("Credentials", p))
        .child(widgets::dropdown_select(
            id,
            label,
            p,
            move |state, _window, cx| {
                let state_entity = cx.entity().clone();
                let mut list = div().flex().flex_col().gap(px(2.0)).w_full();
                let on_manual = on_select.clone();
                let se = state_entity.clone();
                list = list.child(widgets::menu_item(
                    "cred-opt-manual",
                    "Manual",
                    None,
                    selected.is_none(),
                    false,
                    &palette,
                    move |_, window, cx| {
                        on_manual(None, window, cx);
                        se.update(cx, |s, cx| s.dismiss(window, cx));
                    },
                ));
                for (pid, pname) in &profiles {
                    let pid = *pid;
                    let on_pick = on_select.clone();
                    let se = state_entity.clone();
                    list = list.child(widgets::menu_item(
                        ("cred-opt", pid.as_u128() as usize),
                        pname.clone(),
                        None,
                        selected == Some(pid),
                        false,
                        &palette,
                        move |_, window, cx| {
                            on_pick(Some(pid), window, cx);
                            se.update(cx, |s, cx| s.dismiss(window, cx));
                        },
                    ));
                }
                let _ = state;
                list
            },
        ))
        .into_any_element()
}
