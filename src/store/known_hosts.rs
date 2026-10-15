//! Trust-on-first-use pinning of server TLS certificates.
//!
//! Most RDP servers use self-signed certificates, so chain validation is meaningless. Instead we
//! remember the SHA-256 fingerprint the first time the user approves a server and warn loudly if
//! it ever changes.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnownHost {
    pub fingerprint: String,
    #[serde(default)]
    pub subject: String,
    pub trusted_at: DateTime<Utc>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct KnownHostsFile {
    #[serde(default)]
    hosts: BTreeMap<String, KnownHost>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    Trusted,
    Unknown,
    Changed { previous: String },
}

pub struct KnownHosts {
    path: Option<PathBuf>,
    file: KnownHostsFile,
}

impl KnownHosts {
    pub fn load() -> Self {
        let path = super::paths::data_dir().map(|d| d.join("known_hosts.toml"));
        let file = path.as_deref().map(super::load_toml).unwrap_or_default();
        Self { path, file }
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> Self {
        Self {
            path: None,
            file: KnownHostsFile::default(),
        }
    }

    pub fn check(&self, host: &str, port: u16, fingerprint: &str) -> Trust {
        match self.file.hosts.get(&key(host, port)) {
            None => Trust::Unknown,
            Some(known) if known.fingerprint.eq_ignore_ascii_case(fingerprint) => Trust::Trusted,
            Some(known) => Trust::Changed {
                previous: known.fingerprint.clone(),
            },
        }
    }

    pub fn trust(&mut self, host: &str, port: u16, fingerprint: &str, subject: &str) {
        self.file.hosts.insert(
            key(host, port),
            KnownHost {
                fingerprint: fingerprint.to_owned(),
                subject: subject.to_owned(),
                trusted_at: Utc::now(),
            },
        );
        if let Some(path) = &self.path
            && let Err(e) = super::save_toml(path, &self.file)
        {
            tracing::error!(error = %e, "failed to save known hosts");
        }
    }
}

fn key(host: &str, port: u16) -> String {
    format!("{}:{port}", host.to_ascii_lowercase())
}

/// SHA-256 of the DER certificate, as colon-separated uppercase hex (the format Windows shows).
pub fn fingerprint(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    let mut out = String::with_capacity(digest.len() * 3);
    for (i, byte) in digest.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_on_first_use() {
        let mut hosts = KnownHosts::in_memory();
        let fp = fingerprint(b"certificate");
        assert_eq!(hosts.check("DC01", 3389, &fp), Trust::Unknown);

        hosts.trust("DC01", 3389, &fp, "CN=dc01");
        assert_eq!(hosts.check("dc01", 3389, &fp), Trust::Trusted);
        assert_eq!(hosts.check("dc01", 3390, &fp), Trust::Unknown);

        let other = fingerprint(b"another certificate");
        assert_eq!(
            hosts.check("dc01", 3389, &other),
            Trust::Changed { previous: fp }
        );
    }

    #[test]
    fn fingerprint_format() {
        let fp = fingerprint(b"");
        assert_eq!(fp.len(), 32 * 3 - 1);
        assert!(fp.starts_with("E3:B0:C4:42"));
    }
}
