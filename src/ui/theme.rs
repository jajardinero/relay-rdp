//! Design tokens: palette, type scale, shape. One source of truth for every screen.

use std::sync::Arc;

use egui::epaint::Shadow;
use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke, TextStyle,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Window background and letterboxing around the remote desktop.
    pub canvas: Color32,
    /// Panels, rows on hover, dialogs.
    pub surface: Color32,
    /// Inputs and controls.
    pub raised: Color32,
    pub hairline: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub good: Color32,
    pub fair: Color32,
    pub poor: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub const DARK: Palette = Palette {
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
};

pub const LIGHT: Palette = Palette {
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
};

impl Palette {
    pub fn of(ui: &egui::Ui) -> Self {
        if ui.visuals().dark_mode { DARK } else { LIGHT }
    }

    pub fn of_ctx(ctx: &egui::Context) -> Self {
        if ctx.global_style().visuals.dark_mode {
            DARK
        } else {
            LIGHT
        }
    }

    /// The accent at low strength, for selected rows and focus halos.
    pub fn accent_soft(&self) -> Color32 {
        self.accent
            .gamma_multiply(if self.dark { 0.18 } else { 0.12 })
    }

    /// A surface one step closer to the text color, for hover.
    pub fn hover(&self) -> Color32 {
        if self.dark {
            hex(0x222939)
        } else {
            hex(0xF0F3F9)
        }
    }
}

/// Type scale (points). Ratio ≈ 1.25, body at 14.
pub mod size {
    pub const SMALL: f32 = 12.0;
    pub const BODY: f32 = 14.0;
    pub const SUBHEAD: f32 = 17.0;
    pub const HEADING: f32 = 22.0;
}

/// Corner radii by hierarchy: panels > rows > controls.
pub mod radius {
    pub const PANEL: u8 = 12;
    pub const ROW: u8 = 8;
    pub const CONTROL: u8 = 6;
}

pub fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn font_medium(size: f32) -> FontId {
    FontId::new(size, medium())
}

pub fn font_semibold(size: f32) -> FontId {
    FontId::new(size, semibold())
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let add = |fonts: &mut FontDefinitions, name: &str, bytes: &'static [u8]| {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    };
    add(
        &mut fonts,
        "plex",
        include_bytes!("../../assets/fonts/IBMPlexSans-Regular.ttf"),
    );
    add(
        &mut fonts,
        "plex-medium",
        include_bytes!("../../assets/fonts/IBMPlexSans-Medium.ttf"),
    );
    add(
        &mut fonts,
        "plex-semibold",
        include_bytes!("../../assets/fonts/IBMPlexSans-SemiBold.ttf"),
    );
    add(
        &mut fonts,
        "plex-mono",
        include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf"),
    );

    // egui's bundled fonts stay as fallbacks for symbols and scripts Plex lacks.
    let proportional_fallbacks = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mono_fallbacks = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();

    let family = |primary: &str| {
        let mut list = vec![primary.to_owned()];
        list.extend(proportional_fallbacks.iter().cloned());
        list
    };
    fonts
        .families
        .insert(FontFamily::Proportional, family("plex"));
    fonts.families.insert(medium(), family("plex-medium"));
    fonts.families.insert(semibold(), family("plex-semibold"));
    let mut mono_list = vec!["plex-mono".to_owned()];
    mono_list.extend(mono_fallbacks);
    fonts.families.insert(FontFamily::Monospace, mono_list);

    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    for name in ["medium", "semibold"] {
        if let Some(list) = fonts.families.get_mut(&FontFamily::Name(name.into())) {
            list.push("phosphor".to_owned());
        }
    }

    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context, palette: &Palette, reduce_motion: bool) {
    ctx.set_visuals(visuals(palette));
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, font(size::SMALL)),
            (TextStyle::Body, font(size::BODY)),
            (TextStyle::Button, font_medium(size::BODY)),
            (TextStyle::Heading, font_semibold(size::HEADING)),
            (TextStyle::Monospace, mono(13.0)),
        ]
        .into();

        let spacing = &mut style.spacing;
        spacing.item_spacing = egui::vec2(8.0, 8.0);
        spacing.button_padding = egui::vec2(14.0, 7.0);
        spacing.interact_size.y = 32.0;
        spacing.window_margin = Margin::same(20);
        spacing.menu_margin = Margin::same(6);
        spacing.text_edit_width = 280.0;
        spacing.scroll = egui::style::ScrollStyle::floating();

        style.animation_time = if reduce_motion { 0.0 } else { 0.12 };
        style.interaction.selectable_labels = false;
    });
}

fn visuals(p: &Palette) -> egui::Visuals {
    let mut v = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    v.override_text_color = Some(p.text);
    v.weak_text_color = Some(p.muted);
    v.hyperlink_color = p.accent;
    v.panel_fill = p.canvas;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.hairline);
    v.faint_bg_color = p.surface;
    v.extreme_bg_color = p.raised;
    v.text_edit_bg_color = Some(p.raised);
    v.code_bg_color = p.raised;
    v.warn_fg_color = p.fair;
    v.error_fg_color = p.poor;
    v.window_corner_radius = CornerRadius::same(radius::PANEL);
    v.menu_corner_radius = CornerRadius::same(radius::ROW);
    v.window_shadow = Shadow {
        offset: [0, 10],
        blur: 32,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 110 } else { 38 }),
    };
    v.popup_shadow = Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 90 } else { 28 }),
    };
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.striped = false;
    v.indent_has_left_vline = false;

    let control = CornerRadius::same(radius::CONTROL);
    let w = &mut v.widgets;

    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.hairline);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = control;

    w.inactive.bg_fill = p.raised;
    w.inactive.weak_bg_fill = p.raised;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = control;
    w.inactive.expansion = 0.0;

    let hovered_fill = if p.dark { hex(0x2C3450) } else { hex(0xE4E8F1) };
    w.hovered.bg_fill = hovered_fill;
    w.hovered.weak_bg_fill = hovered_fill;
    w.hovered.bg_stroke = Stroke::new(1.0, p.hairline);
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.corner_radius = control;
    w.hovered.expansion = 0.0;

    let active_fill = if p.dark { hex(0x343D5C) } else { hex(0xD9DFEB) };
    w.active.bg_fill = active_fill;
    w.active.weak_bg_fill = active_fill;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.active.corner_radius = control;
    w.active.expansion = 0.0;

    w.open.bg_fill = p.raised;
    w.open.weak_bg_fill = p.raised;
    w.open.bg_stroke = Stroke::new(1.0, p.hairline);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = control;

    v
}
