//! The host editor: a drawer on the right of the host library.

use egui::{Align, Color32, CornerRadius, Layout, RichText, Stroke, Ui, vec2};
use egui_phosphor::regular as icon;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::theme::{self, Palette, radius, size};
use super::widgets;
use crate::store::profiles::{
    AudioMode, GraphicsMode, HostProfile, OsChoice, ProfileStore, Quality, ResolutionMode,
    SharedFolder,
};

pub enum PasswordChange {
    Keep,
    Set(Zeroizing<String>),
    Forget,
}

pub enum Action {
    Cancel,
    /// Open a folder chooser; the result arrives through `EditorState::add_shared_folder`.
    PickFolder,
    Save {
        profile: HostProfile,
        password: PasswordChange,
        connect: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Connection,
    Display,
    Experience,
    Devices,
    Advanced,
}

impl Section {
    const ALL: [Section; 5] = [
        Section::Connection,
        Section::Display,
        Section::Experience,
        Section::Devices,
        Section::Advanced,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Connection => "Connection",
            Section::Display => "Display",
            Section::Experience => "Experience",
            Section::Devices => "Devices",
            Section::Advanced => "Advanced",
        }
    }
}

pub struct EditorState {
    draft: HostProfile,
    is_new: bool,
    port_text: String,
    password: Zeroizing<String>,
    /// The user asked to forget the stored password.
    forget_password: bool,
    section: Section,
    error: Option<String>,
    pick_folder: bool,
    focus_first: bool,
}

impl EditorState {
    pub fn new(profile: HostProfile) -> Self {
        Self::build(profile, true)
    }

    pub fn edit(profile: HostProfile) -> Self {
        Self::build(profile, false)
    }

    fn build(profile: HostProfile, is_new: bool) -> Self {
        Self {
            port_text: profile.port.to_string(),
            draft: profile,
            is_new,
            password: Zeroizing::new(String::new()),
            forget_password: false,
            section: Section::Connection,
            error: None,
            pick_folder: false,
            focus_first: true,
        }
    }

    pub fn add_shared_folder(&mut self, path: std::path::PathBuf) {
        let name = path
            .file_name()
            .map_or_else(|| "Shared".to_owned(), |n| n.to_string_lossy().into_owned());
        if !self
            .draft
            .devices
            .shared_folders
            .iter()
            .any(|f| f.path == path)
        {
            self.draft
                .devices
                .shared_folders
                .push(SharedFolder { name, path });
        }
        self.section = Section::Devices;
    }

    pub fn profile_id(&self) -> Uuid {
        self.draft.id
    }

    fn validate(&mut self) -> Option<HostProfile> {
        let mut profile = self.draft.clone();
        profile.address = profile.address.trim().to_owned();
        profile.name = profile.name.trim().to_owned();
        profile.group = profile.group.trim().to_owned();
        if profile.address.is_empty() {
            self.error = Some("Enter the computer's address.".to_owned());
            self.section = Section::Connection;
            return None;
        }
        match self.port_text.trim().parse::<u16>() {
            Ok(port) if port > 0 => profile.port = port,
            _ => {
                self.error = Some("The port must be a number from 1 to 65535.".to_owned());
                self.section = Section::Connection;
                return None;
            }
        }
        if profile.name.is_empty() {
            profile.name = profile.address.clone();
        }
        if profile.display.resolution == ResolutionMode::Fixed
            && (profile.display.fixed_width < 200 || profile.display.fixed_height < 200)
        {
            self.error = Some("A fixed resolution must be at least 200 × 200.".to_owned());
            self.section = Section::Display;
            return None;
        }
        Some(profile)
    }

    fn password_change(&self) -> PasswordChange {
        if self.draft.save_password && !self.password.is_empty() {
            PasswordChange::Set(self.password.clone())
        } else if self.forget_password {
            PasswordChange::Forget
        } else {
            PasswordChange::Keep
        }
    }
}

/// Outer width, including the frame's margins.
pub const DRAWER_WIDTH: f32 = 468.0;

pub fn show(ui: &mut Ui, state: &mut EditorState, profiles: &ProfileStore) -> Vec<Action> {
    let mut actions = Vec::new();
    let p = Palette::of(ui);

    egui::Panel::right("host-editor")
        .exact_size(DRAWER_WIDTH)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.hairline))
                .inner_margin(egui::Margin::symmetric(24, 20)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let title = if state.is_new {
                    "New host"
                } else {
                    "Edit host"
                };
                ui.label(RichText::new(title).font(theme::font_semibold(size::SUBHEAD)));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::icon_button(ui, icon::X, "Close").clicked() {
                        actions.push(Action::Cancel);
                    }
                });
            });
            ui.add_space(12.0);
            section_tabs(ui, &mut state.section);
            ui.add_space(14.0);

            let footer_height = 64.0;
            let body_height = (ui.available_height() - footer_height).max(100.0);
            egui::ScrollArea::vertical()
                .max_height(body_height)
                .auto_shrink([false, false])
                .show(ui, |ui| match state.section {
                    Section::Connection => connection(ui, state, profiles),
                    Section::Display => display(ui, state),
                    Section::Experience => experience(ui, state),
                    Section::Devices => devices(ui, state),
                    Section::Advanced => advanced(ui, state),
                });

            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::primary_button(ui, "Save and connect").clicked()
                            && let Some(profile) = state.validate()
                        {
                            let password = state.password_change();
                            actions.push(Action::Save {
                                profile,
                                password,
                                connect: true,
                            });
                        }
                        if widgets::secondary_button(ui, "Save").clicked()
                            && let Some(profile) = state.validate()
                        {
                            let password = state.password_change();
                            actions.push(Action::Save {
                                profile,
                                password,
                                connect: false,
                            });
                        }
                    });
                });
                if let Some(error) = &state.error {
                    ui.add_space(4.0);
                    ui.label(RichText::new(error).color(p.poor));
                }
            });
        });

    if std::mem::take(&mut state.pick_folder) {
        actions.push(Action::PickFolder);
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) && !ui.ctx().egui_wants_keyboard_input() {
        actions.push(Action::Cancel);
    }
    actions
}

fn section_tabs(ui: &mut Ui, current: &mut Section) {
    let p = Palette::of(ui);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.spacing_mut().button_padding = vec2(9.0, 5.0);
        for section in Section::ALL {
            let selected = *current == section;
            let text = RichText::new(section.label())
                .font(theme::font_medium(size::SMALL + 1.0))
                .color(if selected { p.text } else { p.muted });
            let button = egui::Button::new(text)
                .fill(if selected {
                    p.raised
                } else {
                    Color32::TRANSPARENT
                })
                .stroke(Stroke::NONE)
                .corner_radius(CornerRadius::same(radius::CONTROL))
                .min_size(vec2(0.0, 28.0));
            if ui.add(button).clicked() {
                *current = section;
            }
        }
    });
}

fn connection(ui: &mut Ui, state: &mut EditorState, profiles: &ProfileStore) {
    let p = Palette::of(ui);

    let address = widgets::field(
        ui,
        "Address",
        &mut state.draft.address,
        "dc01.corp.example.com or 10.0.0.5",
    );
    if state.focus_first {
        state.focus_first = false;
        address.request_focus();
    }
    ui.add_space(6.0);

    pair(
        ui,
        0.72,
        |ui| {
            widgets::field(ui, "Name", &mut state.draft.name, "Shown in your host list");
        },
        |ui| {
            widgets::field(ui, "Port", &mut state.port_text, "3389");
        },
    );
    ui.add_space(14.0);
    widgets::section_label(ui, "Sign-in");
    ui.add_space(2.0);
    pair(
        ui,
        0.5,
        |ui| {
            widgets::field(ui, "User name", &mut state.draft.username, "jdoe");
        },
        |ui| {
            widgets::field(ui, "Domain", &mut state.draft.domain, "Optional");
        },
    );
    ui.add_space(6.0);

    let had_saved = state.draft.save_password && !state.is_new && !state.forget_password;
    ui.label(RichText::new("Password").size(size::SMALL).color(p.muted));
    let hint = if had_saved {
        "Saved in your keyring — type to replace"
    } else {
        "Asked when you connect"
    };
    widgets::text_input(ui, &mut state.password, hint, true);
    ui.add_space(4.0);
    widgets::setting_row(
        ui,
        "Remember password",
        Some("Stored in the system keyring, never in a file."),
        |ui| {
            let before = state.draft.save_password;
            widgets::toggle(ui, &mut state.draft.save_password);
            if before && !state.draft.save_password {
                state.forget_password = true;
            }
            if !before && state.draft.save_password {
                state.forget_password = false;
            }
        },
    );

    ui.add_space(14.0);
    widgets::section_label(ui, "Organize");
    ui.add_space(2.0);
    ui.label(RichText::new("Group").size(size::SMALL).color(p.muted));
    ui.horizontal(|ui| {
        let groups = profiles.groups();
        let width = if groups.is_empty() {
            ui.available_width()
        } else {
            ui.available_width() - 38.0
        };
        ui.add_sized(
            vec2(width, 34.0),
            egui::TextEdit::singleline(&mut state.draft.group)
                .hint_text(RichText::new("e.g. Office, Plant, Lab").color(p.muted))
                .margin(egui::Margin::symmetric(10, 8)),
        );
        if !groups.is_empty() {
            let pick = widgets::icon_button(ui, icon::CARET_DOWN, "Existing groups");
            egui::Popup::menu(&pick).show(|ui| {
                for group in groups {
                    if ui.button(&group).clicked() {
                        state.draft.group = group;
                    }
                }
            });
        }
    });
    ui.add_space(8.0);
    widgets::setting_row(ui, "Favorite", None, |ui| {
        widgets::toggle(ui, &mut state.draft.favorite);
    });

    ui.add_space(10.0);
    ui.label(
        RichText::new("Operating system")
            .size(size::SMALL)
            .color(p.muted),
    );
    // Automatic shows what Relay detected when it last connected.
    let automatic = match state.draft.detected_os {
        Some(os) => format!("Automatic ({})", os.label()),
        None => "Automatic (not detected yet)".to_owned(),
    };
    let label = |choice: OsChoice| match choice {
        OsChoice::Automatic => automatic.clone(),
        OsChoice::Windows => "Windows".to_owned(),
        OsChoice::Linux => "Linux".to_owned(),
        OsChoice::MacOs => "macOS".to_owned(),
    };
    egui::ComboBox::from_id_salt("os")
        .width(ui.available_width())
        .selected_text(label(state.draft.os))
        .show_ui(ui, |ui| {
            for choice in [
                OsChoice::Automatic,
                OsChoice::Windows,
                OsChoice::Linux,
                OsChoice::MacOs,
            ] {
                ui.selectable_value(&mut state.draft.os, choice, label(choice));
            }
        });
}

fn display(ui: &mut Ui, state: &mut EditorState) {
    let p = Palette::of(ui);
    let display = &mut state.draft.display;

    widgets::section_label(ui, "Resolution");
    ui.add_space(2.0);
    choice_card(
        ui,
        display.resolution == ResolutionMode::MatchWindow,
        "Match the window",
        "The remote desktop resizes with this window, pixel for pixel.",
    )
    .then(|| display.resolution = ResolutionMode::MatchWindow);
    choice_card(
        ui,
        display.resolution == ResolutionMode::Fixed,
        "Fixed size",
        "Always use one resolution, scaled down to fit when needed.",
    )
    .then(|| display.resolution = ResolutionMode::Fixed);

    if display.resolution == ResolutionMode::Fixed {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut display.fixed_width)
                    .range(200..=8192)
                    .suffix(" px"),
            );
            ui.label(RichText::new("×").color(p.muted));
            ui.add(
                egui::DragValue::new(&mut display.fixed_height)
                    .range(200..=8192)
                    .suffix(" px"),
            );
            for (w, h) in [(1920, 1080), (2560, 1440), (1280, 800)] {
                if ui
                    .add(
                        egui::Button::new(RichText::new(format!("{w}×{h}")).size(size::SMALL))
                            .frame(false),
                    )
                    .clicked()
                {
                    display.fixed_width = w;
                    display.fixed_height = h;
                }
            }
        });
    }

    ui.add_space(14.0);
    widgets::section_label(ui, "Scaling");
    ui.add_space(2.0);
    let current = display.scale_percent;
    let label = |s: Option<u32>| match s {
        None => "Same as this display".to_owned(),
        Some(v) => format!("{v}%"),
    };
    egui::ComboBox::from_id_salt("scale")
        .width(ui.available_width())
        .selected_text(label(current))
        .show_ui(ui, |ui| {
            for option in [
                None,
                Some(100),
                Some(125),
                Some(150),
                Some(175),
                Some(200),
                Some(250),
                Some(300),
            ] {
                ui.selectable_value(&mut display.scale_percent, option, label(option));
            }
        });

    ui.add_space(14.0);
    widgets::setting_row(
        ui,
        "Start in fullscreen",
        Some("Leave fullscreen with Ctrl+Alt+Enter."),
        |ui| {
            widgets::toggle(ui, &mut display.start_fullscreen);
        },
    );
}

fn experience(ui: &mut Ui, state: &mut EditorState) {
    let p = Palette::of(ui);
    let experience = &mut state.draft.experience;
    widgets::section_label(ui, "Quality");
    ui.add_space(2.0);
    for quality in Quality::ALL {
        if choice_card(
            ui,
            experience.quality == quality,
            quality.label(),
            quality.description(),
        ) {
            experience.quality = quality;
            quality.apply_visuals(experience);
        }
    }
    ui.label(
        RichText::new("Also sets the options below. You can still change them.")
            .size(size::SMALL)
            .color(p.muted),
    );
    ui.add_space(14.0);
    widgets::section_label(ui, "Graphics");
    ui.add_space(2.0);
    for mode in GraphicsMode::ALL {
        if choice_card(
            ui,
            experience.graphics == mode,
            mode.label(),
            mode.description(),
        ) {
            experience.graphics = mode;
        }
    }
    ui.add_space(14.0);
    widgets::section_label(ui, "Remote desktop");
    ui.add_space(2.0);
    widgets::setting_row(
        ui,
        "Desktop wallpaper",
        Some("Off saves bandwidth."),
        |ui| {
            widgets::toggle(ui, &mut experience.wallpaper);
        },
    );
    ui.add_space(4.0);
    widgets::setting_row(ui, "Window animations", None, |ui| {
        widgets::toggle(ui, &mut experience.animations);
    });
    ui.add_space(4.0);
    widgets::setting_row(
        ui,
        "Font smoothing",
        Some("ClearType text on the remote computer."),
        |ui| {
            widgets::toggle(ui, &mut experience.font_smoothing);
        },
    );
}

fn devices(ui: &mut Ui, state: &mut EditorState) {
    let p = Palette::of(ui);
    let devices = &mut state.draft.devices;

    widgets::setting_row(
        ui,
        "Clipboard",
        Some("Copy and paste text and images both ways."),
        |ui| {
            widgets::toggle(ui, &mut devices.clipboard);
        },
    );
    ui.add_space(14.0);
    widgets::section_label(ui, "Sound");
    ui.add_space(2.0);
    egui::ComboBox::from_id_salt("audio")
        .width(ui.available_width())
        .selected_text(devices.audio.label())
        .show_ui(ui, |ui| {
            for mode in AudioMode::ALL {
                ui.selectable_value(&mut devices.audio, mode, mode.label());
            }
        });

    ui.add_space(14.0);
    widgets::section_label(ui, "Shared folders");
    ui.label(
        RichText::new("These appear as drives under This PC on the remote computer.")
            .size(size::SMALL)
            .color(p.muted),
    );
    ui.add_space(4.0);
    let mut remove = None;
    for (i, folder) in devices.shared_folders.iter_mut().enumerate() {
        egui::Frame::new()
            .fill(p.canvas)
            .corner_radius(CornerRadius::same(radius::ROW))
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width() - 40.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut folder.name)
                                .hint_text("Drive name")
                                .desired_width(f32::INFINITY),
                        );
                        let mut path = folder.path.to_string_lossy().into_owned();
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut path)
                                    .hint_text("/home/you/Shared")
                                    .font(theme::mono(12.0))
                                    .desired_width(f32::INFINITY),
                            )
                            .changed()
                        {
                            folder.path = path.into();
                        }
                    });
                    if widgets::icon_button(ui, icon::TRASH, "Remove").clicked() {
                        remove = Some(i);
                    }
                });
            });
        ui.add_space(4.0);
    }
    if let Some(i) = remove {
        devices.shared_folders.remove(i);
    }
    if widgets::secondary_button(ui, format!("{}  Add folder…", icon::FOLDER_PLUS)).clicked() {
        state.pick_folder = true;
    }
}

fn advanced(ui: &mut Ui, state: &mut EditorState) {
    let p = Palette::of(ui);
    let advanced = &mut state.draft.advanced;

    ui.label(
        RichText::new("Keyboard layout on the remote computer")
            .size(size::SMALL)
            .color(p.muted),
    );
    let current = KEYBOARD_LAYOUTS
        .iter()
        .find(|(id, _)| *id == advanced.keyboard_layout)
        .map_or_else(
            || format!("Custom ({:#06x})", advanced.keyboard_layout),
            |(_, name)| (*name).to_owned(),
        );
    egui::ComboBox::from_id_salt("keyboard")
        .width(ui.available_width())
        .selected_text(current)
        .show_ui(ui, |ui| {
            for (id, name) in KEYBOARD_LAYOUTS {
                ui.selectable_value(&mut advanced.keyboard_layout, *id, *name);
            }
        });

    ui.add_space(10.0);
    widgets::field(
        ui,
        "Load balancing info",
        &mut advanced.load_balance_info,
        "From the .rdp file of an RDS farm (loadbalanceinfo)",
    );

    ui.add_space(14.0);
    widgets::setting_row(
        ui,
        "Reconnect automatically",
        Some("If the network drops, keep trying for up to five minutes."),
        |ui| {
            widgets::toggle(ui, &mut advanced.auto_reconnect);
        },
    );
}

/// Two fields side by side; the first takes `split` of the width.
fn pair(ui: &mut Ui, split: f32, left: impl FnOnce(&mut Ui), right: impl FnOnce(&mut Ui)) {
    let gap = 10.0;
    let width = ui.available_width();
    let left_w = ((width - gap) * split).floor();
    let right_w = width - gap - left_w;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        ui.allocate_ui_with_layout(vec2(left_w, 0.0), Layout::top_down(Align::Min), left);
        ui.allocate_ui_with_layout(vec2(right_w, 0.0), Layout::top_down(Align::Min), right);
    });
}

/// A selectable card with a title and one line of explanation. Returns true when clicked.
fn choice_card(ui: &mut Ui, selected: bool, title: &str, description: &str) -> bool {
    let p = Palette::of(ui);
    let response = egui::Frame::new()
        .fill(if selected {
            p.accent_soft()
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(
            1.0,
            if selected { p.accent } else { p.hairline },
        ))
        .corner_radius(CornerRadius::same(radius::ROW))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let glyph = if selected {
                    icon::RADIO_BUTTON
                } else {
                    icon::CIRCLE
                };
                ui.label(RichText::new(glyph).size(16.0).color(if selected {
                    p.accent
                } else {
                    p.muted
                }));
                ui.vertical(|ui| {
                    ui.label(RichText::new(title).font(theme::font_medium(size::BODY)));
                    ui.label(RichText::new(description).size(size::SMALL).color(p.muted));
                });
            });
        })
        .response
        .interact(egui::Sense::click());
    ui.add_space(4.0);
    response.clicked()
}

/// Common Windows keyboard layout identifiers (KLID).
const KEYBOARD_LAYOUTS: &[(u32, &str)] = &[
    (0x0409, "English (United States)"),
    (0x0809, "English (United Kingdom)"),
    (0x1009, "English (Canada)"),
    (0x0464, "Filipino"),
    (0x0407, "German"),
    (0x040C, "French"),
    (0x080C, "French (Belgium)"),
    (0x100C, "French (Switzerland)"),
    (0x0410, "Italian"),
    (0x040A, "Spanish"),
    (0x080A, "Spanish (Latin America)"),
    (0x0816, "Portuguese (Portugal)"),
    (0x0416, "Portuguese (Brazil)"),
    (0x0413, "Dutch"),
    (0x041D, "Swedish"),
    (0x0414, "Norwegian"),
    (0x0406, "Danish"),
    (0x040B, "Finnish"),
    (0x0415, "Polish"),
    (0x0405, "Czech"),
    (0x0419, "Russian"),
    (0x0422, "Ukrainian"),
    (0x041F, "Turkish"),
    (0x0411, "Japanese"),
    (0x0412, "Korean"),
    (0x0804, "Chinese (Simplified)"),
    (0x0404, "Chinese (Traditional)"),
];
