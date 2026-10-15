//! Passwords live in the desktop keyring (Secret Service: gnome-keyring, KWallet, …).
//!
//! Every call here talks to D-Bus and may block (e.g. while the keyring asks to be unlocked),
//! so callers must stay off the UI thread.

use uuid::Uuid;
use zeroize::Zeroizing;

const SERVICE: &str = "relay-rdp";

fn entry(profile: Uuid) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &profile.to_string())
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

/// Fire-and-forget save on a worker thread, so the UI never waits on the keyring.
pub fn save_password_in_background(profile: Uuid, password: Zeroizing<String>) {
    std::thread::spawn(move || {
        if let Err(e) = save_password(profile, &password) {
            tracing::error!(error = %e, "failed to save password to the keyring");
        }
    });
}

pub fn delete_password_in_background(profile: Uuid) {
    std::thread::spawn(move || {
        if let Err(e) = delete_password(profile) {
            tracing::warn!(error = %e, "failed to delete password from the keyring");
        }
    });
}
