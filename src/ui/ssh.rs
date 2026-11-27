//! The SSH section: RusSSH's vault (hosts, keychain, known hosts and terminal settings) and its
//! sessions, embedded in Relay.
//!
//! The vault keeps its own files under `~/.config/relay/ssh` and `~/.local/share/relay/ssh`
//! ([`crate::store::paths::ssh_dirs`]) and its passwords under the [`KEYRING_SERVICE`] keyring
//! service, apart from the standalone RusSSH app's. Relay draws the navigation (the rail's SSH
//! section) and the tabs; RusSSH draws the pages and the sessions.

use std::sync::Arc;

use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, Hsla, InteractiveElement,
    IntoElement, MouseButton, ParentElement, StatefulInteractiveElement, Styled, Subscription,
    Window, div, prelude::FluentBuilder as _, px, transparent_black,
};
use russsh::store::secrets::DesktopKeyring;
use russsh::ui::theme as ssh_theme;
use russsh::ui::{Page, RusSsh, Status, Stores, TabId, VaultEvent};

use super::assets::{icon_svg, icons};
use super::home::Filter;
use super::theme::{self, Palette, radius, size};
use super::{ActiveTab, RelayApp};

/// The keyring service for SSH passwords and key passphrases.
pub const KEYRING_SERVICE: &str = "relay-ssh";

/// App-wide setup, before the window opens: the terminal's and sessions' shortcuts and
/// RusSSH's colors.
pub fn init(cx: &mut App) {
    cx.bind_keys(russsh::terminal::view::key_bindings());
    cx.bind_keys(russsh::ui::session_key_bindings());
    cx.bind_keys(russsh::ui::embedded_key_bindings());
    ssh_theme::set_palettes(palette(&theme::dark()), palette(&theme::light()), cx);
}

/// RusSSH's colors, taken from Relay's palette.
fn palette(p: &Palette) -> ssh_theme::Palette {
    ssh_theme::Palette {
        window: p.canvas,
        sidebar: p.canvas,
        content: p.canvas,
        card: p.surface,
        card_hover: p.raised,
        border: p.hairline,
        text: p.text,
        muted: p.muted,
        accent: p.accent,
        accent_soft: p.accent_soft(),
        danger: p.poor,
        success: p.good,
        warning: p.fair,
    }
}

/// The vault, loaded from disk, and Relay's subscriptions to it.
pub fn new_vault(
    window: &mut Window,
    cx: &mut Context<RelayApp>,
) -> (Entity<RusSsh>, [Subscription; 2]) {
    let secrets = Arc::new(DesktopKeyring::new(KEYRING_SERVICE));
    let vault = cx.new(|cx| RusSsh::embedded(Stores::load(), secrets, window, cx));
    let subscriptions = [
        cx.observe(&vault, |_, _, cx| cx.notify()),
        cx.subscribe_in(&vault, window, |this, _, event: &VaultEvent, window, cx| {
            this.on_ssh_event(*event, window, cx)
        }),
    ];
    (vault, subscriptions)
}

impl RelayApp {
    /// The vault opened a session, or closed the one shown.
    fn on_ssh_event(&mut self, event: VaultEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            VaultEvent::Activated(Some(id)) => self.active = ActiveTab::Ssh(id),
            VaultEvent::Activated(None) => {
                if matches!(self.active, ActiveTab::Ssh(_)) {
                    self.active = ActiveTab::Home;
                    window.focus(&self.focus_handle, cx);
                }
            }
        }
        cx.notify();
    }

    /// Keeps the vault's shown session in step with Relay's active tab, whichever way it
    /// changed.
    pub(super) fn sync_ssh_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = match self.active {
            ActiveTab::Ssh(id) => Some(id),
            _ => None,
        };
        if self.ssh.read(cx).active() != wanted {
            self.ssh
                .update(cx, |vault, cx| vault.activate(wanted, window, cx));
        }
    }

    /// Shows an SSH page from the rail.
    pub(super) fn show_ssh_page(
        &mut self,
        page: Page,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.home.filter = Filter::Ssh(page);
        self.home.selected = None;
        self.active = ActiveTab::Home;
        self.ssh
            .update(cx, |vault, cx| vault.show_vault(page, window, cx));
        cx.notify();
    }

    fn close_ssh_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        self.ssh
            .update(cx, |vault, cx| vault.close_tab(id, window, cx));
        cx.notify();
    }

    /// The title of an SSH session: the remote's window title, or the host's name.
    pub(super) fn ssh_title(&self, id: TabId, cx: &App) -> Option<String> {
        self.ssh
            .read(cx)
            .sessions()
            .map(|view| view.read(cx))
            .find(|view| view.id == id)
            .map(|view| view.title())
    }
}

/// The active SSH session, with the Appearance & session panel when it's open.
pub fn render_session(
    this: &mut RelayApp,
    window: &mut Window,
    cx: &mut Context<RelayApp>,
) -> Option<AnyElement> {
    let session = this
        .ssh
        .update(cx, |vault, cx| vault.render_session(window, cx))?;
    Some(
        div()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(session)
            .into_any_element(),
    )
}

/// The SSH page chosen in the rail.
pub fn render_page(
    this: &mut RelayApp,
    window: &mut Window,
    cx: &mut Context<RelayApp>,
) -> AnyElement {
    this.ssh
        .update(cx, |vault, cx| vault.render_page(window, cx))
}

/// Tab strip chips for the SSH sessions, after the RDP ones: a terminal icon, the status dot,
/// the title and a close button.
pub fn render_chips(this: &RelayApp, p: &Palette, cx: &mut Context<RelayApp>) -> Vec<AnyElement> {
    let hover = p.hover;
    let raised = p.raised;
    let sessions: Vec<_> = this
        .ssh
        .read(cx)
        .sessions()
        .map(|view| {
            let view = view.read(cx);
            (view.id, view.title(), view.status())
        })
        .collect();
    sessions
        .into_iter()
        .map(|(id, title, status)| {
            let selected = this.active == ActiveTab::Ssh(id);
            let unread = status == Status::Unread && !selected;
            let group = gpui_kit::SharedString::from(format!("ssh-chip-{}", id.0));
            div()
                .id(("tab-ssh", id.0))
                .group(group.clone())
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .h(px(32.0))
                .min_w(px(80.0))
                .max_w(px(220.0))
                .pl(px(10.0))
                .pr(px(6.0))
                .rounded(radius::ROW)
                .bg(if selected {
                    p.surface
                } else {
                    transparent_black()
                })
                .border_1()
                .border_color(if selected {
                    p.hairline
                } else {
                    transparent_black()
                })
                .text_size(size::BODY)
                .font_weight(if unread {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::MEDIUM
                })
                .text_color(if selected || unread { p.text } else { p.muted })
                .cursor_pointer()
                .when(!selected, |el| el.hover(move |s| s.bg(hover)))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _, window, cx| this.close_ssh_tab(id, window, cx)),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active = ActiveTab::Ssh(id);
                    cx.notify();
                }))
                .child(icon_svg(
                    icons::TERMINAL,
                    px(14.0),
                    if selected { p.text } else { p.muted },
                ))
                .child(
                    div()
                        .size(px(7.0))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(status_color(status, p)),
                )
                .child(div().flex_1().min_w_0().truncate().child(title))
                .child(
                    div()
                        .id(("tab-ssh-close", id.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(20.0))
                        .flex_shrink_0()
                        .rounded(px(4.0))
                        .when(!selected, |el| {
                            el.invisible().group_hover(group, |s| s.visible())
                        })
                        .hover(move |s| s.bg(raised))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_ssh_tab(id, window, cx);
                        }))
                        .child(icon_svg(icons::X, px(12.0), p.muted)),
                )
                .into_any_element()
        })
        .collect()
}

/// The button at the end of the tab strip that opens the Appearance & session panel, shown
/// while an SSH session is.
pub fn render_panel_toggle(
    this: &RelayApp,
    p: &Palette,
    cx: &mut Context<RelayApp>,
) -> Option<AnyElement> {
    if !matches!(this.active, ActiveTab::Ssh(_)) {
        return None;
    }
    let open = this.ssh.read(cx).session_panel_open();
    let hover = p.hover;
    Some(
        div()
            .id("ssh-panel-toggle")
            .flex()
            .items_center()
            .justify_center()
            .size(px(30.0))
            .flex_shrink_0()
            .rounded(radius::ROW)
            .when(open, |el| el.bg(p.surface))
            .hover(move |s| s.bg(hover))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| {
                this.ssh
                    .update(cx, |vault, cx| vault.toggle_session_panel(window, cx));
            }))
            .child(icon_svg(
                icons::PANEL_RIGHT,
                px(16.0),
                if open { p.text } else { p.muted },
            ))
            .into_any_element(),
    )
}

fn status_color(status: Status, p: &Palette) -> Hsla {
    match status {
        Status::Connecting => p.accent,
        Status::NeedsInput => p.fair,
        Status::Connected | Status::Unread => p.good,
        Status::Ended => p.muted,
    }
}

/// Rail icons for the SSH pages.
pub fn page_icon(page: Page) -> &'static str {
    match page {
        Page::Hosts => icons::SERVER,
        Page::Keychain => icons::KEY_ROUND,
        Page::KnownHosts => icons::SHIELD_CHECK,
        Page::Settings => icons::SETTINGS,
    }
}
