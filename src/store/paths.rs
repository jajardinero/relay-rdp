use std::path::PathBuf;

use directories::ProjectDirs;

use crate::APP_ID;

/// Set `RELAY_HOME` to keep all config, data and logs under one directory (for development and
/// tests) instead of the XDG locations.
fn override_dir() -> Option<PathBuf> {
    std::env::var_os("RELAY_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", APP_ID)
}

/// `~/.config/relay`
pub fn config_dir() -> Option<PathBuf> {
    if let Some(home) = override_dir() {
        return Some(home.join("config"));
    }
    project_dirs().map(|d| d.config_dir().to_path_buf())
}

/// `~/.local/share/relay`
pub fn data_dir() -> Option<PathBuf> {
    if let Some(home) = override_dir() {
        return Some(home.join("data"));
    }
    project_dirs().map(|d| d.data_dir().to_path_buf())
}

/// `~/.local/share/relay/previews`: snapshots of hosts' screens for the host library.
pub fn previews_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("previews"))
}

/// `~/.local/state/relay/logs`
pub fn log_dir() -> Option<PathBuf> {
    if let Some(home) = override_dir() {
        return Some(home.join("logs"));
    }
    let dirs = project_dirs()?;
    let base = dirs.state_dir().unwrap_or(dirs.data_local_dir());
    Some(base.join("logs"))
}
