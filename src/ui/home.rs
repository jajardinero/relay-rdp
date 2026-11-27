//! The host library: a rail of filters, a search / quick-connect field and the host list.

use std::sync::Arc;

use chrono::{DateTime, Local, Utc};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    Anchor, AnyElement, App, AppContext as _, Context, Entity, Focusable as _, FontWeight, Hsla,
    InteractiveElement, IntoElement, ObjectFit, ParentElement, RenderImage, SharedString,
    StatefulInteractiveElement, Styled, StyledImage as _, Subscription, Window, div, img,
    prelude::FluentBuilder as _, px, rgb, rgba, transparent_black,
};
use uuid::Uuid;

use super::RelayApp;
use super::assets::{icon_svg, icons};
use super::editor::DRAWER_WIDTH;
use super::theme::{FONT_MONO, Palette, radius, size};
use super::widgets::{self, Signal};
use crate::store::profiles::{DEFAULT_PORT, HostProfile, OsKind, parse_endpoint};
use crate::store::settings::HostView;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Favorites,
    Group(String),
    /// A page of the SSH section, drawn by the embedded RusSSH vault.
    Ssh(russsh::ui::Page),
}

#[derive(Clone)]
pub enum Row {
    Quick(String, u16),
    Host(Uuid),
}

pub struct HomeState {
    pub query: String,
    pub filter: Filter,
    pub selected: Option<usize>,
    pub search_input: Entity<InputState>,
    /// Visible rows computed on render, used for keyboard Up/Down/Enter navigation.
    pub visible_rows: Vec<Row>,
    _sub: Subscription,
}

pub struct ComputedRows {
    pub display_hosts: Vec<HostProfile>,
    pub sections: Vec<(Option<String>, Vec<Uuid>)>,
    pub endpoint: Option<(String, u16)>,
}

impl HomeState {
    pub fn new(window: &mut Window, cx: &mut Context<RelayApp>) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search hosts, or type an address to connect")
        });
        search_input.update(cx, |s, cx| s.focus(window, cx));

        let sub = cx.subscribe_in(&search_input, window, |this, input, event, window, cx| {
            match event {
                InputEvent::Change => {
                    this.home.query = input.read(cx).value().to_string();
                    this.home.recompute_rows(&this.profiles, &this.credentials);
                    this.home.selected = (!this.home.query.trim().is_empty()
                        && !this.home.visible_rows.is_empty())
                    .then_some(0);
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    this.home.recompute_rows(&this.profiles, &this.credentials);
                    let picked = this
                        .home
                        .selected
                        .and_then(|s| this.home.visible_rows.get(s))
                        .or(this.home.visible_rows.first())
                        .cloned();
                    match picked {
                        Some(Row::Quick(host, port)) => {
                            this.quick_connect(&host, port, window, cx);
                        }
                        Some(Row::Host(id)) => {
                            this.connect_host_id(id, window, cx);
                        }
                        None => {}
                    }
                }
                _ => {}
            }
        });

        Self {
            query: String::new(),
            filter: Filter::All,
            selected: None,
            search_input,
            visible_rows: Vec::new(),
            _sub: sub,
        }
    }

    pub fn focus_search(&self, window: &mut Window, cx: &mut App) {
        self.search_input.update(cx, |s, cx| s.focus(window, cx));
    }

    pub fn clear_search(&mut self, window: &mut Window, cx: &mut App) {
        self.query.clear();
        self.selected = None;
        self.search_input
            .update(cx, |s, cx| s.set_value("", window, cx));
    }

    pub fn search_focused(&self, window: &Window, cx: &App) -> bool {
        self.search_input.read(cx).focus_handle(cx).is_focused(window)
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.visible_rows.is_empty() {
            self.selected = None;
            return;
        }
        let max = self.visible_rows.len() - 1;
        self.selected = Some(match (self.selected, delta > 0) {
            (None, true) => 0,
            (None, false) => 0,
            (Some(s), true) => (s + 1).min(max),
            (Some(s), false) => s.saturating_sub(1),
        });
    }

    pub fn recompute_rows(
        &mut self,
        profiles: &crate::store::profiles::ProfileStore,
        credentials: &crate::store::credentials::CredentialStore,
    ) -> ComputedRows {
        let query = self.query.trim().to_owned();
        let endpoint = parse_endpoint(&query);
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
            .filter(|h| match &self.filter {
                Filter::All => true,
                Filter::Favorites => h.favorite,
                Filter::Group(g) => h.group.trim() == g,
                Filter::Ssh(_) => true,
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

        let sections = build_sections(&hosts, &self.filter, query.is_empty());
        for (_, ids) in &sections {
            rows.extend(ids.iter().map(|id| Row::Host(*id)));
        }

        if let Some(selected) = self.selected
            && selected >= rows.len()
        {
            self.selected = rows.len().checked_sub(1);
        }

        self.visible_rows = rows;
        ComputedRows {
            display_hosts,
            sections,
            endpoint,
        }
    }
}

const RAIL_WIDTH: f32 = 232.0;
const ROW_HEIGHT: f32 = 52.0;
const TILE_MIN_WIDTH: f32 = 200.0;
const TILE_INFO_HEIGHT: f32 = 76.0;
const TILE_GAP: f32 = 12.0;

pub fn render_home(
    this: &mut RelayApp,
    p: &Palette,
    window: &mut Window,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let ComputedRows {
        display_hosts,
        sections,
        endpoint,
    } = this.home.recompute_rows(&this.profiles, &this.credentials);
    let view = this.settings.settings.host_view;

    let rail = render_rail(this, p, cx);
    let main = match this.home.filter {
        Filter::Ssh(_) => super::ssh::render_page(this, window, cx),
        _ => render_main_column(
            this,
            &display_hosts,
            &sections,
            endpoint,
            view,
            p,
            window,
            cx,
        ),
    };

    let editor = this.editor.as_ref().map(|ed| {
        super::editor::render_editor(ed, &this.profiles, &this.credentials, p, cx)
    });

    div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h_0()
        .w_full()
        .bg(p.canvas)
        .child(rail)
        .child(main)
        .when_some(editor, |el, ed| el.child(ed))
        .into_any_element()
}

fn render_rail(this: &RelayApp, p: &Palette, cx: &mut Context<RelayApp>) -> AnyElement {
    let hosts = this.profiles.hosts();
    let favorites = hosts.iter().filter(|h| h.favorite).count();
    let groups = this.profiles.groups();

    let mut top = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        // Brand row
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .pl(px(6.0))
                .mb(px(22.0))
                .child(widgets::relay_logo(p))
                .child(
                    div()
                        .text_size(size::SUBHEAD)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text)
                        .child(crate::APP_NAME),
                ),
        )
        .child(nav_item(
            "nav-all",
            &this.home.filter,
            Filter::All,
            icons::LIST,
            "All hosts",
            Some(hosts.len()),
            p,
            cx,
        ))
        .child(nav_item(
            "nav-fav",
            &this.home.filter,
            Filter::Favorites,
            icons::STAR,
            "Favorites",
            Some(favorites),
            p,
            cx,
        ));

    if !groups.is_empty() {
        top = top
            .child(
                div()
                    .mt(px(14.0))
                    .mb(px(4.0))
                    .pl(px(10.0))
                    .child(widgets::section_label("Groups", p)),
            )
            .children(groups.into_iter().map(|group| {
                let count = hosts.iter().filter(|h| h.group.trim() == group).count();
                nav_item(
                    SharedString::from(format!("nav-group-{group}")),
                    &this.home.filter,
                    Filter::Group(group.clone()),
                    icons::FOLDER,
                    &group,
                    Some(count),
                    p,
                    cx,
                )
            }));
    }

    top = top
        .child(
            div()
                .mt(px(14.0))
                .mb(px(4.0))
                .pl(px(10.0))
                .child(widgets::section_label("SSH", p)),
        )
        .children(russsh::ui::Page::ALL.into_iter().map(|(page, label, _)| {
            nav_item(
                SharedString::from(format!("nav-ssh-{label}")),
                &this.home.filter,
                Filter::Ssh(page),
                super::ssh::page_icon(page),
                label,
                None,
                p,
                cx,
            )
        }));

    let bottom = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .pl(px(10.0))
                .mb(px(6.0))
                .text_size(size::SMALL)
                .text_color(p.muted)
                .child(version_label()),
        )
        .child(rail_button(
            "rail-new-host",
            icons::PLUS,
            "New host",
            p,
            cx.listener(|this, _, window, cx| {
                this.open_new_host_editor(None, window, cx);
            }),
        ))
        .child(rail_button(
            "rail-passwords",
            icons::KEY,
            "Passwords",
            p,
            cx.listener(|this, _, _, cx| {
                this.dialog = Some(super::dialogs::Dialog::Passwords);
                cx.notify();
            }),
        ))
        .child(rail_button(
            "rail-import",
            icons::FILE_DOWN,
            "Import .rdp file",
            p,
            cx.listener(|this, _, _, _cx| {
                this.picker.rdp_file();
            }),
        ))
        .child(rail_button(
            "rail-settings",
            icons::SETTINGS,
            "Settings",
            p,
            cx.listener(|this, _, _, cx| {
                this.dialog = Some(super::dialogs::Dialog::Settings);
                cx.notify();
            }),
        ));

    div()
        .flex()
        .flex_col()
        .justify_between()
        .w(px(RAIL_WIDTH))
        .h_full()
        .flex_shrink_0()
        .bg(p.canvas)
        .pl(px(14.0))
        .pr(px(10.0))
        .pt(px(18.0))
        .pb(px(16.0))
        .child(
            div()
                .id("rail-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .child(top),
        )
        .child(bottom)
        .into_any_element()
}

fn version_label() -> String {
    format!("Version {}", env!("CARGO_PKG_VERSION"))
}

#[allow(clippy::too_many_arguments)]
fn nav_item(
    id: impl Into<gpui_kit::ElementId>,
    current: &Filter,
    filter: Filter,
    icon_path: &'static str,
    label: &str,
    count: Option<usize>,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let selected = *current == filter;
    let fill = if selected {
        p.accent_soft()
    } else {
        transparent_black()
    };
    let hover = p.hover;
    let fg = if selected { p.accent } else { p.muted };
    let text_color = if selected { p.text } else { p.muted };

    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(34.0))
        .px(px(10.0))
        .gap(px(10.0))
        .rounded(radius::ROW)
        .bg(fill)
        .cursor_pointer()
        .when(!selected, |el| el.hover(move |s| s.bg(hover)))
        .on_click(cx.listener(move |this, _, window, cx| match &filter {
            Filter::Ssh(page) => this.show_ssh_page(*page, window, cx),
            _ => {
                this.home.filter = filter.clone();
                this.home.selected = None;
                cx.notify();
            }
        }))
        .child(icon_svg(icon_path, px(16.0), fg))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(size::BODY)
                .font_weight(FontWeight::MEDIUM)
                .text_color(text_color)
                .child(label.to_owned()),
        )
        .when_some(count, |el, count| {
            el.child(
                div()
                    .text_size(size::SMALL)
                    .text_color(p.muted)
                    .child(count.to_string()),
            )
        })
        .into_any_element()
}

fn rail_button(
    id: impl Into<gpui_kit::ElementId>,
    icon_path: &'static str,
    label: &'static str,
    p: &Palette,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let hover = p.hover;
    let text = p.text;
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(34.0))
        .px(px(10.0))
        .gap(px(10.0))
        .rounded(radius::ROW)
        .text_color(p.muted)
        .cursor_pointer()
        .hover(move |s| s.bg(hover).text_color(text))
        .on_click(on_click)
        .child(icon_svg(icon_path, px(16.0), p.muted))
        .child(
            div()
                .text_size(size::BODY)
                .font_weight(FontWeight::MEDIUM)
                .child(label),
        )
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_main_column(
    this: &mut RelayApp,
    display_hosts: &[HostProfile],
    sections: &[(Option<String>, Vec<Uuid>)],
    endpoint: Option<(String, u16)>,
    view: HostView,
    p: &Palette,
    window: &mut Window,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let title = match &this.home.filter {
        Filter::All => "All hosts".to_owned(),
        Filter::Favorites => "Favorites".to_owned(),
        Filter::Group(g) => g.clone(),
        Filter::Ssh(_) => "SSH".to_owned(),
    };

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .mb(px(14.0))
        .child(
            div()
                .text_size(size::HEADING)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text)
                .child(title),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .children(
                    [
                        (HostView::List, icons::LIST, "List"),
                        (HostView::Tiles, icons::SQUARES_FOUR, "Tiles"),
                    ]
                    .into_iter()
                    .map(|(option, icon_path, label)| {
                        let active = view == option;
                        let hover = p.control_hover;
                        div()
                            .id(SharedString::from(format!("view-toggle-{label}")))
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(32.0))
                            .h(px(28.0))
                            .rounded(radius::CONTROL)
                            .bg(if active { p.accent } else { p.raised })
                            .cursor_pointer()
                            .when(!active, |el| el.hover(move |s| s.bg(hover)))
                            .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.settings.settings.host_view != option {
                                    this.update_settings(cx, |s| s.host_view = option);
                                }
                            }))
                            .child(icon_svg(
                                icon_path,
                                px(16.0),
                                if active { p.on_accent } else { p.muted },
                            ))
                    }),
                ),
        );

    let search_focused = this.home.search_focused(window, cx);
    let search_box = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(46.0))
        .px(px(14.0))
        .gap(px(10.0))
        .mb(px(20.0))
        .rounded(radius::ROW)
        .bg(p.canvas)
        .border_1()
        .border_color(if search_focused { p.accent } else { p.hairline })
        .child(icon_svg(icons::SEARCH, px(18.0), p.muted))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(
                    Input::new(&this.home.search_input)
                        .appearance(false)
                        .bordered(false)
                        .focus_bordered(false),
                ),
        )
        .child(
            div()
                .text_size(size::SMALL)
                .text_color(p.muted)
                .child("Ctrl K"),
        );

    // Compute responsive tile width from viewport minus sidebar, editor, and panel padding.
    let viewport_w = f32::from(window.viewport_size().width);
    let editor_w = if this.editor.is_some() {
        DRAWER_WIDTH
    } else {
        0.0
    };
    let avail_w = (viewport_w - RAIL_WIDTH - editor_w - 64.0 - 12.0).max(TILE_MIN_WIDTH);
    let columns = ((avail_w + TILE_GAP) / (TILE_MIN_WIDTH + TILE_GAP))
        .floor()
        .max(1.0);
    let tile_width = ((avail_w - TILE_GAP * (columns - 1.0)) / columns).floor();

    let query = this.home.query.trim().to_owned();
    let content: AnyElement = if this.profiles.hosts().is_empty() && this.home.visible_rows.is_empty()
    {
        render_empty_state(p, cx)
    } else {
        let mut body = div().flex().flex_col().w_full().pb(px(28.0)).pr(px(8.0));
        let mut index = 0usize;

        if let Some(Row::Quick(host, port)) = this.home.visible_rows.first().cloned() {
            let selected = this.home.selected == Some(0);
            body = body
                .child(render_quick_row(&host, port, selected, p, cx))
                .child(
                    div()
                        .id("quick-save-host")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .mt(px(6.0))
                        .mb(px(18.0))
                        .text_size(size::SMALL)
                        .text_color(p.muted)
                        .cursor_pointer()
                        .hover({
                            let text = p.text;
                            move |s| s.text_color(text)
                        })
                        .on_click(cx.listener({
                            let q = query.clone();
                            move |this, _, window, cx| {
                                this.open_new_host_editor(Some(q.clone()), window, cx);
                            }
                        }))
                        .child(icon_svg(icons::PLUS, px(13.0), p.muted))
                        .child("Save as a host"),
                );
            index = 1;
        }

        if sections.is_empty() && !query.is_empty() && endpoint.is_none() {
            body = body.child(
                div()
                    .text_size(size::BODY)
                    .text_color(p.muted)
                    .child(format!("No hosts match “{query}”.")),
            );
        }

        for (sec_idx, (heading, ids)) in sections.iter().enumerate() {
            if let Some(heading) = heading {
                body = body.child(
                    div()
                        .mb(px(8.0))
                        .child(widgets::section_label(heading.clone(), p)),
                );
            }
            let section_hosts: Vec<&HostProfile> = ids
                .iter()
                .filter_map(|id| display_hosts.iter().find(|h| h.id == *id))
                .collect();

            match view {
                HostView::List => {
                    let mut list = div().flex().flex_col().w_full().mb(px(18.0));
                    for host in section_hosts {
                        let signal = this.prober.signal(host.address.trim(), host.port);
                        let selected = this.home.selected == Some(index);
                        list = list.child(render_host_row(
                            sec_idx, host, signal, selected, p, cx,
                        ));
                        index += 1;
                    }
                    body = body.child(list);
                }
                HostView::Tiles => {
                    let mut grid = div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(TILE_GAP))
                        .w_full()
                        .mb(px(20.0));
                    for host in section_hosts {
                        let signal = this.prober.signal(host.address.trim(), host.port);
                        let selected = this.home.selected == Some(index);
                        let preview = if this.settings.settings.desktop_previews {
                            this.previews.image(host.id)
                        } else {
                            None
                        };
                        grid = grid.child(render_host_tile(
                            sec_idx, host, signal, selected, tile_width, preview, p, cx,
                        ));
                        index += 1;
                    }
                    body = body.child(grid);
                }
            }
        }

        div()
            .id("home-hosts-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .child(body)
            .into_any_element()
    };

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .h_full()
        .bg(p.surface)
        .border_t_1()
        .border_l_1()
        .border_color(p.hairline)
        .rounded_tl(radius::PANEL)
        .pl(px(32.0))
        .pr(px(24.0))
        .pt(px(28.0))
        .child(header)
        .child(search_box)
        .child(content)
        .into_any_element()
}

fn render_quick_row(
    host: &str,
    port: u16,
    selected: bool,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let endpoint = if port == DEFAULT_PORT {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    };
    let h = host.to_owned();
    let hover = p.accent_soft();

    div()
        .id("quick-connect-row")
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(ROW_HEIGHT))
        .px(px(16.0))
        .rounded(radius::ROW)
        .bg(if selected { p.accent_soft() } else { p.canvas })
        .border_1()
        .border_color(p.accent.opacity(0.5))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .on_click(cx.listener(move |this, _, window, cx| {
            this.quick_connect(&h, port, window, cx);
        }))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .child(icon_svg(icons::ARROW_RIGHT, px(18.0), p.accent))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .child(
                            div()
                                .text_size(size::BODY)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(p.text)
                                .child("Connect to"),
                        )
                        .child(
                            div()
                                .text_size(size::BODY)
                                .font_family(FONT_MONO)
                                .text_color(p.text)
                                .child(endpoint),
                        ),
                ),
        )
        .child(
            div()
                .text_size(size::SMALL)
                .text_color(p.muted)
                .child("Enter"),
        )
        .into_any_element()
}

fn render_host_row(
    sec_idx: usize,
    host: &HostProfile,
    signal: Signal,
    selected: bool,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let host_id = host.id;
    let uid = (sec_idx << 16) ^ (host_id.as_u128() as usize & 0xFFFF);
    let group_name = gpui_kit::SharedString::from(format!("row-{uid}"));
    let hover = p.hover;
    let account = host.account_label();
    let (signal_text, signal_color) = match signal {
        Signal::Offline => ("Offline".to_owned(), p.poor),
        Signal::Unknown => (String::new(), p.muted),
        Signal::Rtt(_) => (signal.label(), p.muted),
    };

    div()
        .id(("host-row", uid))
        .group(group_name.clone())
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(ROW_HEIGHT))
        .px(px(12.0))
        .rounded(radius::ROW)
        .bg(if selected {
            p.accent_soft()
        } else {
            transparent_black()
        })
        .border_b_1()
        .border_color(p.hairline.opacity(0.6))
        .hover(move |s| s.bg(hover))
        .on_click(cx.listener(move |this, event: &gpui_kit::ClickEvent, window, cx| {
            if event.click_count() >= 2 {
                this.connect_host_id(host_id, window, cx);
            }
        }))
        // Signal bars
        .child(div().mr(px(14.0)).child(widgets::signal_bars(
            ("row-sig", uid),
            signal,
            p,
        )))
        // Name & account
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .pr(px(12.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(size::BODY)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(p.text)
                                .child(host.display_name().to_owned()),
                        )
                        .when(host.favorite, |el| {
                            el.child(icon_svg(icons::STAR, px(12.0), p.fair))
                        }),
                )
                .when(!account.is_empty(), |el| {
                    el.child(
                        div()
                            .truncate()
                            .text_size(size::SMALL)
                            .text_color(p.muted)
                            .child(account),
                    )
                }),
        )
        // Endpoint
        .child(
            div()
                .w(px(200.0))
                .flex_shrink_0()
                .truncate()
                .font_family(FONT_MONO)
                .text_size(px(13.0))
                .text_color(p.muted)
                .child(host.endpoint()),
        )
        // Latency & last used
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(14.0))
                .mr(px(8.0))
                .child(
                    div()
                        .w(px(56.0))
                        .text_right()
                        .font_family(FONT_MONO)
                        .text_size(size::SMALL)
                        .text_color(signal_color)
                        .child(signal_text),
                )
                .child(
                    div()
                        .w(px(90.0))
                        .text_right()
                        .text_size(size::SMALL)
                        .text_color(p.muted)
                        .child(last_used(host.last_connected)),
                ),
        )
        // Hover actions
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .invisible()
                .group_hover(group_name, |s| s.visible())
                .child(widgets::primary_button(
                    ("row-connect", uid),
                    "Connect",
                    p,
                    false,
                    cx.listener(move |this, _, window, cx| {
                        this.connect_host_id(host_id, window, cx);
                    }),
                ))
                .child(widgets::icon_button(
                    ("row-edit", uid),
                    icons::PENCIL,
                    "Edit",
                    p,
                    cx.listener(move |this, _, window, cx| {
                        this.edit_host_id(host_id, window, cx);
                    }),
                ))
                .child(more_menu(uid, host, p, cx)),
        )
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_host_tile(
    sec_idx: usize,
    host: &HostProfile,
    signal: Signal,
    selected: bool,
    width: f32,
    preview: Option<Arc<RenderImage>>,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let host_id = host.id;
    let uid = (sec_idx << 16) ^ (host_id.as_u128() as usize & 0xFFFF);
    let group_name = gpui_kit::SharedString::from(format!("tile-{uid}"));
    let preview_h = (width * 9.0 / 16.0).round();
    let os = host.os_kind();
    let hover = p.hover;
    let fill = if selected { p.accent_soft() } else { p.raised };

    let has_preview = preview.is_some();
    let preview_area = div()
        .relative()
        .w_full()
        .h(px(preview_h))
        .overflow_hidden()
        .rounded_t(px(11.0))
        .bg(p.canvas)
        .border_b_1()
        .border_color(p.hairline)
        .child(if let Some(image) = preview {
            div()
                .relative()
                .size_full()
                .child(
                    img(image)
                        .w_full()
                        .h_full()
                        .object_fit(ObjectFit::Cover),
                )
                // OS circular badge in bottom-left of preview
                .child(
                    div()
                        .absolute()
                        .left(px(8.0))
                        .bottom(px(8.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(28.0))
                        .rounded_full()
                        .bg(p.raised)
                        .border_1()
                        .border_color(p.hairline)
                        .child(icon_svg(os_icon(os), px(15.0), os_logo_color(os, p))),
                )
                .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .child(icon_svg(os_icon(os), px(40.0), os_logo_color(os, p)))
                .into_any_element()
        })
        // Signal bars pill in top-right of preview
        .child(
            div()
                .absolute()
                .top(px(6.0))
                .right(px(8.0))
                .flex()
                .items_center()
                .justify_center()
                .w(px(30.0))
                .h(px(20.0))
                .rounded(px(10.0))
                .when(has_preview, |el| el.bg(rgba(0x00000096)))
                .child(widgets::signal_bars(("tile-sig", uid), signal, p)),
        );

    let detail = if host.credential_profile_id.is_some() {
        host.account_label()
    } else {
        host.os_label()
            .unwrap_or_else(|| last_used(host.last_connected))
    };

    let info_area = div()
        .relative()
        .flex()
        .flex_col()
        .justify_between()
        .w_full()
        .h(px(TILE_INFO_HEIGHT))
        .px(px(14.0))
        .pt(px(10.0))
        .pb(px(10.0))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .pr(px(4.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(size::BODY)
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(p.text)
                                .child(host.display_name().to_owned()),
                        )
                        .when(host.favorite, |el| {
                            el.child(icon_svg(icons::STAR, px(12.0), p.fair))
                        }),
                )
                .child(
                    div()
                        .truncate()
                        .font_family(FONT_MONO)
                        .text_size(size::SMALL)
                        .text_color(p.muted)
                        .child(host.endpoint()),
                ),
        )
        .child(
            div()
                .truncate()
                .text_size(size::SMALL)
                .text_color(p.muted)
                .child(detail),
        )
        // Hover actions overlay: Edit & More at top-right, Connect at bottom-right
        .child(
            div()
                .absolute()
                .top(px(4.0))
                .right(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .invisible()
                .group_hover(group_name.clone(), |s| s.visible())
                .child(widgets::icon_button(
                    ("tile-edit", uid),
                    icons::PENCIL,
                    "Edit",
                    p,
                    cx.listener(move |this, _, window, cx| {
                        this.edit_host_id(host_id, window, cx);
                    }),
                ))
                .child(more_menu(uid, host, p, cx)),
        )
        .child(
            div()
                .absolute()
                .bottom(px(6.0))
                .right(px(8.0))
                .invisible()
                .group_hover(group_name.clone(), |s| s.visible())
                .child(widgets::primary_button(
                    ("tile-connect", uid),
                    "Connect",
                    p,
                    false,
                    cx.listener(move |this, _, window, cx| {
                        this.connect_host_id(host_id, window, cx);
                    }),
                )),
        );

    div()
        .id(("host-tile", uid))
        .group(group_name)
        .flex()
        .flex_col()
        .w(px(width))
        .h(px(preview_h + TILE_INFO_HEIGHT))
        .rounded(radius::PANEL)
        .bg(fill)
        .border_1()
        .border_color(p.hairline)
        .hover(move |s| s.bg(hover))
        .on_click(cx.listener(move |this, event: &gpui_kit::ClickEvent, window, cx| {
            if event.click_count() >= 2 {
                this.connect_host_id(host_id, window, cx);
            }
        }))
        .child(preview_area)
        .child(info_area)
        .into_any_element()
}

fn more_menu(
    uid: usize,
    host: &HostProfile,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    let host_id = host.id;
    let is_fav = host.favorite;
    let entity = cx.entity().clone();
    let palette = *p;
    let raised = p.raised;
    let surface = p.surface;
    let hairline = p.hairline;
    let muted = p.muted;
    let shadow = p.popup_shadow();

    let trigger = div()
        .flex()
        .items_center()
        .justify_center()
        .size(px(30.0))
        .rounded(radius::CONTROL)
        .cursor_pointer()
        .hover(move |s| s.bg(raised))
        .child(icon_svg(icons::DOTS_THREE, px(16.0), muted));

    Popover::new(("host-more", uid))
        .anchor(Anchor::TopRight)
        .appearance(false)
        .trigger(widgets::PopoverTrigger::new(("host-more-btn", uid), trigger))
        .content(move |_state, _window, cx| {
            let se = cx.entity().clone();
            let e_dup = entity.clone();
            let e_fav = entity.clone();
            let e_del = entity.clone();
            let se1 = se.clone();
            let se2 = se.clone();
            let se3 = se.clone();
            let fav_label = if is_fav {
                "Remove from favorites"
            } else {
                "Add to favorites"
            };

            div()
                .flex()
                .flex_col()
                .min_w(px(180.0))
                .p(px(6.0))
                .rounded(radius::ROW)
                .bg(surface)
                .border_1()
                .border_color(hairline)
                .shadow(shadow.clone())
                .child(widgets::menu_item(
                    ("more-dup", uid),
                    "Duplicate",
                    Some(icons::COPY),
                    false,
                    false,
                    &palette,
                    move |_, window, cx| {
                        e_dup.update(cx, |app, cx| app.duplicate_host(host_id, window, cx));
                        se1.update(cx, |s, cx| s.dismiss(window, cx));
                    },
                ))
                .child(widgets::menu_item(
                    ("more-fav", uid),
                    fav_label,
                    Some(icons::STAR),
                    false,
                    false,
                    &palette,
                    move |_, window, cx| {
                        e_fav.update(cx, |app, cx| {
                            app.profiles.toggle_favorite(host_id);
                            cx.notify();
                        });
                        se2.update(cx, |s, cx| s.dismiss(window, cx));
                    },
                ))
                .child(widgets::menu_separator(&palette))
                .child(widgets::menu_item(
                    ("more-del", uid),
                    "Delete",
                    Some(icons::TRASH),
                    false,
                    true,
                    &palette,
                    move |_, window, cx| {
                        e_del.update(cx, |app, cx| {
                            app.prompt_delete_host(host_id);
                            cx.notify();
                        });
                        se3.update(cx, |s, cx| s.dismiss(window, cx));
                    },
                ))
        })
        .into_any_element()
}

fn render_empty_state(p: &Palette, cx: &mut Context<RelayApp>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_start()
        .pt(px(48.0))
        .gap(px(6.0))
        .child(
            div()
                .text_size(size::SUBHEAD)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text)
                .child("No saved hosts yet"),
        )
        .child(
            div()
                .text_size(size::BODY)
                .text_color(p.muted)
                .mb(px(12.0))
                .child(
                    "Type an address above to connect right away, or save a host to keep its settings and sign-in.",
                ),
        )
        .child(
            div()
                .id("empty-new-host")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .h(px(34.0))
                .px(px(14.0))
                .rounded(radius::CONTROL)
                .bg(p.accent)
                .text_color(p.on_accent)
                .text_size(size::BODY)
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_new_host_editor(None, window, cx);
                }))
                .child(icon_svg(icons::PLUS, px(16.0), p.on_accent))
                .child("New host"),
        )
        .into_any_element()
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
        recent.sort_by_key(|a| std::cmp::Reverse(a.last_connected));
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
    groups.sort_by_key(|a| (a.is_empty(), a.to_lowercase()));
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

fn os_icon(os: OsKind) -> &'static str {
    match os {
        OsKind::Windows => icons::WINDOWS,
        OsKind::Linux => icons::LINUX,
        OsKind::MacOs => icons::APPLE,
        OsKind::Unknown => icons::DESKTOP,
    }
}

fn os_logo_color(os: OsKind, p: &Palette) -> Hsla {
    match (os, p.dark) {
        (OsKind::Windows, true) => rgb(0x40A6FF).into(),
        (OsKind::Windows, false) => rgb(0x0067C0).into(),
        (OsKind::Linux, true) => rgb(0xFACC15).into(),
        (OsKind::Linux, false) => rgb(0xB47800).into(),
        (OsKind::MacOs, true) => rgb(0xC4A7FF).into(),
        (OsKind::MacOs, false) => rgb(0x7C3AED).into(),
        (OsKind::Unknown, _) => p.muted,
    }
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
