//! Importing `.rdp` connection files (as saved by Remote Desktop Connection, RD Web Access or
//! an RDS broker) into a host profile.

use std::path::Path;

use ironrdp_propertyset::PropertySet;

use super::profiles::{
    AudioMode, DEFAULT_PORT, HostProfile, Quality, ResolutionMode, parse_endpoint,
};

pub struct Imported {
    pub profile: HostProfile,
    /// Settings in the file that Relay can't honour, in plain words.
    pub unsupported: Vec<&'static str>,
}

pub fn import_file(path: &Path) -> anyhow::Result<Imported> {
    let bytes = std::fs::read(path)?;
    let text = decode(&bytes);
    let mut imported =
        import(&text).ok_or_else(|| anyhow::anyhow!("the file has no computer address"))?;
    if imported.profile.name == imported.profile.address
        && let Some(stem) = path.file_stem()
    {
        imported.profile.name = stem.to_string_lossy().into_owned();
    }
    Ok(imported)
}

/// Remote Desktop Connection writes UTF-16LE with a byte-order mark; other tools write UTF-8.
fn decode(bytes: &[u8]) -> String {
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn utf16(bytes: &[u8], read: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| read(*c)).collect();
    String::from_utf16_lossy(&units)
}

pub fn import(text: &str) -> Option<Imported> {
    // Key names are case-insensitive in practice; normalise before handing over to the parser.
    let normalized: String = text
        .lines()
        .filter_map(|line| {
            let (key, rest) = line.trim().split_once(':')?;
            Some(format!("{}:{rest}\n", key.trim().to_ascii_lowercase()))
        })
        .collect();
    let props = ironrdp_rdpfile::parse(&normalized).properties;

    let str_of = |key: &str| {
        props
            .get::<&str>(key)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let int_of = |key: &str| props.get::<i64>(key);

    let address = str_of("full address").or_else(|| str_of("alternate full address"))?;
    let (host, mut port) = parse_endpoint(&address)?;
    if let Some(p) = int_of("server port").and_then(|p| u16::try_from(p).ok())
        && port == DEFAULT_PORT
        && p != 0
    {
        port = p;
    }

    let mut profile = HostProfile::new(host, port);
    let mut unsupported = Vec::new();

    if let Some(user) = str_of("username") {
        profile.username = user;
    }
    if let Some(domain) = str_of("domain") {
        profile.domain = domain;
    }

    apply_display(&props, &mut profile);

    if let Some(info) = str_of("loadbalanceinfo") {
        profile.advanced.load_balance_info = info;
    }
    if let Some(clipboard) = int_of("redirectclipboard") {
        profile.devices.clipboard = clipboard != 0;
    }
    if let Some(mode) = int_of("audiomode") {
        // 0: play here, 1: leave on the remote computer, 2: don't play.
        profile.devices.audio = if mode == 0 {
            AudioMode::Local
        } else {
            AudioMode::Off
        };
    }
    if int_of("autoreconnection enabled") == Some(0) {
        profile.advanced.auto_reconnect = false;
    }

    apply_quality(&props, &mut profile);

    if str_of("gatewayhostname").is_some()
        && int_of("gatewayusagemethod").is_some_and(|m| m != 0 && m != 4)
    {
        unsupported.push("RD Gateway");
    }
    if str_of("remoteapplicationprogram").is_some() || int_of("remoteapplicationmode") == Some(1) {
        unsupported.push("RemoteApp");
    }
    if int_of("use multimon") == Some(1) {
        unsupported.push("multiple monitors");
    }
    if int_of("redirectsmartcards") == Some(1) {
        unsupported.push("smart card redirection");
    }

    Some(Imported {
        profile,
        unsupported,
    })
}

/// The file's connection speed picks a quality preset; explicit effect settings then win.
fn apply_quality(props: &PropertySet, profile: &mut HostProfile) {
    let int_of = |key: &str| props.get::<i64>(key);
    let experience = &mut profile.experience;

    let quality = match int_of("connection type") {
        // Modem, low-speed broadband, satellite.
        Some(1..=3) => Some(Quality::LowBandwidth),
        // High-speed broadband, WAN.
        Some(4 | 5) => Some(Quality::Balanced),
        // LAN, auto-detect.
        Some(6 | 7) => Some(Quality::Best),
        _ => None,
    };
    if let Some(quality) = quality {
        experience.quality = quality;
        quality.apply_visuals(experience);
    }

    if let Some(off) = int_of("disable wallpaper") {
        experience.wallpaper = off == 0;
    }
    if let Some(off) = int_of("disable menu anims") {
        experience.animations = off == 0;
    }
    if let Some(on) = int_of("allow font smoothing") {
        experience.font_smoothing = on != 0;
    }
}

fn apply_display(props: &PropertySet, profile: &mut HostProfile) {
    let int_of = |key: &str| props.get::<i64>(key);
    let display = &mut profile.display;

    // Screen mode 2 is fullscreen, 1 is windowed.
    display.start_fullscreen = int_of("screen mode id") == Some(2);

    let size = int_of("desktopwidth")
        .zip(int_of("desktopheight"))
        .and_then(|(w, h)| Some((u16::try_from(w).ok()?, u16::try_from(h).ok()?)))
        .filter(|(w, h)| *w >= 200 && *h >= 200);
    // A fixed size only makes sense when the file turns dynamic resolution off.
    if int_of("dynamic resolution") == Some(0)
        && let Some((width, height)) = size
    {
        display.resolution = ResolutionMode::Fixed;
        display.fixed_width = width;
        display.fixed_height = height;
    }

    if let Some(scale) = int_of("desktopscalefactor").and_then(|s| u32::try_from(s).ok())
        && (100..=500).contains(&scale)
    {
        display.scale_percent = Some(scale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSTSC: &str = "screen mode id:i:2\r\nuse multimon:i:0\r\ndesktopwidth:i:1920\r\ndesktopheight:i:1080\r\n\
        session bpp:i:32\r\nfull address:s:dc01.corp.example.com:3390\r\naudiomode:i:2\r\n\
        redirectclipboard:i:0\r\nusername:s:CORP\\jdoe\r\nloadbalanceinfo:s:tsv://MS Terminal Services Plugin.1.Desk\r\n\
        dynamic resolution:i:0\r\n\r\n";

    #[test]
    fn imports_typical_file() {
        let imported = import(MSTSC).unwrap();
        let p = &imported.profile;
        assert_eq!(
            (p.address.as_str(), p.port),
            ("dc01.corp.example.com", 3390)
        );
        assert_eq!(p.username, "CORP\\jdoe");
        assert!(p.display.start_fullscreen);
        assert_eq!(p.display.resolution, ResolutionMode::Fixed);
        assert_eq!(
            (p.display.fixed_width, p.display.fixed_height),
            (1920, 1080)
        );
        assert_eq!(p.devices.audio, AudioMode::Off);
        assert!(!p.devices.clipboard);
        assert_eq!(
            p.advanced.load_balance_info,
            "tsv://MS Terminal Services Plugin.1.Desk"
        );
        assert!(imported.unsupported.is_empty());
    }

    #[test]
    fn utf16_files_with_bom_decode() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(
            "full address:s:10.0.0.5\r\n"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        let imported = import(&decode(&bytes)).unwrap();
        assert_eq!(
            (imported.profile.address.as_str(), imported.profile.port),
            ("10.0.0.5", 3389)
        );
        // Dynamic resolution is the default when the file doesn't turn it off.
        assert_eq!(
            imported.profile.display.resolution,
            ResolutionMode::MatchWindow
        );
    }

    #[test]
    fn server_port_and_unsupported_features() {
        let text = "Full Address:s:host\nserver port:i:4000\ngatewayhostname:s:gw.example.com\n\
                    gatewayusagemethod:i:1\nremoteapplicationmode:i:1\n";
        let imported = import(text).unwrap();
        assert_eq!(imported.profile.port, 4000);
        assert_eq!(imported.unsupported, vec!["RD Gateway", "RemoteApp"]);
    }

    #[test]
    fn imports_from_disk_and_names_after_the_file() {
        let dir = std::env::temp_dir().join(format!("relay-rdpfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Lab server.rdp");
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(
            "full address:s:127.0.0.1:3390\r\nusername:s:test\r\naudiomode:i:2\r\n"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        std::fs::write(&path, bytes).unwrap();

        let imported = import_file(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(imported.profile.name, "Lab server");
        assert_eq!(imported.profile.endpoint(), "127.0.0.1:3390");
        assert_eq!(imported.profile.username, "test");
        assert_eq!(imported.profile.devices.audio, AudioMode::Off);
    }

    #[test]
    fn connection_speed_picks_quality_and_explicit_effects_win() {
        let low = import("full address:s:h\nconnection type:i:2\n").unwrap();
        let e = &low.profile.experience;
        assert_eq!(e.quality, Quality::LowBandwidth);
        assert!(!e.wallpaper && !e.animations && !e.font_smoothing);

        let wan = import("full address:s:h\nconnection type:i:5\nallow font smoothing:i:0\ndisable wallpaper:i:0\n")
            .unwrap();
        let e = &wan.profile.experience;
        assert_eq!(e.quality, Quality::Balanced);
        assert!(e.wallpaper && e.animations && !e.font_smoothing);

        // No connection type: the default quality and effects are kept.
        let plain = import("full address:s:h\n").unwrap();
        assert_eq!(
            plain.profile.experience,
            HostProfile::new("h", 3389).experience
        );
    }

    #[test]
    fn files_without_an_address_are_rejected() {
        assert!(import("username:s:jdoe\n").is_none());
    }
}
