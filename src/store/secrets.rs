//! Passwords live in the desktop keyring (Secret Service: gnome-keyring, KWallet, …).
//!
//! Every call here talks to D-Bus and may block (e.g. while the keyring asks to be unlocked),
//! so callers must stay off the UI thread.

use uuid::Uuid;
use zeroize::Zeroizing;

use std::sync::mpsc::{self, Receiver, Sender};

const SERVICE: &str = "relay-rdp";

fn entry(profile: Uuid) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &profile.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SecretId {
    Host(Uuid),
    Credential(Uuid),
}

impl SecretId {
    fn key(self) -> String {
        match self {
            Self::Host(id) => id.to_string(), // Preserve existing host keyring entries.
            Self::Credential(id) => format!("credential:{id}"),
        }
    }
}

pub trait SecretBackend: Send + 'static {
    fn load(&mut self, id: SecretId) -> anyhow::Result<Option<Zeroizing<String>>>;
    fn save(&mut self, id: SecretId, password: &str) -> anyhow::Result<()>;
    fn delete(&mut self, id: SecretId) -> anyhow::Result<()>;
}

pub struct DesktopKeyring;

impl SecretBackend for DesktopKeyring {
    fn load(&mut self, id: SecretId) -> anyhow::Result<Option<Zeroizing<String>>> {
        if let SecretId::Host(id) = id {
            return load_password(id);
        }
        match keyring::Entry::new(SERVICE, &id.key())?.get_password() {
            Ok(password) => Ok(Some(Zeroizing::new(password))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&mut self, id: SecretId, password: &str) -> anyhow::Result<()> {
        if let SecretId::Host(id) = id {
            return save_password(id, password);
        }
        keyring::Entry::new(SERVICE, &id.key())?.set_password(password)?;
        Ok(())
    }

    fn delete(&mut self, id: SecretId) -> anyhow::Result<()> {
        match keyring::Entry::new(SERVICE, &id.key())?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

pub enum SecretOperation {
    Load(SecretId),
    Save(SecretId, Zeroizing<String>),
    Delete(SecretId),
    /// A metadata-only save still completes through the same acknowledgement path.
    Keep,
}

pub struct SecretCompletion {
    pub request: Uuid,
    pub result: anyhow::Result<Option<Zeroizing<String>>>,
}

/// A single worker serializes keyring operations so a following load sees a saved password.
pub struct SecretWorker {
    tx: Sender<(Uuid, SecretOperation)>,
    rx: Receiver<SecretCompletion>,
}

impl SecretWorker {
    pub fn new(mut backend: impl SecretBackend, wake: impl Fn() + Send + 'static) -> Self {
        let (tx, requests) = mpsc::channel::<(Uuid, SecretOperation)>();
        let (results, rx) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok((request, operation)) = requests.recv() {
                let result = match operation {
                    SecretOperation::Load(id) => backend.load(id),
                    SecretOperation::Save(id, password) => {
                        backend.save(id, &password).map(|()| None)
                    }
                    SecretOperation::Delete(id) => backend.delete(id).map(|()| None),
                    SecretOperation::Keep => Ok(None),
                };
                if results.send(SecretCompletion { request, result }).is_err() {
                    break;
                }
                wake();
            }
        });
        Self { tx, rx }
    }

    pub fn submit(&self, operation: SecretOperation) -> Uuid {
        let request = Uuid::new_v4();
        // The worker lives until all senders are dropped and never disconnects normally.
        self.tx
            .send((request, operation))
            .expect("secret worker disconnected");
        request
    }

    pub fn try_recv(&self) -> Option<SecretCompletion> {
        self.rx.try_recv().ok()
    }
}

/// Returns `Ok(None)` when no password is stored for this host.
pub fn load_password(profile: Uuid) -> anyhow::Result<Option<Zeroizing<String>>> {
    match entry(profile)?.get_password() {
        Ok(password) => Ok(Some(Zeroizing::new(password))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn save_password(profile: Uuid, password: &str) -> anyhow::Result<()> {
    entry(profile)?.set_password(password)?;
    Ok(())
}

pub fn delete_password(profile: Uuid) -> anyhow::Result<()> {
    match entry(profile)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_password_in_background(profile: Uuid) {
    std::thread::spawn(move || {
        if let Err(e) = delete_password(profile) {
            tracing::warn!(error = %e, "failed to delete password from the keyring");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    #[derive(Default)]
    struct FakeKeyring(HashMap<SecretId, Zeroizing<String>>);

    impl SecretBackend for FakeKeyring {
        fn load(&mut self, id: SecretId) -> anyhow::Result<Option<Zeroizing<String>>> {
            anyhow::ensure!(id != SecretId::Credential(Uuid::nil()), "keyring locked");
            Ok(self.0.get(&id).cloned())
        }
        fn save(&mut self, id: SecretId, password: &str) -> anyhow::Result<()> {
            anyhow::ensure!(password != "fail", "keyring locked");
            self.0.insert(id, Zeroizing::new(password.to_owned()));
            Ok(())
        }
        fn delete(&mut self, id: SecretId) -> anyhow::Result<()> {
            anyhow::ensure!(id != SecretId::Credential(Uuid::nil()), "keyring locked");
            self.0.remove(&id);
            Ok(())
        }
    }

    #[test]
    fn worker_acknowledges_writes_replacements_errors_and_deletion() {
        let worker = SecretWorker::new(FakeKeyring::default(), || {});
        let id = SecretId::Credential(Uuid::new_v4());
        let receive = || worker.rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let request = worker.submit(SecretOperation::Save(id, Zeroizing::new("first".into())));
        let done = receive();
        assert_eq!(done.request, request);
        assert!(done.result.unwrap().is_none());
        worker.submit(SecretOperation::Save(
            id,
            Zeroizing::new("replacement".into()),
        ));
        assert!(receive().result.is_ok());
        worker.submit(SecretOperation::Save(id, Zeroizing::new("fail".into())));
        assert!(receive().result.is_err());
        worker.submit(SecretOperation::Load(id));
        assert_eq!(&**receive().result.unwrap().unwrap(), "replacement");
        worker.submit(SecretOperation::Delete(id));
        assert!(receive().result.is_ok());
        worker.submit(SecretOperation::Load(id));
        assert!(receive().result.unwrap().is_none());
    }

    #[test]
    fn shared_profile_and_legacy_host_keys_are_distinct() {
        let id = Uuid::new_v4();
        assert_eq!(SecretId::Host(id).key(), id.to_string());
        assert_ne!(SecretId::Host(id).key(), SecretId::Credential(id).key());
    }

    #[test]
    fn worker_reports_lookup_and_delete_failures_and_allows_metadata_only_saves() {
        let worker = SecretWorker::new(FakeKeyring::default(), || {});
        let id = SecretId::Credential(Uuid::nil());
        worker.submit(SecretOperation::Load(id));
        assert!(
            worker
                .rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .result
                .is_err()
        );
        worker.submit(SecretOperation::Delete(id));
        assert!(
            worker
                .rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .result
                .is_err()
        );
        worker.submit(SecretOperation::Keep);
        assert!(
            worker
                .rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .result
                .is_ok()
        );
    }
}
