//! SVG icon asset source: combines GPUI Kit's Lucide catalog with custom OS brand icons.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Hsla, Pixels, Result, SharedString, Styled, Svg, svg};

const WINDOWS_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor"><path d="M3 5.55 10.35 4.5v7.1H3V5.55zm8.25-1.18L21 3v8.6h-9.75V4.37zM3 12.4h7.35v7.1L3 18.45V12.4zm8.25 0H21V21l-9.75-1.37V12.4z"/></svg>"#;

const LINUX_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3c-2.2 0-3.5 1.8-3.5 4.2 0 1.4.2 2.4-.5 3.6-1 1.7-2.2 3.3-2.2 5.4 0 2.5 1.8 4.3 6.2 4.3s6.2-1.8 6.2-4.3c0-2.1-1.2-3.7-2.2-5.4-.7-1.2-.5-2.2-.5-3.6C15.5 4.8 14.2 3 12 3z"/><circle cx="10.5" cy="7.5" r=".7" fill="currentColor"/><circle cx="13.5" cy="7.5" r=".7" fill="currentColor"/><path d="M10.2 9.8c.6.5 1.2.8 1.8.8s1.2-.3 1.8-.8"/><path d="M8.5 20.5c-1.2.4-2.5.4-3.2-.2M15.5 20.5c1.2.4 2.5.4 3.2-.2"/></svg>"#;

pub struct RelayAssets;

impl AssetSource for RelayAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match path {
            "icons/relay-windows.svg" => Ok(Some(Cow::Borrowed(WINDOWS_SVG))),
            "icons/relay-linux.svg" => Ok(Some(Cow::Borrowed(LINUX_SVG))),
            _ => gpui_kit::assets::AllAssets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut items = gpui_kit::assets::AllAssets.list(path)?;
        for extra in ["icons/relay-windows.svg", "icons/relay-linux.svg"] {
            if extra.starts_with(path) {
                items.push(extra.into());
            }
        }
        Ok(items)
    }
}

/// Icon paths used throughout Relay.
pub mod icons {
    pub const SQUARES_FOUR: &str = "icons/layout-grid.svg";
    pub const LIST: &str = "icons/list.svg";
    pub const STAR: &str = "icons/star.svg";
    pub const FOLDER: &str = "icons/folder.svg";
    pub const FOLDER_PLUS: &str = "icons/folder-plus.svg";
    pub const SETTINGS: &str = "icons/settings.svg";
    pub const FILE_DOWN: &str = "icons/file-down.svg";
    pub const KEY: &str = "icons/key.svg";
    pub const PLUS: &str = "icons/plus.svg";
    pub const SEARCH: &str = "icons/search.svg";
    pub const ARROW_RIGHT: &str = "icons/arrow-right.svg";
    pub const PENCIL: &str = "icons/pencil.svg";
    pub const DOTS_THREE: &str = "icons/ellipsis.svg";
    pub const COPY: &str = "icons/copy.svg";
    pub const TRASH: &str = "icons/trash.svg";
    pub const WINDOWS: &str = "icons/relay-windows.svg";
    pub const LINUX: &str = "icons/relay-linux.svg";
    pub const APPLE: &str = "icons/apple.svg";
    pub const DESKTOP: &str = "icons/monitor.svg";
    pub const WARNING_CIRCLE: &str = "icons/circle-alert.svg";
    pub const CHECK_CIRCLE: &str = "icons/circle-check.svg";
    pub const CHECK: &str = "icons/check.svg";
    pub const X: &str = "icons/x.svg";
    pub const SHIELD_WARNING: &str = "icons/shield-alert.svg";
    pub const SHIELD_CHECK: &str = "icons/shield-check.svg";
    pub const PLUGS: &str = "icons/unplug.svg";
    pub const STACK: &str = "icons/layers.svg";
    pub const KEYBOARD: &str = "icons/keyboard.svg";
    pub const MINUS: &str = "icons/minus.svg";
    pub const CORNERS_IN: &str = "icons/minimize-2.svg";
    pub const CORNERS_OUT: &str = "icons/maximize-2.svg";
    pub const CARET_DOWN: &str = "icons/chevron-down.svg";
    pub const RADIO_ON: &str = "icons/circle-dot.svg";
    pub const RADIO_OFF: &str = "icons/circle.svg";
    pub const SERVER: &str = "icons/server.svg";
    pub const KEY_ROUND: &str = "icons/key-round.svg";
    pub const TERMINAL: &str = "icons/square-terminal.svg";
    pub const PANEL_RIGHT: &str = "icons/panel-right.svg";
}

/// Renders an SVG icon at `size` in `color`.
pub fn icon_svg(path: &'static str, size: Pixels, color: Hsla) -> Svg {
    svg().path(path).flex_shrink_0().size(size).text_color(color)
}
