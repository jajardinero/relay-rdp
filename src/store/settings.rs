use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThemePreference {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ShortcutCapture {
    /// System shortcuts (Super, Alt+Tab) go to the remote computer in fullscreen only.
    #[default]
    Fullscreen,
    /// Whenever a session has keyboard focus.
    Always,
    Never,
}

/// How the host library shows hosts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum HostView {
    #[default]
    List,
    Tiles,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppSettings {
    #[serde(default)]
    pub theme: ThemePreference,
    #[serde(default)]
    pub reduce_motion: bool,
    #[serde(default)]
    pub capture_shortcuts: ShortcutCapture,
    /// Present frames with one frame of queueing instead of two (lower input latency).
    #[serde(default = "default_true")]
    pub low_latency: bool,
    /// Prefer VA-API decoding and GPU composition for new H.264 sessions.
    #[serde(default = "default_true")]
    pub hardware_video_decoding: bool,
    /// Multiplies the distance touchpad scrolling covers on the remote computer.
    #[serde(default = "default_scroll_speed")]
    pub touchpad_scroll_speed: f32,
    /// Keep touchpad scrolling going briefly after the fingers lift, as local apps do.
    #[serde(default = "default_true")]
    pub touchpad_momentum: bool,
    #[serde(default)]
    pub host_view: HostView,
}

fn default_true() -> bool {
    true
}

fn default_scroll_speed() -> f32 {
    1.0
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: ThemePreference::default(),
            reduce_motion: false,
            capture_shortcuts: ShortcutCapture::default(),
            low_latency: true,
            hardware_video_decoding: true,
            touchpad_scroll_speed: default_scroll_speed(),
            touchpad_momentum: true,
            host_view: HostView::default(),
        }
    }
}

pub struct SettingsStore {
    path: Option<PathBuf>,
    pub settings: AppSettings,
}

impl SettingsStore {
    pub fn load() -> Self {
        let path = super::paths::config_dir().map(|d| d.join("settings.toml"));
        let settings = path.as_deref().map(super::load_toml).unwrap_or_default();
        Self { path, settings }
    }

    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Err(e) = super::save_toml(path, &self.settings) {
            tracing::error!(error = %e, "failed to save settings");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_settings_enable_hardware_by_default() {
        let old: AppSettings = toml::from_str("theme = 'dark'\nlow_latency = false\n").unwrap();
        assert!(old.hardware_video_decoding);
        assert!(old.touchpad_momentum);
        assert_eq!(old.touchpad_scroll_speed, 1.0);
        assert_eq!(old.host_view, HostView::List);
        assert!(!old.low_latency);
        assert_eq!(old.theme, ThemePreference::Dark);
        assert!(AppSettings::default().hardware_video_decoding);
    }

    #[test]
    fn disabled_preference_survives_save_and_reload() {
        let settings = AppSettings {
            hardware_video_decoding: false,
            ..Default::default()
        };
        let text = toml::to_string(&settings).unwrap();
        let restored: AppSettings = toml::from_str(&text).unwrap();
        assert_eq!(restored, settings);
        assert!(!restored.hardware_video_decoding);
    }
}
