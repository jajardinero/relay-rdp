//! Credential profile management and account selectors for the host editor and sign-in.

use egui::{RichText, Ui};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    theme::{self, Palette, size},
    widgets,
};
use crate::store::{
    credentials::{CredentialProfile, CredentialStore},
    profiles::HostProfile,
};

pub enum Action {
    Save {
        profile: CredentialProfile,
        password: Option<Zeroizing<String>>,
    },
    Delete(Uuid),
}

pub struct Draft {
    pub profile: CredentialProfile,
    pub password: Zeroizing<String>,
    pub is_new: bool,
}

impl Draft {
    fn new(profile: CredentialProfile, is_new: bool) -> Self {
        Self {
            profile,
            password: Zeroizing::new(String::new()),
            is_new,
        }
    }
}

#[derive(Default)]
pub struct Manager {
    pub draft: Option<Draft>,
    deleting: Option<Uuid>,
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

    pub fn show(
        &mut self,
        ui: &mut Ui,
        store: &CredentialStore,
        hosts: &[HostProfile],
    ) -> Option<Action> {
        let p = Palette::of(ui);
        widgets::section_label(ui, "Credential profiles");
        ui.label(
            RichText::new(
                "Reuse a sign-in across your hosts. Passwords stay in the system keyring.",
            )
            .size(size::SMALL)
            .color(p.muted),
        );
        let mut action = None;
        let mut cancel = false;
        ui.add_enabled_ui(!self.busy, |ui| {
            if let Some(draft) = &mut self.draft {
                ui.add_space(6.0);
                widgets::field(ui, "Profile name", &mut draft.profile.name, "Office account");
                widgets::field(ui, "User name", &mut draft.profile.username, "jdoe or CORP\\jdoe");
                widgets::field(ui, "Domain", &mut draft.profile.domain, "Optional");
                ui.label(RichText::new("Password").size(size::SMALL).color(p.muted));
                widgets::text_input(ui, &mut draft.password, if draft.is_new { "Optional — asked when you connect" } else { "Leave blank to keep saved password" }, true);
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, "Save").clicked() {
                        match draft.profile.validate() {
                            Ok(()) => action = Some(Action::Save {
                                profile: draft.profile.clone(),
                                password: (!draft.password.is_empty()).then(|| draft.password.clone()),
                            }),
                            Err(error) => self.error = Some(error.to_string()),
                        }
                    }
                    if widgets::secondary_button(ui, "Cancel").clicked() {
                        cancel = true;
                    }
                });
            } else if let Some(id) = self.deleting {
                let name = store.get(id).map_or("this profile", |p| p.name.as_str());
                let count = hosts.iter().filter(|h| h.credential_profile_id == Some(id)).count();
                ui.label(RichText::new(format!("Delete {name}?")).font(theme::font_medium(size::BODY)));
                ui.label(format!("{count} assigned host(s) will be unassigned and ask for credentials next time. Open sessions stay connected."));
                ui.horizontal(|ui| {
                    if widgets::danger_button(ui, "Delete profile").clicked() {
                        action = Some(Action::Delete(id));
                    }
                    if widgets::secondary_button(ui, "Cancel").clicked() {
                        self.deleting = None;
                        self.error = None;
                    }
                });
            } else {
                if store.profiles().is_empty() {
                    ui.label(RichText::new("No credential profiles yet.").color(p.muted));
                }
                for profile in store.profiles() {
                    ui.push_id(profile.id, |ui| {
                        let count = hosts.iter().filter(|h| h.credential_profile_id == Some(profile.id)).count();
                        ui.label(RichText::new(&profile.name).font(theme::font_medium(size::BODY)));
                        ui.label(RichText::new(format!("{} · {count} host(s)", profile.account_label())).size(size::SMALL).color(p.muted));
                        ui.horizontal(|ui| {
                            if widgets::secondary_button(ui, "Edit").clicked() {
                                self.draft = Some(Draft::new(profile.clone(), false));
                                self.error = None;
                            }
                            if widgets::secondary_button(ui, "Delete").clicked() {
                                self.deleting = Some(profile.id);
                                self.error = None;
                            }
                        });
                        ui.add_space(6.0);
                    });
                }
                if widgets::secondary_button(ui, "Add profile").clicked() {
                    self.draft = Some(Draft::new(CredentialProfile::default(), true));
                    self.error = None;
                }
            }
        });
        if cancel {
            self.draft = None;
            self.error = None;
        }
        if self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Saving credential changes…");
            });
        }
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(p.poor));
        }
        action
    }
}

/// Returns true when the assignment changes. A missing link stays visible until replaced.
pub fn selector(ui: &mut Ui, selected: &mut Option<Uuid>, store: &CredentialStore) -> bool {
    let before = *selected;
    let label = match *selected {
        None => "Manual",
        Some(id) => store
            .get(id)
            .map_or("Missing credential profile", |p| p.name.as_str()),
    };
    ui.label(
        RichText::new("Credentials")
            .size(size::SMALL)
            .color(Palette::of(ui).muted),
    );
    egui::ComboBox::from_id_salt("credential-profile")
        .width(ui.available_width())
        .selected_text(label)
        .show_ui(ui, |ui| {
            ui.selectable_value(selected, None, "Manual");
            for profile in store.profiles() {
                ui.selectable_value(selected, Some(profile.id), &profile.name);
            }
        });
    before != *selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Context, Event, Modifiers, PointerButton, Pos2, Rect, vec2};

    struct Harness {
        ctx: Context,
        manager: Manager,
        store: CredentialStore,
        hosts: Vec<HostProfile>,
        selected: Option<Uuid>,
        selector: bool,
        shapes: Vec<egui::epaint::ClippedShape>,
    }

    impl Harness {
        fn new() -> Self {
            let ctx = Context::default();
            theme::install_fonts(&ctx);
            Self {
                ctx,
                manager: Manager::default(),
                store: CredentialStore::default(),
                hosts: vec![],
                selected: None,
                selector: false,
                shapes: vec![],
            }
        }

        fn frame(&mut self, events: Vec<Event>) -> Option<Action> {
            let mut action = None;
            let mut output = self.ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(488.0, 800.0))),
                    events,
                    ..Default::default()
                },
                |root| {
                    theme::apply(root.ctx(), &theme::DARK, true);
                    egui::CentralPanel::default().show(root, |ui| {
                        if self.selector {
                            selector(ui, &mut self.selected, &self.store);
                        } else {
                            action = self.manager.show(ui, &self.store, &self.hosts);
                        }
                    });
                },
            );
            output.textures_delta.clear();
            self.shapes = output.shapes;
            action
        }

        fn text(&self, label: &str) -> Pos2 {
            self.shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == label => {
                        Some(text.pos + text.galley.size() / 2.0)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing label: {label}"))
        }

        fn click(&mut self, label: &str) -> Option<Action> {
            self.frame(vec![]);
            let pos = self.text(label);
            self.frame(vec![Event::PointerMoved(pos)]);
            self.frame(vec![Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            }]);
            self.frame(vec![Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }])
        }
    }

    #[test]
    fn manager_adds_edits_confirms_deletion_and_keeps_failed_drafts() {
        let mut h = Harness::new();
        h.click("Add profile");
        assert!(h.manager.draft.is_some());
        assert!(h.click("Save").is_none());
        assert!(h.manager.error.is_some());
        let draft = h.manager.draft.as_mut().unwrap();
        draft.profile.name = "Office".into();
        draft.profile.username = "alice".into();
        *draft.password = "private-secret".into();
        let id = draft.profile.id;
        assert!(matches!(
            h.click("Save"),
            Some(Action::Save {
                password: Some(_),
                ..
            })
        ));
        assert!(
            h.store.profiles().is_empty(),
            "UI submission must wait for acknowledgement"
        );
        h.manager.busy = true;
        h.manager.finish(Err(anyhow::anyhow!("keyring locked")));
        assert_eq!(
            &**h.manager.draft.as_ref().unwrap().password,
            "private-secret"
        );
        assert!(
            h.manager
                .error
                .as_deref()
                .unwrap()
                .contains("keyring locked")
        );
        let Some(Action::Save { profile, .. }) = h.click("Save") else {
            panic!("retry should submit")
        };
        h.store.upsert(profile).unwrap();
        h.manager.finish(Ok(()));
        assert!(h.manager.draft.is_none());
        let mut host = HostProfile::new("a", 3389);
        host.credential_profile_id = Some(id);
        h.hosts = vec![host.clone(), host];
        h.frame(vec![]);
        h.text("alice · 2 host(s)");
        h.click("Edit");
        assert!(!h.manager.draft.as_ref().unwrap().is_new);
        assert!(h.manager.draft.as_ref().unwrap().password.is_empty());
        assert!(matches!(
            h.click("Save"),
            Some(Action::Save { password: None, .. })
        ));
        h.manager.finish(Ok(()));
        h.click("Delete");
        assert_eq!(h.manager.deleting, Some(id));
        h.frame(vec![]);
        assert!(h.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text().starts_with("2 assigned host(s)"))));
        h.click("Cancel");
        assert!(h.store.get(id).is_some());
        h.click("Delete");
        assert!(
            matches!(h.click("Delete profile"), Some(Action::Delete(deleted)) if deleted == id)
        );
    }

    #[test]
    fn selector_assigns_profiles_and_returns_to_manual() {
        let mut h = Harness::new();
        let profile = CredentialProfile {
            name: "Office".into(),
            username: "alice".into(),
            ..Default::default()
        };
        let id = profile.id;
        h.store.upsert(profile).unwrap();
        h.selector = true;
        h.click("Manual");
        h.click("Office");
        assert_eq!(h.selected, Some(id));
        h.click("Office");
        h.click("Manual");
        assert_eq!(h.selected, None);
        h.selected = Some(Uuid::new_v4());
        h.frame(vec![]);
        h.text("Missing credential profile");
        assert!(
            h.selected.is_some(),
            "a missing reference must not silently become Manual"
        );
    }
}
