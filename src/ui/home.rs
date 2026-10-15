//! The host library: a rail of filters, a search / quick-connect field and the host list.

use chrono::{DateTime, Local, Utc};
use egui::{Align, Color32, CornerRadius, Key, Layout, RichText, Sense, Stroke, Ui, vec2};
use egui_phosphor::regular as icon;
use uuid::Uuid;

use super::theme::{self, Palette, radius, size};
use super::widgets::{self, Signal};
use crate::store::profiles::{HostProfile, ProfileStore, parse_endpoint};
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
    ImportRdpFile,
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
const CONTENT_MAX_WIDTH: f32 = 1040.0;

pub fn show(
    ui: &mut Ui,
    state: &mut HomeState,
    profiles: &ProfileStore,
    prober: &Prober,
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
                    main_column(ui, state, profiles, prober, &mut actions);
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
    prober: &Prober,
    actions: &mut Vec<Action>,
) {
    let p = Palette::of(ui);
    let title = match &state.filter {
        Filter::All => "All hosts",
        Filter::Favorites => "Favorites",
        Filter::Group(g) => g.as_str(),
    };
    ui.label(RichText::new(title).font(theme::font_semibold(size::HEADING)));
    ui.add_space(14.0);

    // Build the visible rows before drawing the search box so its key handling can use them.
    let query = state.query.trim().to_owned();
    let endpoint = parse_endpoint(&query);
    let hosts: Vec<&HostProfile> = profiles
        .hosts()
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
                for id in ids {
                    if let Some(host) = profiles.get(*id) {
                        let signal = prober.signal(host.address.trim(), host.port);
                        host_row(ui, host, signal, state.selected == Some(index), actions);
                    }
                    index += 1;
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

    let actions_w = if hovered { 176.0 } else { 0.0 };
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
    if hovered {
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

        let more = widgets::icon_button(&mut row_ui, icon::DOTS_THREE, "More");
        egui::Popup::menu(&more).show(|ui| {
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
        clicked_action |= more.clicked();

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
