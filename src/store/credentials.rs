//! Reusable account metadata. Passwords are stored separately in the desktop keyring.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::profiles::HostProfile;
use super::secrets::SecretId;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialProfile {
    pub id: Uuid,
    pub name: String,
    pub username: String,
    #[serde(default)]
    pub domain: String,
}

impl Default for CredentialProfile {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            name: String::new(),
            username: String::new(),
            domain: String::new(),
        }
    }
}

impl CredentialProfile {
    pub fn account_label(&self) -> String {
        if self.domain.is_empty() {
            self.username.clone()
        } else {
            format!("{}\\{}", self.domain, self.username)
        }
    }

    pub fn validate(&mut self) -> anyhow::Result<()> {
        self.name = self.name.trim().to_owned();
        self.username = self.username.trim().to_owned();
        self.domain = self.domain.trim().to_owned();
        anyhow::ensure!(!self.name.is_empty(), "Enter a profile name.");
        anyhow::ensure!(!self.username.is_empty(), "Enter a user name.");
        Ok(())
    }
}

#[derive(Default, Serialize, Deserialize)]
struct CredentialsFile {
    #[serde(default, rename = "credential")]
    profiles: Vec<CredentialProfile>,
}

#[derive(Default)]
pub struct CredentialStore {
    path: Option<PathBuf>,
    profiles: Vec<CredentialProfile>,
}

impl CredentialStore {
    pub fn load() -> Self {
        let path = super::paths::config_dir().map(|d| d.join("credentials.toml"));
        let profiles = path
            .as_deref()
            .map(|p| super::load_toml::<CredentialsFile>(p).profiles)
            .unwrap_or_default();
        Self { path, profiles }
    }

    pub fn profiles(&self) -> &[CredentialProfile] {
        &self.profiles
    }

    pub fn get(&self, id: Uuid) -> Option<&CredentialProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn upsert(&mut self, mut profile: CredentialProfile) -> anyhow::Result<()> {
        profile.validate()?;
        let mut profiles = self.profiles.clone();
        if let Some(existing) = profiles.iter_mut().find(|p| p.id == profile.id) {
            *existing = profile;
        } else {
            profiles.push(profile);
        }
        self.commit(profiles)
    }

    pub fn remove(&mut self, id: Uuid) -> anyhow::Result<()> {
        let profiles = self
            .profiles
            .iter()
            .filter(|p| p.id != id)
            .cloned()
            .collect();
        self.commit(profiles)
    }

    fn commit(&mut self, profiles: Vec<CredentialProfile>) -> anyhow::Result<()> {
        if let Some(path) = &self.path {
            super::save_toml(
                path,
                &CredentialsFile {
                    profiles: profiles.clone(),
                },
            )?;
        } else if !cfg!(test) {
            anyhow::bail!("The configuration directory is unavailable.");
        }
        self.profiles = profiles;
        Ok(())
    }

    /// Resolve a connection snapshot; never mutate the host's manual credentials.
    pub fn resolve(&self, host: &HostProfile) -> anyhow::Result<HostProfile> {
        let mut snapshot = host.clone();
        if let Some(id) = host.credential_profile_id {
            let profile = self.get(id).ok_or_else(|| {
                anyhow::anyhow!(
                    "The assigned credential profile is missing. Choose another profile or Manual."
                )
            })?;
            snapshot.username.clone_from(&profile.username);
            snapshot.domain.clone_from(&profile.domain);
        }
        Ok(snapshot)
    }

    pub fn account_label(&self, host: &HostProfile) -> String {
        match host.credential_profile_id {
            Some(id) => self.get(id).map_or_else(
                || "Missing credential profile".to_owned(),
                CredentialProfile::account_label,
            ),
            None => host.account_label(),
        }
    }

    pub fn connection(
        &self,
        host: &HostProfile,
        saved: bool,
    ) -> anyhow::Result<(HostProfile, Option<SecretId>)> {
        let snapshot = self.resolve(host)?;
        let target = host
            .credential_profile_id
            .map(SecretId::Credential)
            .or_else(|| (saved && host.save_password).then_some(SecretId::Host(host.id)))
            .filter(|_| !snapshot.username.trim().is_empty());
        Ok((snapshot, target))
    }

    pub fn for_launch(
        &self,
        host: &HostProfile,
        user: Option<String>,
        explicit_password: bool,
    ) -> anyhow::Result<HostProfile> {
        if let Some(user) = user {
            let mut snapshot = host.clone();
            snapshot.username = user;
            snapshot.credential_profile_id = None;
            snapshot.save_password = false;
            Ok(snapshot)
        } else if explicit_password {
            self.resolve(host)
        } else {
            Ok(host.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_hosts_resolve_current_credentials_without_changing_manual_fields() {
        let mut store = CredentialStore::default();
        let mut credential = CredentialProfile {
            name: "Office".into(),
            username: "alice".into(),
            domain: "CORP".into(),
            ..Default::default()
        };
        store.upsert(credential.clone()).unwrap();
        let mut a = HostProfile::new("a", 3389);
        a.username = "local".into();
        a.credential_profile_id = Some(credential.id);
        let mut b = HostProfile::new("b", 3389);
        b.credential_profile_id = Some(credential.id);
        let original_snapshot = store.resolve(&a).unwrap();
        credential.username = "bob".into();
        store.upsert(credential.clone()).unwrap();
        assert_eq!(store.resolve(&a).unwrap().username, "bob");
        assert_eq!(store.resolve(&b).unwrap().username, "bob");
        assert_eq!(original_snapshot.username, "alice");
        assert_eq!(a.username, "local");
        assert_eq!(store.account_label(&a), "CORP\\bob");
        store.remove(credential.id).unwrap();
        assert!(store.resolve(&a).is_err());
        a.credential_profile_id = None;
        assert_eq!(store.resolve(&a).unwrap().username, "local");
    }

    #[test]
    fn metadata_round_trip_contains_no_passwords() {
        let path = std::env::temp_dir().join(format!("relay-credentials-{}.toml", Uuid::new_v4()));
        let mut store = CredentialStore {
            path: Some(path.clone()),
            ..Default::default()
        };
        let profile = CredentialProfile {
            name: "Office".into(),
            username: "alice".into(),
            ..Default::default()
        };
        store.upsert(profile.clone()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("password"));
        let file: CredentialsFile = toml::from_str(&text).unwrap();
        assert_eq!(file.profiles, vec![profile]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_metadata_write_keeps_existing_profiles() {
        let mut store = CredentialStore {
            path: Some(PathBuf::from("/dev/null/credentials.toml")),
            ..Default::default()
        };
        assert!(
            store
                .upsert(CredentialProfile {
                    name: "Office".into(),
                    username: "alice".into(),
                    ..Default::default()
                })
                .is_err()
        );
        assert!(store.profiles().is_empty());
        assert!(CredentialProfile::default().validate().is_err());
    }

    #[test]
    fn connection_targets_never_fall_back_from_a_link_to_a_host_password() {
        let mut store = CredentialStore::default();
        let credential = CredentialProfile {
            name: "Office".into(),
            username: "alice".into(),
            ..Default::default()
        };
        store.upsert(credential.clone()).unwrap();
        let mut host = HostProfile::new("host", 3389);
        host.username = "local".into();
        host.save_password = true;
        assert_eq!(
            store.connection(&host, true).unwrap().1,
            Some(SecretId::Host(host.id))
        );
        assert_eq!(store.connection(&host, false).unwrap().1, None);
        host.credential_profile_id = Some(credential.id);
        for saved in [true, false] {
            assert_eq!(
                store.connection(&host, saved).unwrap().1,
                Some(SecretId::Credential(credential.id))
            );
        }
        store.remove(credential.id).unwrap();
        assert!(store.connection(&host, true).is_err());
        assert!(store.connection(&host, false).is_err());
    }

    #[test]
    fn explicit_launch_credentials_take_precedence_without_updating_saved_profiles() {
        let mut store = CredentialStore::default();
        let credential = CredentialProfile {
            name: "Office".into(),
            username: "alice".into(),
            ..Default::default()
        };
        store.upsert(credential.clone()).unwrap();
        let mut host = HostProfile::new("host", 3389);
        host.username = "local".into();
        host.credential_profile_id = Some(credential.id);
        let password_override = store.for_launch(&host, None, true).unwrap();
        assert_eq!(password_override.username, "alice");
        let user_override = store
            .for_launch(&host, Some("other".into()), false)
            .unwrap();
        assert_eq!(user_override.username, "other");
        assert_eq!(store.connection(&user_override, true).unwrap().1, None);
        assert_eq!(host.username, "local");
        assert_eq!(host.credential_profile_id, Some(credential.id));
        assert_eq!(store.get(credential.id), Some(&credential));
    }
}
