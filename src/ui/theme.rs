//! Design tokens: palette, type scale, shape, and GPUI Kit Theme synchronization.

use std::borrow::Cow;

use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::{App, BoxShadow, Hsla, point, px, rgb, rgba};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Window background and letterboxing around the remote desktop.
    pub canvas: Hsla,
    /// Panels, rows on hover, dialogs.
    pub surface: Hsla,
    /// Inputs and controls.
    pub raised: Hsla,
    pub hairline: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub on_accent: Hsla,
    pub good: Hsla,
    pub fair: Hsla,
    pub poor: Hsla,
    pub hover: Hsla,
    pub control_hover: Hsla,
    pub control_active: Hsla,
}

fn hex(code: u32) -> Hsla {
    rgb(code).into()
}

pub fn dark() -> Palette {
    Palette {
        dark: true,
        canvas: hex(0x151A26),
        surface: hex(0x1C2232),
        raised: hex(0x252C40),
        hairline: hex(0x2F3750),
        text: hex(0xE6E9F2),
        muted: hex(0x8B93AB),
        accent: hex(0x6EA8FE),
        on_accent: hex(0x0E1420),
        good: hex(0x3DD68C),
        fair: hex(0xF5B83D),
        poor: hex(0xF2676A),
        hover: hex(0x222939),
        control_hover: hex(0x2C3450),
        control_active: hex(0x343D5C),
    }
}

pub fn light() -> Palette {
    Palette {
        dark: false,
        canvas: hex(0xF5F7FB),
        surface: hex(0xFFFFFF),
        raised: hex(0xEEF1F7),
        hairline: hex(0xDCE1EC),
        text: hex(0x161B29),
        muted: hex(0x5E6780),
        accent: hex(0x2F6FEB),
        on_accent: hex(0xFFFFFF),
        good: hex(0x1F9D61),
        fair: hex(0xB7791F),
        poor: hex(0xD64545),
        hover: hex(0xF0F3F9),
        control_hover: hex(0xE4E8F1),
        control_active: hex(0xD9DFEB),
    }
}

impl Palette {
    pub fn for_mode(is_dark: bool) -> Self {
        if is_dark { dark() } else { light() }
    }

    /// The accent at low strength, for selected rows and focus halos.
    pub fn accent_soft(&self) -> Hsla {
        self.accent.opacity(if self.dark { 0.18 } else { 0.12 })
    }

    pub fn window_shadow(&self) -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: rgba(if self.dark { 0x0000006E } else { 0x00000026 }).into(),
            offset: point(px(0.), px(10.)),
            blur_radius: px(32.),
            spread_radius: px(0.),
            inset: false,
        }]
    }

    pub fn popup_shadow(&self) -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: rgba(if self.dark { 0x0000005A } else { 0x0000001C }).into(),
            offset: point(px(0.), px(6.)),
            blur_radius: px(18.),
            spread_radius: px(0.),
            inset: false,
        }]
    }

    pub fn backdrop(&self) -> Hsla {
        rgba(if self.dark { 0x00000096 } else { 0x0000005A }).into()
    }
}

/// Type scale (px). Ratio ~ 1.25, body at 14.
pub mod size {
    use gpui_kit::{Pixels, px};

    pub const SMALL: Pixels = px(12.0);
    pub const BODY: Pixels = px(14.0);
    pub const SUBHEAD: Pixels = px(17.0);
    pub const HEADING: Pixels = px(22.0);
    pub const MONO: Pixels = px(13.0);
}

/// Corner radii by hierarchy: panels > rows > controls.
pub mod radius {
    use gpui_kit::{Pixels, px};

    pub const PANEL: Pixels = px(12.0);
    pub const ROW: Pixels = px(8.0);
    pub const CONTROL: Pixels = px(6.0);
}

pub const FONT_SANS: &str = "IBM Plex Sans";
pub const FONT_MONO: &str = "IBM Plex Mono";

/// Loads Relay's bundled IBM Plex Sans & Mono fonts into GPUI's text system.
pub fn install_fonts(cx: &App) {
    let fonts = vec![
        Cow::Borrowed(include_bytes!("../../assets/fonts/IBMPlexSans-Regular.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../../assets/fonts/IBMPlexSans-Medium.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../../assets/fonts/IBMPlexSans-SemiBold.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf").as_slice()),
    ];
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        tracing::warn!(%error, "couldn't load bundled IBM Plex fonts");
    }
}

/// Synchronizes GPUI Kit's global `Theme` with Relay's exact design palette and fonts.
pub fn apply(cx: &mut App, p: &Palette) {
    let mode = if p.dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    Theme::change(mode, None, cx);
    Theme::update(cx, |theme| {
        theme.font_family = FONT_SANS.into();
        theme.mono_font_family = FONT_MONO.into();
        theme.font_size = size::BODY;
        theme.mono_font_size = size::MONO;
        theme.radius = radius::CONTROL;
        theme.radius_lg = radius::PANEL;

        let c = &mut theme.colors;
        c.background = p.canvas;
        c.foreground = p.text;
        c.muted = p.raised;
        c.muted_foreground = p.muted;
        c.border = p.hairline;
        c.input = p.hairline;
        c.ring = p.accent;
        c.caret = p.accent;
        c.selection = p.accent.opacity(0.35);

        c.primary = p.accent;
        c.primary_hover = p.accent.opacity(0.9);
        c.primary_active = p.accent.opacity(0.8);
        c.primary_foreground = p.on_accent;

        c.secondary = p.raised;
        c.secondary_hover = p.control_hover;
        c.secondary_active = p.control_active;
        c.secondary_foreground = p.text;

        c.accent = p.hover;
        c.accent_foreground = p.text;

        c.popover = p.surface;
        c.popover_foreground = p.text;

        c.danger = p.poor;
        c.danger_foreground = p.text;
        c.success = p.good;
        c.warning = p.fair;

        c.switch = p.hairline;
        c.switch_thumb = if p.dark { p.text } else { p.surface };

        c.sidebar = p.canvas;
        c.sidebar_foreground = p.text;
        c.sidebar_border = p.hairline;
        c.sidebar_accent = p.accent_soft();
        c.sidebar_accent_foreground = p.text;

        c.list = p.surface;
        c.list_hover = p.hover;
        c.list_active = p.accent_soft();
        c.list_active_border = p.accent;

        c.scrollbar = p.canvas.opacity(0.0);
        c.scrollbar_thumb = p.hairline;
        c.scrollbar_thumb_hover = p.muted;
    });
}
