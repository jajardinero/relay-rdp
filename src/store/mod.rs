pub mod credentials;
pub mod known_hosts;
pub mod paths;
pub mod profiles;
pub mod rdpfile;
pub mod secrets;
pub mod settings;

use std::io::Write as _;
use std::path::Path;

/// Writes `contents` to `path` atomically: a crash mid-write never leaves a truncated file.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;

    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Reads a TOML file. A missing file yields the default value; an unreadable or corrupt file
/// is moved aside (so the user's data is never silently overwritten) and also yields the default.
pub fn load_toml<T>(path: &Path) -> T
where
    T: serde::de::DeserializeOwned + Default,
{
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return T::default(),
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e, "failed to read file");
            return T::default();
        }
    };

    match toml::from_str(&text) {
        Ok(value) => value,
        Err(e) => {
            let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
            let backup = path.with_extension(format!("toml.corrupt-{stamp}"));
            tracing::error!(path = %path.display(), backup = %backup.display(), error = %e, "corrupt file moved aside");
            let _ = std::fs::rename(path, &backup);
            T::default()
        }
    }
}

pub fn save_toml<T: serde::Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let text = toml::to_string_pretty(value)?;
    write_atomic(path, text.as_bytes())?;
    Ok(())
}
