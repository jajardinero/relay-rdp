//! The host library: a rail of filters, a search / quick-connect field and the host list.

use chrono::{DateTime, Local, Utc};
use egui::{Align, Color32, CornerRadius, Key, Layout, RichText, Sense, Stroke, Ui, vec2};
use egui_phosphor::regular as icon;
use uuid::Uuid;

use super::theme::{self, Palette, radius, size};
use super::widgets::{self, Signal};
use crate::store::credentials::CredentialStore;
use crate::store::profiles::{HostProfile, OsKind, ProfileStore, parse_endpoint};
use crate::store::settings::HostView;
use crate::util::probe::Prober;

pub enum Action {
    Connect(Uuid),
    QuickConnect(String, u16),
    /// Open the editor for a new host, optionally prefilled with an address.
    New(Option<String>),
    Edit(Uuid),
    Duplicate(Uuid),
    ToggleFavorite(Uuid),
    Delete(Uuid),
    OpenSettings,
    OpenPasswords,
    ImportRdpFile,
    SetHostView(HostView),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Favorites,
    Group(String),
}

pub struct HomeState {
    query: String,
    filter: Filter,
    selected: Option<usize>,
    focus_search: bool,
}

impl Default for HomeState {
    fn default() -> Self {
        Self {
            query: String::new(),
            filter: Filter::All,
            selected: None,
            focus_search: true,
        }
    }
}

#[derive(Clone)]
enum Row {
    Quick(String, u16),
    Host(Uuid),
}

const RAIL_WIDTH: f32 = 232.0;
const ROW_HEIGHT: f32 = 52.0;
const TILE_MIN_WIDTH: f32 = 200.0;
const TILE_HEIGHT: f32 = 132.0;
const TILE_GAP: f32 = 12.0;
const CONTENT_MAX_WIDTH: f32 = 1040.0;

pub fn show(
    ui: &mut Ui,
    state: &mut HomeState,
    profiles: &ProfileStore,
    credentials: &CredentialStore,
    prober: &Prober,
    view: HostView,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let p = Palette::of(ui);

    egui::Panel::left("home-rail")
        .exact_size(RAIL_WIDTH)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(p.canvas)
                .inner_margin(egui::Margin {
                    left: 14,
                    right: 10,
                    top: 18,
                    bottom: 16,
                }),
        )
        .show(ui, |ui| rail(ui, state, profiles, &mut actions));

    egui::CentralPanel::no_frame()
        .frame(
            egui::Frame::new()
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.hairline))
                .corner_radius(CornerRadius {
                    nw: radius::PANEL,
                    ne: 0,
                    sw: 0,
                    se: 0,
                })
                .inner_margin(egui::Margin {
                    left: 32,
                    right: 32,
                    top: 28,
                    bottom: 0,
                }),
        )
        .show(ui, |ui| {
            let width = ui.available_width().min(CONTENT_MAX_WIDTH);
            ui.allocate_ui_with_layout(
                vec2(width, ui.available_height()),
                Layout::top_down(Align::Min),
                |ui| {
                    main_column(ui, state, profiles, credentials, prober, view, &mut actions);
                },
            );
        });

    actions
}

fn rail(ui: &mut Ui, state: &mut HomeState, profiles: &ProfileStore, actions: &mut Vec<Action>) {
    let p = Palette::of(ui);

    ui.horizontal(|ui| {
        ui.add_space(6.0);
        logo(ui, &p);
        ui.add_space(4.0);
        ui.label(RichText::new(crate::APP_NAME).font(theme::font_semibold(size::SUBHEAD)));
    });
    ui.add_space(22.0);

    let hosts = profiles.hosts();
    let favorites = hosts.iter().filter(|h| h.favorite).count();
    nav_item(ui, state, Filter::All, icon::LIST, "All hosts", hosts.len());
    nav_item(
        ui,
        state,
        Filter::Favorites,
        icon::STAR,
        "Favorites",
        favorites,
    );

    let groups = profiles.groups();
    if !groups.is_empty() {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            widgets::section_label(ui, "Groups");
        });
        ui.add_space(2.0);
        for group in groups {
            let count = hosts.iter().filter(|h| h.group.trim() == group).count();
            nav_item(
                ui,
                state,
                Filter::Group(group.clone()),
                icon::FOLDER_SIMPLE,
                &group,
                count,
            );
        }
    }

    ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
        if rail_button(ui, icon::GEAR_SIX, "Settings").clicked() {
            actions.push(Action::OpenSettings);
        }
        if rail_button(ui, icon::FILE_ARROW_DOWN, "Import .rdp file").clicked() {
            actions.push(Action::ImportRdpFile);
        }
        if rail_button(ui, icon::KEY, "Passwords").clicked() {
            actions.push(Action::OpenPasswords);
        }
        if rail_button(ui, icon::PLUS, "New host").clicked() {
            actions.push(Action::New(None));
        }
    });
}

/// The mark: the four-bar link glyph, at full strength.
fn logo(ui: &mut Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(20.0, 18.0), Sense::hover());
    let painter = ui.painter_at(rect);
    for i in 0..4 {
        let h = 6.0 + i as f32 * 4.0;
        let x = rect.left() + i as f32 * 5.0;
        let bar = egui::Rect::from_min_max(
            egui::pos2(x, rect.bottom() - h),
            egui::pos2(x + 3.5, rect.bottom()),
        );
        painter.rect_filled(bar, CornerRadius::same(1), p.accent);
    }
}

fn nav_item(
    ui: &mut Ui,
    state: &mut HomeState,
    filter: Filter,
    glyph: &str,
    label: &str,
    count: usize,
) {
    let p = Palette::of(ui);
    let selected = state.filter == filter;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    let fill = if selected {
        p.accent_soft()
    } else if response.hovered() {
        p.hover()
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(radius::ROW), fill);
    let fg = if selected {
        p.accent
    } else if response.hovered() {
        p.text
    } else {
        p.muted
    };
    let text_color = if selected { p.text } else { fg };
    ui.painter().text(
        rect.left_center() + vec2(12.0, 0.0),
        egui::Align2::LEFT_CENTER,
        glyph,
        theme::font(16.0),
        fg,
    );
    ui.painter().text(
        rect.left_center() + vec2(36.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        theme::font_medium(size::BODY),
        text_color,
    );
    ui.painter().text(
        rect.right_center() - vec2(10.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        count.to_string(),
        theme::font(size::SMALL),
        p.muted,
    );
    if response.clicked() {
        state.filter = filter;
        state.selected = None;
    }
}

fn rail_button(ui: &mut Ui, glyph: &str, label: &str) -> egui::Response {
    let p = Palette::of(ui);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(radius::ROW), p.hover());
    }
    let color = if response.hovered() { p.text } else { p.muted };
    ui.painter().text(
        rect.left_center() + vec2(12.0, 0.0),
        egui::Align2::LEFT_CENTER,
        glyph,
        theme::font(16.0),
        color,
    );
    ui.painter().text(
        rect.left_center() + vec2(36.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        theme::font_medium(size::BODY),
        color,
    );
    response
}

fn main_column(
    ui: &mut Ui,
    state: &mut HomeState,
    profiles: &ProfileStore,
    credentials: &CredentialStore,
    prober: &Prober,
    view: HostView,
    actions: &mut Vec<Action>,
) {
    let p = Palette::of(ui);
    let title = match &state.filter {
        Filter::All => "All hosts",
        Filter::Favorites => "Favorites",
        Filter::Group(g) => g.as_str(),
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).font(theme::font_semibold(size::HEADING)));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            // Right to left: Tiles ends up on the right.
            for (option, glyph, label) in [
                (HostView::Tiles, icon::SQUARES_FOUR, "Tiles"),
                (HostView::List, icon::LIST, "List"),
            ] {
                let active = view == option;
                let button = egui::Button::new(RichText::new(glyph).size(16.0).color(if active {
                    p.on_accent
                } else {
                    p.muted
                }))
                .fill(if active { p.accent } else { p.raised })
                .corner_radius(CornerRadius::same(radius::CONTROL))
                .min_size(vec2(32.0, 28.0));
                if ui.add(button).on_hover_text(label).clicked() && !active {
                    actions.push(Action::SetHostView(option));
                }
            }
        });
    });
    ui.add_space(14.0);

    // Build the visible rows before drawing the search box so its key handling can use them.
    let query = state.query.trim().to_owned();
    let endpoint = parse_endpoint(&query);
    // Resolve display accounts without changing the saved manual credentials.
    let display_hosts: Vec<HostProfile> = profiles
        .hosts()
        .iter()
        .map(|host| {
            let mut display = host.clone();
            if host.credential_profile_id.is_some() {
                display.username = credentials.account_label(host);
                display.domain.clear();
            }
            display
        })
        .collect();
    let hosts: Vec<&HostProfile> = display_hosts
        .iter()
        .filter(|h| match &state.filter {
            Filter::All => true,
            Filter::Favorites => h.favorite,
            Filter::Group(g) => h.group.trim() == g,
        })
        .filter(|h| query.is_empty() || matches_query(h, &query))
        .collect();

    let exact = endpoint.as_ref().is_some_and(|(host, port)| {
        hosts
            .iter()
            .any(|h| h.address.eq_ignore_ascii_case(host) && h.port == *port)
    });

    let mut rows: Vec<Row> = Vec::new();
    if let Some((host, port)) = &endpoint
        && !exact
    {
        rows.push(Row::Quick(host.clone(), *port));
    }

    let sections = build_sections(&hosts, &state.filter, query.is_empty());
    for (_, ids) in &sections {
        rows.extend(ids.iter().map(|id| Row::Host(*id)));
    }

    if let Some(selected) = state.selected
        && selected >= rows.len()
    {
        state.selected = rows.len().checked_sub(1);
    }

    search_box(ui, state, &rows, actions);
    ui.add_space(20.0);

    if profiles.hosts().is_empty() && rows.is_empty() {
        empty_state(ui, &p, actions);
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut index = 0usize;
            if let Some(Row::Quick(host, port)) = rows.first() {
                if quick_row(ui, host, *port, state.selected == Some(0)).clicked() {
                    actions.push(Action::QuickConnect(host.clone(), *port));
                }
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(format!("{}  Save as a host", icon::PLUS))
                                .size(size::SMALL)
                                .color(p.muted),
                        )
                        .frame(false),
                    )
                    .clicked()
                {
                    actions.push(Action::New(Some(query.clone())));
                }
                ui.add_space(18.0);
                index = 1;
            }

            if sections.is_empty() && !query.is_empty() && endpoint.is_none() {
                ui.label(RichText::new(format!("No hosts match “{query}”.")).color(p.muted));
            }

            for (heading, ids) in &sections {
                if let Some(heading) = heading {
                    widgets::section_label(ui, heading);
                    ui.add_space(2.0);
                }
                let hosts: Vec<&HostProfile> = ids
                    .iter()
                    .filter_map(|id| display_hosts.iter().find(|host| host.id == *id))
                    .collect();
                // A host can appear in both Recent and its group. Keep each row's or tile's
                // widget/menu IDs stable and distinct as hover controls appear.
                match view {
                    HostView::List => {
                        for host in hosts {
                            let signal = prober.signal(host.address.trim(), host.port);
                            ui.push_id((heading, host.id), |ui| {
                                host_row(ui, host, signal, state.selected == Some(index), actions);
                            });
                            index += 1;
                        }
                    }
                    HostView::Tiles => {
                        let width = ui.available_width();
                        let columns = ((width + TILE_GAP) / (TILE_MIN_WIDTH + TILE_GAP))
                            .floor()
                            .max(1.0);
                        let tile_width = (width - TILE_GAP * (columns - 1.0)) / columns;
                        for row in hosts.chunks(columns as usize) {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = TILE_GAP;
                                for host in row {
                                    let signal = prober.signal(host.address.trim(), host.port);
                                    let selected = state.selected == Some(index);
                                    ui.push_id((heading, host.id), |ui| {
                                        host_tile(ui, host, signal, selected, tile_width, actions);
                                    });
                                    index += 1;
                                }
                            });
                            ui.add_space(TILE_GAP);
                        }
                    }
                }
                ui.add_space(18.0);
            }
        });
}

fn matches_query(host: &HostProfile, query: &str) -> bool {
    let q = query.to_lowercase();
    [&host.name, &host.address, &host.username, &host.group]
        .iter()
        .any(|field| field.to_lowercase().contains(&q))
}

/// Sections in display order: recent first (unfiltered "All" view only), then groups.
fn build_sections(
    hosts: &[&HostProfile],
    filter: &Filter,
    unfiltered: bool,
) -> Vec<(Option<String>, Vec<Uuid>)> {
    let mut sections = Vec::new();

    if *filter == Filter::All && unfiltered {
        let mut recent: Vec<&&HostProfile> = hosts
            .iter()
            .filter(|h| h.last_connected.is_some())
            .collect();
        recent.sort_by(|a, b| b.last_connected.cmp(&a.last_connected));
        recent.truncate(4);
        if !recent.is_empty() && hosts.len() > 4 {
            sections.push((
                Some("Recent".to_owned()),
                recent.iter().map(|h| h.id).collect(),
            ));
        }
    }

    if !matches!(filter, Filter::All) || !unfiltered {
        let mut ids: Vec<&&HostProfile> = hosts.iter().collect();
        ids.sort_by_key(|h| h.display_name().to_lowercase());
        if !ids.is_empty() {
            sections.push((None, ids.iter().map(|h| h.id).collect()));
        }
        return sections;
    }

    let mut groups: Vec<String> = Vec::new();
    for host in hosts {
        let g = host.group.trim().to_owned();
        if !groups.contains(&g) {
            groups.push(g);
        }
    }
    groups.sort_by(|a, b| (a.is_empty(), a.to_lowercase()).cmp(&(b.is_empty(), b.to_lowercase())));
    let only_ungrouped = groups.len() == 1 && groups[0].is_empty();
    for group in groups {
        let mut members: Vec<&&HostProfile> =
            hosts.iter().filter(|h| h.group.trim() == group).collect();
        members.sort_by_key(|h| h.display_name().to_lowercase());
        let heading = if only_ungrouped {
            None
        } else if group.is_empty() {
            Some("Other hosts".to_owned())
        } else {
            Some(group)
        };
        sections.push((heading, members.iter().map(|h| h.id).collect()));
    }
    sections
}

fn search_box(ui: &mut Ui, state: &mut HomeState, rows: &[Row], actions: &mut Vec<Action>) {
    let p = Palette::of(ui);
    let height = 46.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());

    let id = egui::Id::new("home-search");
    let focused = ui.memory(|m| m.has_focus(id));
    let stroke = if focused {
        Stroke::new(1.5, p.accent)
    } else {
        Stroke::new(1.0, p.hairline)
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(radius::ROW),
        p.canvas,
        stroke,
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.left_center() + vec2(16.0, 0.0),
        egui::Align2::LEFT_CENTER,
        icon::MAGNIFYING_GLASS,
        theme::font(18.0),
        p.muted,
    );

    let shortcut_w = 64.0;
    let edit_rect = egui::Rect::from_min_max(
        rect.left_top() + vec2(42.0, 0.0),
        rect.right_bottom() - vec2(shortcut_w, 0.0),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(edit_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let response = child.add(
        egui::TextEdit::singleline(&mut state.query)
            .id(id)
            .frame(egui::Frame::NONE)
            .font(theme::font(size::SUBHEAD - 1.0))
            .hint_text(RichText::new("Search hosts, or type an address to connect").color(p.muted))
            .desired_width(edit_rect.width()),
    );

    ui.painter().text(
        rect.right_center() - vec2(16.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        "Ctrl K",
        theme::font(size::SMALL),
        p.muted,
    );

    if state.focus_search || ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::K)) {
        state.focus_search = false;
        response.request_focus();
    }
    if response.changed() {
        state.selected = (!state.query.trim().is_empty() && !rows.is_empty()).then_some(0);
    }

    if response.has_focus() || response.lost_focus() {
        let (down, up, enter) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                i.key_pressed(Key::Enter),
            )
        });
        if !rows.is_empty() {
            if down {
                state.selected = Some(state.selected.map_or(0, |s| (s + 1).min(rows.len() - 1)));
            }
            if up {
                state.selected = state.selected.map(|s| s.saturating_sub(1));
            }
        }
        if enter && response.lost_focus() {
            match state.selected.and_then(|s| rows.get(s)).or(rows.first()) {
                Some(Row::Quick(host, port)) => {
                    actions.push(Action::QuickConnect(host.clone(), *port))
                }
                Some(Row::Host(id)) => actions.push(Action::Connect(*id)),
                None => {}
            }
        }
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            state.query.clear();
            state.selected = None;
        }
    }
}

fn quick_row(ui: &mut Ui, host: &str, port: u16, selected: bool) -> egui::Response {
    let p = Palette::of(ui);
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    let fill = if selected || response.hovered() {
        p.accent_soft()
    } else {
        p.canvas
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(radius::ROW),
        fill,
        Stroke::new(1.0, p.accent.gamma_multiply(0.5)),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.left_center() + vec2(16.0, 0.0),
        egui::Align2::LEFT_CENTER,
        icon::ARROW_RIGHT,
        theme::font(18.0),
        p.accent,
    );
    let endpoint = if port == crate::store::profiles::DEFAULT_PORT {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    };
    let galley = ui.painter().layout_no_wrap(
        "Connect to ".to_owned(),
        theme::font_medium(size::BODY),
        p.text,
    );
    let x = rect.left() + 46.0;
    let y = rect.center().y - galley.size().y / 2.0;
    let w = galley.size().x;
    ui.painter().galley(egui::pos2(x, y), galley, p.text);
    ui.painter().text(
        egui::pos2(x + w, rect.center().y),
        egui::Align2::LEFT_CENTER,
        endpoint,
        theme::mono(size::BODY),
        p.text,
    );
    ui.painter().text(
        rect.right_center() - vec2(16.0, 0.0),
        egui::Align2::RIGHT_CENTER,
        "Enter",
        theme::font(size::SMALL),
        p.muted,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn host_row(
    ui: &mut Ui,
    host: &HostProfile,
    signal: Signal,
    selected: bool,
    actions: &mut Vec<Action>,
) {
    let p = Palette::of(ui);
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    let hovered = response.hovered() || ui.rect_contains_pointer(rect);
    let more_id = ui.make_persistent_id("host-more");
    // The menu extends beyond the row; keep drawing its anchor and contents when the
    // pointer leaves the row to enter the menu.
    let show_actions = hovered || egui::Popup::is_id_open(ui.ctx(), more_id);
    let fill = if selected {
        p.accent_soft()
    } else if hovered {
        p.hover()
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(radius::ROW), fill);
    // Hairline separators between rows, inset to the text column.
    ui.painter().hline(
        rect.left() + 46.0..=rect.right() - 12.0,
        rect.bottom(),
        Stroke::new(1.0, p.hairline.gamma_multiply(0.6)),
    );

    // Signal glyph.
    let mut glyph_ui = ui.new_child(
        egui::UiBuilder::new().max_rect(egui::Rect::from_center_size(
            rect.left_center() + vec2(24.0, 0.0),
            vec2(16.0, 14.0),
        )),
    );
    widgets::signal_bars(&mut glyph_ui, signal).on_hover_text(signal.label());

    let actions_w = if show_actions { 176.0 } else { 0.0 };
    let address_x = rect.left() + (rect.width() * 0.45).max(260.0);
    let meta_x = rect.right() - 16.0 - actions_w;

    // Name and account.
    let name_x = rect.left() + 46.0;
    let name_clip = egui::Rect::from_min_max(
        egui::pos2(name_x, rect.top()),
        egui::pos2(address_x - 16.0, rect.bottom()),
    );
    let painter = ui.painter_at(name_clip);
    let account = host.account_label();
    let name_y = if account.is_empty() {
        rect.center().y
    } else {
        rect.center().y - 8.0
    };
    painter.text(
        egui::pos2(name_x, name_y),
        egui::Align2::LEFT_CENTER,
        host.display_name(),
        theme::font_medium(size::BODY),
        p.text,
    );
    if host.favorite {
        let name_w = ui
            .painter()
            .layout_no_wrap(
                host.display_name().to_owned(),
                theme::font_medium(size::BODY),
                p.text,
            )
            .size()
            .x;
        painter.text(
            egui::pos2(name_x + name_w + 6.0, name_y),
            egui::Align2::LEFT_CENTER,
            icon::STAR,
            theme::font(12.0),
            p.fair,
        );
    }
    if !account.is_empty() {
        painter.text(
            egui::pos2(name_x, rect.center().y + 9.0),
            egui::Align2::LEFT_CENTER,
            account,
            theme::font(size::SMALL),
            p.muted,
        );
    }

    // Address.
    let address_clip = egui::Rect::from_min_max(
        egui::pos2(address_x, rect.top()),
        egui::pos2(meta_x - 120.0, rect.bottom()),
    );
    ui.painter_at(address_clip).text(
        egui::pos2(address_x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        host.endpoint(),
        theme::mono(13.0),
        p.muted,
    );

    // Latency + last used, right-aligned.
    let (signal_text, signal_color) = match signal {
        Signal::Offline => ("Offline".to_owned(), p.poor),
        Signal::Unknown => (String::new(), p.muted),
        Signal::Rtt(_) => (signal.label(), p.muted),
    };
    ui.painter().text(
        egui::pos2(meta_x - 110.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        signal_text,
        theme::mono(12.0),
        signal_color,
    );
    ui.painter().text(
        egui::pos2(meta_x, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        last_used(host.last_connected),
        theme::font(size::SMALL),
        p.muted,
    );

    let mut clicked_action = false;
    if show_actions {
        let actions_rect = egui::Rect::from_min_max(
            egui::pos2(rect.right() - actions_w, rect.top()),
            egui::pos2(rect.right() - 10.0, rect.bottom()),
        );
        let mut row_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(actions_rect)
                .layout(Layout::right_to_left(Align::Center)),
        );
        row_ui.spacing_mut().item_spacing.x = 2.0;

        clicked_action |= more_menu(&mut row_ui, host, more_id, actions).clicked();

        if widgets::icon_button(&mut row_ui, icon::PENCIL_SIMPLE, "Edit").clicked() {
            actions.push(Action::Edit(host.id));
            clicked_action = true;
        }
        row_ui.add_space(6.0);
        if widgets::primary_button(&mut row_ui, "Connect").clicked() {
            actions.push(Action::Connect(host.id));
            clicked_action = true;
        }
    }

    if !clicked_action && response.double_clicked() {
        actions.push(Action::Connect(host.id));
    }
}

/// The "More" button and its menu: duplicate, favorite, delete.
fn more_menu(
    ui: &mut Ui,
    host: &HostProfile,
    id: egui::Id,
    actions: &mut Vec<Action>,
) -> egui::Response {
    let p = Palette::of(ui);
    let more = widgets::icon_button(ui, icon::DOTS_THREE, "More");
    egui::Popup::menu(&more).id(id).show(|ui| {
        ui.set_min_width(170.0);
        if ui.button(format!("{}  Duplicate", icon::COPY)).clicked() {
            actions.push(Action::Duplicate(host.id));
        }
        let favorite_label = if host.favorite {
            "Remove from favorites"
        } else {
            "Add to favorites"
        };
        if ui
            .button(format!("{}  {favorite_label}", icon::STAR))
            .clicked()
        {
            actions.push(Action::ToggleFavorite(host.id));
        }
        ui.separator();
        if ui
            .button(RichText::new(format!("{}  Delete", icon::TRASH)).color(p.poor))
            .clicked()
        {
            actions.push(Action::Delete(host.id));
        }
    });
    more
}

/// `text` on one line, shortened with an ellipsis to fit `width`.
fn fitted(ui: &Ui, text: &str, font: egui::FontId, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job =
        egui::text::LayoutJob::simple_singleline(text.to_owned(), font, Color32::PLACEHOLDER);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(0.0));
    ui.fonts_mut(|fonts| fonts.layout_job(job))
}

fn os_glyph(os: OsKind) -> &'static str {
    match os {
        OsKind::Windows => icon::WINDOWS_LOGO,
        OsKind::Linux => icon::LINUX_LOGO,
        OsKind::MacOs => icon::APPLE_LOGO,
        OsKind::Unknown => icon::DESKTOP,
    }
}

/// A host as a tile: its operating system's logo, name, address, and the detected operating
/// system (or when it was last used). Hovering shows the same actions as a row.
fn host_tile(
    ui: &mut Ui,
    host: &HostProfile,
    signal: Signal,
    selected: bool,
    width: f32,
    actions: &mut Vec<Action>,
) {
    let p = Palette::of(ui);
    let (rect, response) = ui.allocate_exact_size(vec2(width, TILE_HEIGHT), Sense::click());
    let hovered = response.hovered() || ui.rect_contains_pointer(rect);
    let more_id = ui.make_persistent_id("host-more");
    let show_actions = hovered || egui::Popup::is_id_open(ui.ctx(), more_id);
    let fill = if selected {
        p.accent_soft()
    } else if hovered {
        p.hover()
    } else {
        p.raised
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(radius::PANEL),
        fill,
        Stroke::new(1.0, p.hairline),
        egui::StrokeKind::Inside,
    );
    let inner = rect.shrink(14.0);
    let painter = ui.painter_at(inner);

    let os = host.os_kind();
    painter.text(
        inner.left_top(),
        egui::Align2::LEFT_TOP,
        os_glyph(os),
        theme::font(30.0),
        if os == OsKind::Unknown {
            p.muted
        } else {
            p.text
        },
    );

    let name_y = inner.top() + 50.0;
    let star_w = if host.favorite { 20.0 } else { 0.0 };
    let name = fitted(
        ui,
        host.display_name(),
        theme::font_medium(size::BODY),
        inner.width() - star_w,
    );
    let name_w = name.size().x;
    painter.galley(egui::pos2(inner.left(), name_y), name, p.text);
    if host.favorite {
        painter.text(
            egui::pos2(inner.left() + name_w + 6.0, name_y + 1.0),
            egui::Align2::LEFT_TOP,
            icon::STAR,
            theme::font(12.0),
            p.fair,
        );
    }
    let address = fitted(ui, &host.endpoint(), theme::mono(12.0), inner.width());
    painter.galley(egui::pos2(inner.left(), name_y + 20.0), address, p.muted);
    let detail = if host.credential_profile_id.is_some() {
        host.account_label()
    } else {
        host.os_label()
            .unwrap_or_else(|| last_used(host.last_connected))
    };
    // On hover the Connect button takes the bottom right.
    let detail_w = inner.width() - if show_actions { 96.0 } else { 0.0 };
    let detail = fitted(ui, &detail, theme::font(size::SMALL), detail_w);
    let detail_y = inner.bottom() - detail.size().y;
    painter.galley(egui::pos2(inner.left(), detail_y), detail, p.muted);

    let mut clicked_action = false;
    if show_actions {
        let top = egui::Rect::from_min_max(
            egui::pos2(rect.right() - 100.0, rect.top() + 8.0),
            egui::pos2(rect.right() - 8.0, rect.top() + 40.0),
        );
        let mut top_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(top)
                .layout(Layout::right_to_left(Align::Center)),
        );
        top_ui.spacing_mut().item_spacing.x = 2.0;
        clicked_action |= more_menu(&mut top_ui, host, more_id, actions).clicked();
        if widgets::icon_button(&mut top_ui, icon::PENCIL_SIMPLE, "Edit").clicked() {
            actions.push(Action::Edit(host.id));
            clicked_action = true;
        }

        let bottom = egui::Rect::from_min_max(
            egui::pos2(rect.right() - 110.0, rect.bottom() - 44.0),
            egui::pos2(rect.right() - 10.0, rect.bottom() - 8.0),
        );
        let mut bottom_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(bottom)
                .layout(Layout::right_to_left(Align::Center)),
        );
        if widgets::primary_button(&mut bottom_ui, "Connect").clicked() {
            actions.push(Action::Connect(host.id));
            clicked_action = true;
        }
    } else {
        let mut glyph_ui = ui.new_child(egui::UiBuilder::new().max_rect(
            egui::Rect::from_center_size(inner.right_top() + vec2(-8.0, 8.0), vec2(16.0, 14.0)),
        ));
        widgets::signal_bars(&mut glyph_ui, signal).on_hover_text(signal.label());
    }

    if !clicked_action && response.double_clicked() {
        actions.push(Action::Connect(host.id));
    }
}

fn empty_state(ui: &mut Ui, p: &Palette, actions: &mut Vec<Action>) {
    ui.add_space(48.0);
    ui.vertical(|ui| {
        ui.label(RichText::new("No saved hosts yet").font(theme::font_semibold(size::SUBHEAD)));
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Type an address above to connect right away, or save a host to keep its settings and sign-in.",
            )
            .color(p.muted),
        );
        ui.add_space(16.0);
        if widgets::primary_button(ui, format!("{}  New host", icon::PLUS)).clicked() {
            actions.push(Action::New(None));
        }
    });
}

fn last_used(when: Option<DateTime<Utc>>) -> String {
    let Some(when) = when else {
        return "Never used".to_owned();
    };
    let local = when.with_timezone(&Local);
    let today = Local::now().date_naive();
    let date = local.date_naive();
    if date == today {
        local.format("Today %H:%M").to_string()
    } else if today.pred_opt() == Some(date) {
        "Yesterday".to_owned()
    } else if (today - date).num_days() < 7 {
        local.format("%A").to_string()
    } else {
        local.format("%b %-d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Context, Event, Id, Modifiers, PointerButton, Pos2, Rect, Shape, pos2};

    #[test]
    fn passwords_button_sits_below_new_host_and_opens_management() {
        let ctx = Context::default();
        theme::install_fonts(&ctx);
        let mut state = HomeState::default();
        let profiles = ProfileStore::default();
        let mut render = |events| {
            let mut actions = Vec::new();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(720.0, 480.0))),
                    events,
                    ..Default::default()
                },
                |root| {
                    egui::Panel::left("test-rail")
                        .exact_size(RAIL_WIDTH)
                        .show(root, |ui| {
                            rail(ui, &mut state, &profiles, &mut actions);
                        });
                },
            );
            output.textures_delta.clear();
            (output, actions)
        };
        let (output, _) = render(vec![]);
        let label_pos = |label| {
            output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    Shape::Text(text) if text.galley.text() == label => {
                        Some(text.pos + text.galley.size() / 2.0)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing sidebar entry: {label}"))
        };
        let new_host = label_pos("New host");
        let passwords = label_pos("Passwords");
        let import = label_pos("Import .rdp file");
        assert!(new_host.y < passwords.y && passwords.y < import.y);
        render(vec![Event::PointerMoved(passwords)]);
        render(vec![pointer_button(passwords, true)]);
        let (_, actions) = render(vec![pointer_button(passwords, false)]);
        assert!(matches!(actions.as_slice(), [Action::OpenPasswords]));
    }

    struct MenuFrame {
        menu: Id,
        other_menu: Id,
        more: Pos2,
        duplicate: Option<Pos2>,
        actions: Vec<Action>,
    }

    fn frame(ctx: &Context, host: &HostProfile, events: Vec<Event>) -> MenuFrame {
        frame_in(ctx, host, events, HostView::List)
    }

    fn frame_in(
        ctx: &Context,
        host: &HostProfile,
        events: Vec<Event>,
        view: HostView,
    ) -> MenuFrame {
        let mut result = MenuFrame {
            menu: Id::NULL,
            other_menu: Id::NULL,
            more: Pos2::ZERO,
            duplicate: None,
            actions: Vec::new(),
        };
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))),
                events,
                ..Default::default()
            },
            |root| {
                egui::CentralPanel::default().show(root, |ui| {
                    for heading in [Some("Recent".to_owned()), None] {
                        ui.push_id((&heading, host.id), |ui| {
                            let id = ui.make_persistent_id("host-more");
                            if heading.is_some() {
                                result.menu = id;
                                result.more = ui.cursor().min
                                    + match view {
                                        HostView::List => {
                                            vec2(ui.available_width() - 25.0, ROW_HEIGHT / 2.0)
                                        }
                                        HostView::Tiles => vec2(TILE_WIDTH - 23.0, 24.0),
                                    };
                            } else {
                                result.other_menu = id;
                            }
                            let actions = &mut result.actions;
                            match view {
                                HostView::List => {
                                    host_row(ui, host, Signal::Unknown, false, actions);
                                }
                                HostView::Tiles => {
                                    host_tile(
                                        ui,
                                        host,
                                        Signal::Unknown,
                                        false,
                                        TILE_WIDTH,
                                        actions,
                                    );
                                }
                            }
                        });
                    }
                });
            },
        );
        result.duplicate = output.shapes.iter().find_map(|shape| match &shape.shape {
            Shape::Text(text) if text.galley.text().contains("Duplicate") => {
                Some(text.pos + vec2(30.0, 5.0))
            }
            _ => None,
        });
        output.textures_delta.clear(); // Headless interaction test has no GPU texture upload.
        result
    }

    fn pointer_button(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    const TILE_WIDTH: f32 = 220.0;

    /// Tiles share the rows' menu: it opens from the tile's More button and selects.
    #[test]
    fn tile_more_menu_opens_and_selects() {
        let ctx = Context::default();
        theme::install_fonts(&ctx);
        let host = HostProfile::new("example.test", 3389);
        let frame = |events| frame_in(&ctx, &host, events, HostView::Tiles);
        let more = frame(vec![]).more;
        frame(vec![Event::PointerMoved(more)]);
        frame(vec![pointer_button(more, true)]);
        frame(vec![pointer_button(more, false)]);
        let opened = frame(vec![]);
        assert!(egui::Popup::is_id_open(&ctx, opened.menu));
        assert!(!egui::Popup::is_id_open(&ctx, opened.other_menu));
        let option = opened
            .duplicate
            .expect("Duplicate option should be visible");
        frame(vec![Event::PointerMoved(option)]);
        frame(vec![pointer_button(option, true)]);
        let selected = frame(vec![pointer_button(option, false)]);
        assert!(matches!(selected.actions.as_slice(), [Action::Duplicate(id)] if *id == host.id));
    }

    #[test]
    fn more_menu_stays_visible_when_hovering_and_selecting_an_option() {
        let ctx = Context::default();
        theme::install_fonts(&ctx);
        let host = HostProfile::new("example.test", 3389);
        let initial = frame(&ctx, &host, vec![]);
        assert_ne!(initial.menu, initial.other_menu);
        let more = initial.more;
        frame(&ctx, &host, vec![Event::PointerMoved(more)]);
        frame(&ctx, &host, vec![pointer_button(more, true)]);
        frame(&ctx, &host, vec![pointer_button(more, false)]);
        let opened = frame(&ctx, &host, vec![]);
        assert!(egui::Popup::is_id_open(&ctx, opened.menu));
        assert!(!egui::Popup::is_id_open(&ctx, opened.other_menu));
        let option = opened
            .duplicate
            .expect("Duplicate option should be visible");
        assert!(option.y > more.y + ROW_HEIGHT / 2.0);
        for _ in 0..3 {
            let hovered = frame(&ctx, &host, vec![Event::PointerMoved(option)]);
            assert!(
                hovered.duplicate.is_some(),
                "menu disappeared when leaving the row"
            );
        }
        frame(&ctx, &host, vec![pointer_button(option, true)]);
        let selected = frame(&ctx, &host, vec![pointer_button(option, false)]);
        assert!(matches!(selected.actions.as_slice(), [Action::Duplicate(id)] if *id == host.id));
        assert!(!egui::Popup::is_id_open(&ctx, selected.menu));

        // Reopening and clicking outside still dismisses the menu.
        frame(&ctx, &host, vec![Event::PointerMoved(more)]);
        frame(&ctx, &host, vec![pointer_button(more, true)]);
        frame(&ctx, &host, vec![pointer_button(more, false)]);
        let outside = pos2(20.0, 400.0);
        frame(
            &ctx,
            &host,
            vec![Event::PointerMoved(outside), pointer_button(outside, true)],
        );
        let closed = frame(&ctx, &host, vec![pointer_button(outside, false)]);
        assert!(!egui::Popup::is_id_open(&ctx, closed.menu));
    }
}
