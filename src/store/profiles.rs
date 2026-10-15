use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DEFAULT_PORT: u16 = 3389;

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn yes() -> bool {
    true
}

/// A saved connection. Passwords never live here; they are kept in the system keyring.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostProfile {
    pub id: Uuid,
    pub name: String,
    pub address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub save_password: bool,
    #[serde(default)]
    pub last_connected: Option<DateTime<Utc>>,
    #[serde(default)]
    pub display: DisplaySettings,
    #[serde(default)]
    pub experience: ExperienceSettings,
    #[serde(default)]
    pub devices: DeviceSettings,
    #[serde(default)]
    pub advanced: AdvancedSettings,
}

impl HostProfile {
    pub fn new(address: impl Into<String>, port: u16) -> Self {
        let address = address.into();
        Self {
            id: Uuid::new_v4(),
            name: address.clone(),
            address,
            port,
            username: String::new(),
            domain: String::new(),
            group: String::new(),
            favorite: false,
            save_password: false,
            last_connected: None,
            display: DisplaySettings::default(),
            experience: ExperienceSettings::default(),
            devices: DeviceSettings::default(),
            advanced: AdvancedSettings::default(),
        }
    }

    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.address
        } else {
            &self.name
        }
    }

    /// `host` or `host:port`, the way people write addresses.
    pub fn endpoint(&self) -> String {
        if self.port == DEFAULT_PORT {
            self.address.clone()
        } else {
            format!("{}:{}", self.address, self.port)
        }
    }

    /// `DOMAIN\user`, `user@domain` or `user`, for display.
    pub fn account_label(&self) -> String {
        match (self.username.is_empty(), self.domain.is_empty()) {
            (true, _) => String::new(),
            (false, true) => self.username.clone(),
            (false, false) => format!("{}\\{}", self.domain, self.username),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ResolutionMode {
    /// The remote desktop follows the window size (Display Control).
    #[default]
    MatchWindow,
    /// A fixed remote resolution, scaled to fit the window.
    Fixed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplaySettings {
    #[serde(default)]
    pub resolution: ResolutionMode,
    #[serde(default = "default_fixed_width")]
    pub fixed_width: u16,
    #[serde(default = "default_fixed_height")]
    pub fixed_height: u16,
    /// Remote scale in percent; `None` follows the local display scale.
    #[serde(default)]
    pub scale_percent: Option<u32>,
    #[serde(default)]
    pub start_fullscreen: bool,
}

fn default_fixed_width() -> u16 {
    1920
}

fn default_fixed_height() -> u16 {
    1080
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            resolution: ResolutionMode::default(),
            fixed_width: default_fixed_width(),
            fixed_height: default_fixed_height(),
            scale_percent: None,
            start_fullscreen: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GraphicsMode {
    /// H.264 for motion with lossless text codecs (graphics pipeline, AVC420).
    #[default]
    Auto,
    /// Graphics pipeline without H.264: crisper text, more bandwidth.
    SharpText,
    /// Legacy RemoteFX/bitmap updates; works with every server.
    Compatible,
    /// H.264 4:4:4 (AVC444) when the server offers it, otherwise as Auto.
    FullColor,
}

impl GraphicsMode {
    pub const ALL: [Self; 4] = [
        Self::Auto,
        Self::FullColor,
        Self::SharpText,
        Self::Compatible,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Balanced (H.264)",
            Self::FullColor => "Full color (H.264 4:4:4)",
            Self::SharpText => "Sharp text",
            Self::Compatible => "Compatible",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Auto => "Smooth video and fast scrolling with low bandwidth.",
            Self::FullColor => {
                "Full color detail in video and text. Needs the server's AVC 444 policy; uses more bandwidth."
            }
            Self::SharpText => "Lossless text and UI. Uses more bandwidth for video.",
            Self::Compatible => "Classic RemoteFX updates for older servers.",
        }
    }

    /// Whether this mode decodes H.264.
    pub fn uses_h264(self) -> bool {
        matches!(self, Self::Auto | Self::FullColor)
    }
}

/// How good a link the server should assume. The server tunes video quality, frame rate and
/// effects to it; picking one in the editor also sets the visual-effect toggles.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Quality {
    /// LAN or office network.
    #[default]
    Best,
    /// Home broadband or VPN.
    Balanced,
    /// Mobile hotspots and slow links.
    LowBandwidth,
}

impl Quality {
    pub const ALL: [Self; 3] = [Self::Best, Self::Balanced, Self::LowBandwidth];

    pub fn label(self) -> &'static str {
        match self {
            Self::Best => "Best",
            Self::Balanced => "Balanced",
            Self::LowBandwidth => "Low bandwidth",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Best => "Office network or LAN. Full visual effects.",
            Self::Balanced => "Home broadband or VPN. Skips effects that cost bandwidth.",
            Self::LowBandwidth => "Mobile or slow links. Fewer effects and lighter video.",
        }
    }

    /// Sets the visual-effect toggles to suit this preset.
    pub fn apply_visuals(self, experience: &mut ExperienceSettings) {
        let (wallpaper, animations, font_smoothing) = match self {
            Self::Best => (true, true, true),
            Self::Balanced => (false, true, true),
            Self::LowBandwidth => (false, false, false),
        };
        experience.wallpaper = wallpaper;
        experience.animations = animations;
        experience.font_smoothing = font_smoothing;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExperienceSettings {
    #[serde(default)]
    pub quality: Quality,
    #[serde(default)]
    pub graphics: GraphicsMode,
    #[serde(default)]
    pub wallpaper: bool,
    #[serde(default = "yes")]
    pub animations: bool,
    #[serde(default = "yes")]
    pub font_smoothing: bool,
}

impl Default for ExperienceSettings {
    fn default() -> Self {
        Self {
            quality: Quality::default(),
            graphics: GraphicsMode::default(),
            wallpaper: false,
            animations: true,
            font_smoothing: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AudioMode {
    /// Play remote audio on this computer.
    #[default]
    Local,
    /// No audio.
    #[serde(alias = "remote")]
    Off,
}

impl AudioMode {
    pub const ALL: [Self; 2] = [Self::Local, Self::Off];

    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "Play on this computer",
            Self::Off => "Don't play",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SharedFolder {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceSettings {
    #[serde(default = "yes")]
    pub clipboard: bool,
    #[serde(default)]
    pub audio: AudioMode,
    #[serde(default)]
    pub shared_folders: Vec<SharedFolder>,
}

impl Default for DeviceSettings {
    fn default() -> Self {
        Self {
            clipboard: true,
            audio: AudioMode::default(),
            shared_folders: Vec::new(),
        }
    }
}

/// US English. See <https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/windows-language-pack-default-values>.
pub const DEFAULT_KEYBOARD_LAYOUT: u32 = 0x0409;

fn default_keyboard_layout() -> u32 {
    DEFAULT_KEYBOARD_LAYOUT
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdvancedSettings {
    #[serde(default = "default_keyboard_layout")]
    pub keyboard_layout: u32,
    /// RDS connection broker routing token (`loadbalanceinfo` in .rdp files).
    #[serde(default)]
    pub load_balance_info: String,
    #[serde(default = "yes")]
    pub auto_reconnect: bool,
}

impl Default for AdvancedSettings {
    fn default() -> Self {
        Self {
            keyboard_layout: DEFAULT_KEYBOARD_LAYOUT,
            load_balance_info: String::new(),
            auto_reconnect: true,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HostsFile {
    #[serde(default, rename = "host")]
    hosts: Vec<HostProfile>,
}

/// The saved host library, persisted to `~/.config/relay/hosts.toml`.
pub struct ProfileStore {
    path: Option<PathBuf>,
    hosts: Vec<HostProfile>,
}

impl ProfileStore {
    pub fn load() -> Self {
        let path = super::paths::config_dir().map(|d| d.join("hosts.toml"));
        let hosts = path
            .as_deref()
            .map(|p| super::load_toml::<HostsFile>(p).hosts)
            .unwrap_or_default();
        Self { path, hosts }
    }

    pub fn hosts(&self) -> &[HostProfile] {
        &self.hosts
    }

    pub fn get(&self, id: Uuid) -> Option<&HostProfile> {
        self.hosts.iter().find(|h| h.id == id)
    }

    /// Inserts or replaces the profile with the same id.
    pub fn upsert(&mut self, profile: HostProfile) {
        match self.hosts.iter_mut().find(|h| h.id == profile.id) {
            Some(existing) => *existing = profile,
            None => self.hosts.push(profile),
        }
        self.save();
    }

    pub fn remove(&mut self, id: Uuid) -> Option<HostProfile> {
        let index = self.hosts.iter().position(|h| h.id == id)?;
        let removed = self.hosts.remove(index);
        self.save();
        Some(removed)
    }

    pub fn mark_connected(&mut self, id: Uuid) {
        if let Some(host) = self.hosts.iter_mut().find(|h| h.id == id) {
            host.last_connected = Some(Utc::now());
            self.save();
        }
    }

    pub fn toggle_favorite(&mut self, id: Uuid) {
        if let Some(host) = self.hosts.iter_mut().find(|h| h.id == id) {
            host.favorite = !host.favorite;
            self.save();
        }
    }

    /// Group names in first-seen order, excluding the empty (ungrouped) group.
    pub fn groups(&self) -> Vec<String> {
        let mut groups: Vec<String> = Vec::new();
        for host in &self.hosts {
            let group = host.group.trim();
            if !group.is_empty() && !groups.iter().any(|g| g == group) {
                groups.push(group.to_owned());
            }
        }
        groups
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let file = HostsFile {
            hosts: self.hosts.clone(),
        };
        if let Err(e) = super::save_toml(path, &file) {
            tracing::error!(error = %e, "failed to save hosts");
        }
    }
}

/// Parses `host`, `host:port`, `[v6]:port` or a bare IPv6 address.
pub fn parse_endpoint(input: &str) -> Option<(String, u16)> {
    let input = input.trim();
    if input.is_empty() || input.chars().any(char::is_whitespace) {
        return None;
    }

    if let Some(rest) = input.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => DEFAULT_PORT,
            None => return None,
        };
        return Some((host.to_owned(), port));
    }

    match input.matches(':').count() {
        0 => Some((input.to_owned(), DEFAULT_PORT)),
        1 => {
            let (host, port) = input.split_once(':')?;
            if host.is_empty() {
                return None;
            }
            Some((host.to_owned(), port.parse().ok()?))
        }
        // Bare IPv6 address without a port.
        _ => input
            .parse::<std::net::Ipv6Addr>()
            .ok()
            .map(|_| (input.to_owned(), DEFAULT_PORT)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        assert_eq!(parse_endpoint("dc01"), Some(("dc01".into(), 3389)));
        assert_eq!(
            parse_endpoint(" 10.0.0.5:3390 "),
            Some(("10.0.0.5".into(), 3390))
        );
        assert_eq!(
            parse_endpoint("[fe80::1]:4000"),
            Some(("fe80::1".into(), 4000))
        );
        assert_eq!(parse_endpoint("fe80::1"), Some(("fe80::1".into(), 3389)));
        assert_eq!(parse_endpoint("host:notaport"), None);
        assert_eq!(parse_endpoint("two words"), None);
        assert_eq!(parse_endpoint(":3389"), None);
        assert_eq!(parse_endpoint(""), None);
    }

    #[test]
    fn profile_round_trip() {
        let mut profile = HostProfile::new("build-01.corp.local", 3390);
        profile.username = "jdoe".into();
        profile.domain = "CORP".into();
        profile.experience.quality = Quality::LowBandwidth;
        profile.devices.shared_folders.push(SharedFolder {
            name: "Downloads".into(),
            path: "/home/jdoe/Downloads".into(),
        });
        let file = HostsFile {
            hosts: vec![profile.clone()],
        };
        let text = toml::to_string_pretty(&file).unwrap();
        let parsed: HostsFile = toml::from_str(&text).unwrap();
        assert_eq!(parsed.hosts, vec![profile]);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let text = format!(
            "[[host]]\nid = \"{}\"\nname = \"a\"\naddress = \"a\"\n",
            Uuid::new_v4()
        );
        let parsed: HostsFile = toml::from_str(&text).unwrap();
        let host = &parsed.hosts[0];
        assert_eq!(host.port, DEFAULT_PORT);
        assert!(host.devices.clipboard);
        assert!(host.advanced.auto_reconnect);
        assert_eq!(host.advanced.keyboard_layout, DEFAULT_KEYBOARD_LAYOUT);
        assert_eq!(host.experience.quality, Quality::Best);
    }

    #[test]
    fn quality_presets_set_visual_effects() {
        let mut experience = ExperienceSettings::default();
        Quality::LowBandwidth.apply_visuals(&mut experience);
        assert!(!experience.wallpaper && !experience.animations && !experience.font_smoothing);
        Quality::Balanced.apply_visuals(&mut experience);
        assert!(!experience.wallpaper && experience.animations && experience.font_smoothing);
        Quality::Best.apply_visuals(&mut experience);
        assert!(experience.wallpaper && experience.animations && experience.font_smoothing);
        // The preset only touches visuals.
        assert_eq!(experience.graphics, GraphicsMode::default());
    }

    #[test]
    fn quality_serializes_in_kebab_case() {
        let text = toml::to_string(&ExperienceSettings {
            quality: Quality::LowBandwidth,
            ..Default::default()
        })
        .unwrap();
        assert!(text.contains("quality = \"low-bandwidth\""), "{text}");
    }
}
