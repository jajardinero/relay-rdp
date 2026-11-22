//! Application UI: GPUI Kit root view, tab strip, host library, sessions, dialogs and input routing.

pub mod assets;
pub mod credentials;
pub mod dialogs;
pub mod editor;
pub mod home;
pub mod previews;
pub mod session_view;
pub mod tab;
pub mod theme;
pub mod toasts;
pub mod widgets;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{
    App, Context, FocusHandle, Focusable as _, FontWeight, Hsla, InteractiveElement, IntoElement,
    KeyDownEvent, KeyUpEvent, Modifiers, ModifiersChangedEvent, MouseButton, ParentElement,
    Pixels, Point, Render, ScrollEvent, StatefulInteractiveElement, Styled, Subscription, Window,
    div, prelude::FluentBuilder as _, px, transparent_black,
};
use ironrdp_input::{Operation, Scancode};
use parking_lot::Mutex;
use uuid::Uuid;
use zeroize::Zeroizing;

use self::assets::{icon_svg, icons};
use self::dialogs::{CredentialsDialog, Dialog};
use self::session_view::ViewportSlot;
use self::tab::{Phase, SessionContext, SessionTab, StageState, TabId};
use self::theme::{Palette, radius, size};
use crate::APP_NAME;
use crate::app::gpu::RemoteTextures;
use crate::app::inhibit::ShortcutInhibitor;
use crate::app::input::{self, RemoteViewport};
use crate::app::keymap::{self, codes};
use crate::app::UiWaker;
use crate::session::{Ended, SessionEvent, Stage};
use crate::store::credentials::{CredentialProfile, CredentialStore};
use crate::store::known_hosts::KnownHosts;
use crate::store::profiles::{HostProfile, ProfileStore, parse_endpoint};
use crate::store::secrets::{SecretId, SecretOperation, SecretWorker};
use crate::store::settings::{AppSettings, SettingsStore, ShortcutCapture, ThemePreference};
use crate::store::{rdpfile, secrets};
use crate::util::pick::{Picked, Picker};
use crate::util::probe::Prober;

pub const TAB_STRIP_HEIGHT: f32 = 44.0;

#[derive(Debug, Clone, Copy)]
pub enum Hotkey {
    ToggleFullscreen,
    ShowSessionBar,
    SendCtrlAltDel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTab {
    Home,
    Session(TabId),
}

/// A connection requested on the command line.
pub struct LaunchRequest {
    pub target: String,
    pub user: Option<String>,
    pub password: Option<Zeroizing<String>>,
}

struct SignIn {
    profile: HostProfile,
    saved: bool,
    tab: Option<TabId>,
    password: Zeroizing<String>,
    remember: bool,
}

enum PendingSecret {
    Connect {
        profile: HostProfile,
        saved: bool,
        tab: Option<TabId>,
    },
    SaveCredential(CredentialProfile),
    DeleteCredential(Uuid),
    UpdatePassword(SignIn),
    HostWrite,
}

pub struct RelayApp {
    pub(crate) gpu: Option<Arc<crate::gpu::GpuContext>>,
    pub(crate) waker: UiWaker,
    pub(crate) settings: SettingsStore,
    pub(crate) profiles: ProfileStore,
    pub(crate) credentials: CredentialStore,
    pub(crate) credential_manager: credentials::Manager,
    pub(crate) known_hosts: Arc<Mutex<KnownHosts>>,
    pub(crate) prober: Prober,
    pub(crate) picker: Picker,
    pub(crate) previews: previews::Previews,
    pub(crate) textures: RemoteTextures,

    pub(crate) tabs: Vec<SessionTab>,
    pub(crate) active: ActiveTab,
    pub(crate) home: home::HomeState,
    pub(crate) editor: Option<editor::EditorState>,
    pub(crate) dialog: Option<Dialog>,
    pub(crate) toasts: toasts::Toasts,
    pub(crate) pending_launch: Option<LaunchRequest>,
    pub(crate) failure_details_open: bool,

    pub(crate) viewport_slot: ViewportSlot,
    pub(crate) pointer_pos: Option<Point<Pixels>>,
    pub(crate) pointer_over_remote: bool,
    pointer_grab: Option<TabId>,
    buttons_down: u8,
    pending_move: Option<(TabId, ironrdp_input::MousePosition)>,
    prev_modifiers: Modifiers,
    prev_capslock: bool,
    bar_menu_open: Option<TabId>,

    focus_handle: FocusHandle,
    inhibitor: Option<ShortcutInhibitor>,
    applied_dark: Option<bool>,
    title: String,
    window_focused: bool,
    focused_session: Option<TabId>,
    frames_shown: u64,

    secret_worker: SecretWorker,
    pending_secrets: HashMap<Uuid, PendingSecret>,
    _window_subs: Vec<Subscription>,
}

impl RelayApp {
    pub fn new(
        launch: Option<LaunchRequest>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (waker, notify) = UiWaker::new();
        let secret_waker = waker.clone();
        let secret_worker = SecretWorker::new(secrets::DesktopKeyring, move || {
            secret_waker.wake();
        });
        let prober = Prober::new(waker.clone());
        let picker = Picker::new(waker.clone());
        let preview_waker = waker.clone();
        let previews = previews::Previews::new(move || {
            preview_waker.wake();
        });

        let gpu = crate::gpu::GpuContext::new_headless();
        let inhibitor = ShortcutInhibitor::new(window);
        let focus_handle = cx.focus_handle();
        let home = home::HomeState::new(window, cx);

        let settings = SettingsStore::load();
        let profiles = ProfileStore::load();
        let credentials = CredentialStore::load();
        let known_hosts = Arc::new(Mutex::new(KnownHosts::load()));

        // Spawn background wakeup loop driven by Notify (RDP frames/events, prober, keyring, picker, previews).
        cx.spawn_in(window, async move |this, cx| {
            loop {
                notify.notified().await;
                let ok = this.update_in(cx, |app, window, cx| {
                    app.poll(window, cx);
                    cx.notify();
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();

        // Periodic tick for smooth touchpad momentum glide, session bar edge dwell/linger,
        // connect track spinner animation, reconnect countdown, and toast expiry.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let ok = this.update_in(cx, |app, window, cx| {
                    if app.tick(window, cx) {
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();

        let mut window_subs = Vec::new();
        window_subs.push(cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            if !active {
                this.release_all_input();
            }
            this.window_focused = active;
            this.sync_session_focus();
            cx.notify();
        }));
        window_subs.push(cx.observe_window_appearance(window, |this, window, cx| {
            this.applied_dark = None;
            this.apply_theme(window, cx);
            cx.notify();
        }));

        let mut app = Self {
            gpu,
            waker,
            settings,
            profiles,
            credentials,
            credential_manager: credentials::Manager::default(),
            known_hosts,
            prober,
            picker,
            previews,
            textures: RemoteTextures::default(),
            tabs: Vec::new(),
            active: ActiveTab::Home,
            home,
            editor: None,
            dialog: None,
            toasts: toasts::Toasts::default(),
            pending_launch: launch,
            failure_details_open: false,
            viewport_slot: ViewportSlot::default(),
            pointer_pos: None,
            pointer_over_remote: false,
            pointer_grab: None,
            buttons_down: 0,
            pending_move: None,
            prev_modifiers: Modifiers::default(),
            prev_capslock: false,
            bar_menu_open: None,
            focus_handle,
            inhibitor,
            applied_dark: None,
            title: String::new(),
            window_focused: true,
            focused_session: None,
            frames_shown: 0,
            secret_worker,
            pending_secrets: HashMap::new(),
            _window_subs: window_subs,
        };
        app.watch_hosts();
        app.apply_theme(window, cx);
        app
    }

    fn is_dark(&self, window: &Window) -> bool {
        match self.settings.settings.theme {
            ThemePreference::Dark => true,
            ThemePreference::Light => false,
            ThemePreference::System => match window.appearance() {
                gpui_kit::WindowAppearance::Light | gpui_kit::WindowAppearance::VibrantLight => {
                    false
                }
                _ => !matches!(dark_light::detect(), Ok(dark_light::Mode::Light)),
            },
        }
    }

    fn palette(&self, window: &Window) -> Palette {
        Palette::for_mode(self.is_dark(window))
    }

    fn apply_theme(&mut self, window: &Window, cx: &mut App) {
        let dark = self.is_dark(window);
        if self.applied_dark != Some(dark) {
            let p = Palette::for_mode(dark);
            theme::apply(cx, &p);
            self.applied_dark = Some(dark);
        }
    }

    pub fn update_settings(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut AppSettings),
    ) {
        let prev_previews = self.settings.settings.desktop_previews;
        f(&mut self.settings.settings);
        if prev_previews && !self.settings.settings.desktop_previews {
            self.previews.clear();
        }
        self.settings.save();
        self.applied_dark = None;
        cx.notify();
    }

    fn watch_hosts(&self) {
        let endpoints = self
            .profiles
            .hosts()
            .iter()
            .map(|h| (h.address.trim().to_owned(), h.port))
            .collect();
        self.prober.watch(endpoints);
    }

    fn tab(&self, id: TabId) -> Option<&SessionTab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    fn tab_mut(&mut self, id: TabId) -> Option<&mut SessionTab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn active_tab_mut(&mut self) -> Option<&mut SessionTab> {
        match self.active {
            ActiveTab::Home => None,
            ActiveTab::Session(id) => self.tab_mut(id),
        }
    }

    // ── Periodic tick & background polling ────────────────────────────────────

    /// Returns true if the UI needs a redraw.
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut dirty = false;
        let now = Instant::now();

        // Flush coalesced mouse moves.
        if let Some((tab_id, position)) = self.pending_move.take()
            && let Some(tab) = self.tab_mut(tab_id)
        {
            tab.send_ops([Operation::MouseMove(position)]);
        }

        // Touchpad momentum glide steps.
        let touchpad = self.touchpad_scrolling();
        for tab in &mut self.tabs {
            if tab.wheel.next_step().is_some_and(|at| at <= now) {
                let ops = tab.wheel.glide(now, touchpad);
                tab.send_ops(ops);
            }
        }

        // Session bar edge dwell & linger timers.
        if let ActiveTab::Session(id) = self.active {
            let area_top = self
                .viewport_slot
                .0
                .borrow()
                .map(|(bounds, _)| f32::from(bounds.origin.y));
            let pointer = self.pointer_pos;
            let menu_open = self.bar_menu_open == Some(id);
            if let Some(tab) = self.tab_mut(id) {
                match &tab.phase {
                    Phase::Connecting => {
                        if tab.stages.iter().any(|s| matches!(s, StageState::Active(_))) {
                            dirty = true;
                        }
                    }
                    Phase::Reconnecting { .. } => {
                        dirty = true;
                    }
                    Phase::Live => {
                        let was_visible = tab.bar.engaged
                            || tab.bar.visible_until.is_some_and(|until| now < until);
                        let near_edge = pointer.zip(area_top).is_some_and(|(pos, top)| {
                            let y = f32::from(pos.y);
                            y >= top - 1.0 && y <= top + session_view::EDGE_ZONE
                        });
                        if near_edge {
                            let since = *tab.bar.edge_since.get_or_insert(now);
                            if now.duration_since(since) >= session_view::EDGE_DWELL {
                                tab.bar.visible_until = Some(now + session_view::BAR_LINGER);
                            }
                        } else {
                            tab.bar.edge_since = None;
                        }
                        if tab.bar.engaged || menu_open {
                            tab.bar.visible_until = Some(now + session_view::BAR_LINGER);
                        }
                        let is_visible = tab.bar.engaged
                            || menu_open
                            || tab.bar.visible_until.is_some_and(|until| now < until);
                        if !is_visible {
                            tab.bar.visible_until = None;
                        }
                        if was_visible != is_visible {
                            dirty = true;
                        }
                    }
                    Phase::Ended(_) => {}
                }
            }
        }

        // Toast pruning.
        let toast_before = self.toasts.prune().is_some();
        let toast_after = self.toasts.prune().is_some();
        if toast_before != toast_after {
            dirty = true;
        }

        // Update Wayland shortcut inhibitor.
        let capture = self.wants_shortcut_capture(window);
        if let Some(inhibitor) = &mut self.inhibitor {
            inhibitor.set(capture);
        }

        // Sync remote desktop frames if any session signaled a frame update.
        let frames: Vec<_> = self
            .tabs
            .iter()
            .map(|t| (t.handle.id, Arc::clone(&t.handle.frame)))
            .collect();
        if !frames.is_empty() {
            self.textures.sync(self.gpu.as_ref(), &frames);
        }

        // Check periodic desktop preview capture.
        if self.settings.settings.desktop_previews {
            for tab in &self.tabs {
                if let (true, true, Some(since)) = (tab.saved, tab.is_live(), tab.live_since) {
                    let gpu = self.gpu.as_ref();
                    self.previews
                        .capture_if_due(tab.profile.id, since, &tab.handle.frame, gpu);
                }
            }
        }

        // Wait a couple of frames so the window has its real size before CLI launch.
        if self.frames_shown > 2
            && let Some(launch) = self.pending_launch.take()
        {
            self.open_launch(launch, window, cx);
            dirty = true;
        }

        dirty
    }

    /// Drains session events, file picker results, and keyring completions.
    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for tab in &mut self.tabs {
            while let Some(event) = tab.handle.try_event() {
                let went_live = matches!(event, SessionEvent::StageCompleted(Stage::Desktop, _));
                if let SessionEvent::HostOs(os) = event
                    && tab.saved
                {
                    self.profiles.set_detected_os(tab.profile.id, os);
                }
                tab.handle_event(event);
                if went_live && tab.saved {
                    self.profiles.mark_connected(tab.profile.id);
                }
            }
        }

        let frames: Vec<_> = self
            .tabs
            .iter()
            .map(|t| (t.handle.id, Arc::clone(&t.handle.frame)))
            .collect();
        self.textures.sync(self.gpu.as_ref(), &frames);

        // Tabs the user disconnected on purpose simply close.
        let closed: Vec<TabId> = self
            .tabs
            .iter()
            .filter(|t| matches!(t.phase, Phase::Ended(Ended::Closed)))
            .map(|t| t.id)
            .collect();
        for id in closed {
            self.close_tab(id, window);
        }

        while let Some(picked) = self.picker.try_recv() {
            match picked {
                Picked::RdpFile(path) => self.import_rdp_file(&path, window, cx),
                Picked::SharedFolder(path) => {
                    if let Some(editor) = &mut self.editor {
                        editor.add_shared_folder(path, window, cx);
                    }
                }
            }
        }

        self.poll_secrets(window, cx);
    }

    // ── Session bar & pointer / keyboard input ────────────────────────────────

    pub fn set_session_bar_hovered(&mut self, id: TabId, hovered: bool) {
        let menu_open = self.bar_menu_open == Some(id);
        if let Some(tab) = self.tab_mut(id) {
            tab.bar.engaged = hovered || menu_open;
            if hovered {
                tab.bar.visible_until = Some(Instant::now() + session_view::BAR_LINGER);
            }
        }
    }

    pub fn set_session_bar_menu_open(&mut self, id: TabId, open: bool) {
        self.bar_menu_open = open.then_some(id);
        if let Some(tab) = self.tab_mut(id) {
            tab.bar.engaged = open;
            if open {
                tab.bar.visible_until = Some(Instant::now() + session_view::BAR_LINGER);
            }
        }
    }

    fn current_viewport(&self) -> Option<RemoteViewport> {
        self.viewport_slot.0.borrow().and_then(|(_, vp)| vp)
    }

    fn pointer_target(&self, tab_id: TabId, pos: Point<Pixels>) -> Option<RemoteViewport> {
        let viewport = self.current_viewport()?;
        if self.pointer_grab == Some(tab_id) {
            return Some(viewport);
        }
        if self.dialog.is_some() || !viewport.contains(pos) {
            return None;
        }
        let tab = self.tab(tab_id)?;
        if !tab.is_live() || tab.cert_prompt.is_some() || tab.bar.engaged {
            return None;
        }
        Some(viewport)
    }

    pub fn on_session_mouse_move(
        &mut self,
        tab_id: TabId,
        pos: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.pointer_pos = Some(pos);
        if let Some(viewport) = self.pointer_target(tab_id, pos) {
            self.pointer_over_remote = true;
            self.pending_move = Some((tab_id, viewport.to_remote(pos)));
            // Repaint when software cursor bitmap is active so the cursor tracks smoothly.
            if let Some(tab) = self.tab(tab_id)
                && matches!(tab.cursor, crate::session::PointerUpdate::Bitmap(_))
            {
                cx.notify();
            }
        } else {
            self.pointer_over_remote = false;
        }
    }

    pub fn on_session_mouse_button(
        &mut self,
        tab_id: TabId,
        button: MouseButton,
        pos: Point<Pixels>,
        pressed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pointer_pos = Some(pos);
        let target = if pressed || self.pointer_grab.is_none() {
            self.pointer_target(tab_id, pos)
        } else if self.pointer_grab == Some(tab_id) {
            self.current_viewport()
        } else {
            None
        };
        let Some(viewport) = target else {
            return;
        };

        if let Some((tid, mpos)) = self.pending_move.take()
            && let Some(tab) = self.tab_mut(tid)
        {
            tab.send_ops([Operation::MouseMove(mpos)]);
        }

        if pressed {
            self.buttons_down = self.buttons_down.saturating_add(1);
            self.pointer_grab = Some(tab_id);
            // Return keyboard focus to the root view so keys go straight to the remote desktop.
            window.focus(&self.focus_handle, cx);
        } else {
            self.buttons_down = self.buttons_down.saturating_sub(1);
            if self.buttons_down == 0 {
                self.pointer_grab = None;
            }
        }

        let Some(rdp_button) = input::rdp_button(button) else {
            return;
        };
        if let Some(tab) = self.tab_mut(tab_id) {
            let op = if pressed {
                tab.wheel.stop_glide();
                Operation::MouseButtonPressed(rdp_button)
            } else {
                Operation::MouseButtonReleased(rdp_button)
            };
            tab.send_ops([Operation::MouseMove(viewport.to_remote(pos)), op]);
        }
    }

    pub fn on_session_scroll(&mut self, tab_id: TabId, event: &ScrollEvent) {
        self.pointer_pos = Some(event.position);
        if self.pointer_target(tab_id, event.position).is_none() {
            return;
        }
        if let Some((tid, mpos)) = self.pending_move.take()
            && let Some(tab) = self.tab_mut(tid)
        {
            tab.send_ops([Operation::MouseMove(mpos)]);
        }
        let touchpad = self.touchpad_scrolling();
        if let Some(tab) = self.tab_mut(tab_id) {
            let ops =
                tab.wheel
                    .operations(event.delta, event.touch_phase, Instant::now(), touchpad);
            tab.send_ops(ops);
        }
    }

    fn touchpad_scrolling(&self) -> input::TouchpadScrolling {
        let s = &self.settings.settings;
        input::TouchpadScrolling {
            speed: f64::from(s.touchpad_scroll_speed),
            momentum: s.touchpad_momentum,
        }
    }

    fn any_input_focused(&self, window: &Window, cx: &App) -> bool {
        if self.home.search_focused(window, cx) {
            return true;
        }
        if let Some(ed) = &self.editor
            && ed.any_input_focused(window, cx)
        {
            return true;
        }
        if let Some(Dialog::Credentials(dlg)) = &self.dialog
            && (dlg.user_input.read(cx).focus_handle(cx).is_focused(window)
                || dlg.domain_input.read(cx).focus_handle(cx).is_focused(window)
                || dlg.password_input.read(cx).focus_handle(cx).is_focused(window))
        {
            return true;
        }
        if let Some(draft) = &self.credential_manager.draft
            && (draft.name_input.read(cx).focus_handle(cx).is_focused(window)
                || draft.user_input.read(cx).focus_handle(cx).is_focused(window)
                || draft.domain_input.read(cx).focus_handle(cx).is_focused(window)
                || draft.password_input.read(cx).focus_handle(cx).is_focused(window))
        {
            return true;
        }
        false
    }

    fn keyboard_session(&mut self, window: &Window, cx: &App) -> Option<&mut SessionTab> {
        if self.dialog.is_some() || self.any_input_focused(window, cx) {
            return None;
        }
        let tab = self.active_tab_mut()?;
        (tab.is_live() && tab.cert_prompt.is_none()).then_some(tab)
    }

    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prev = self.prev_modifiers;
        let next = event.modifiers;
        let prev_caps = self.prev_capslock;
        let next_caps = event.capslock.on;
        self.prev_modifiers = next;
        self.prev_capslock = next_caps;

        let Some(session) = self.keyboard_session(window, cx) else {
            return;
        };
        for (old, new, code) in [
            (prev.control, next.control, codes::CONTROL_LEFT),
            (prev.alt, next.alt, codes::ALT_LEFT),
            (prev.shift, next.shift, codes::SHIFT_LEFT),
            (prev.platform, next.platform, codes::SUPER_LEFT),
        ] {
            if old != new {
                session.key(code, new, false);
            }
        }
        if prev_caps != next_caps {
            session.send_combo(&[codes::CAPS_LOCK]);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;

        // Global Ctrl+K focuses search on Home.
        if self.active == ActiveTab::Home
            && self.dialog.is_none()
            && (mods.control || mods.platform)
            && !mods.alt
            && key.eq_ignore_ascii_case("k")
        {
            self.home.focus_search(window, cx);
            cx.notify();
            return;
        }

        // Escape dismisses open modal/drawer/editor or clears Home search.
        if key == "escape" && !self.is_live_session_focused(window, cx) {
            if let Some(dialog) = &self.dialog {
                let busy = matches!(dialog, Dialog::Credentials(c) if c.busy);
                if !busy {
                    self.dialog = None;
                    cx.notify();
                    return;
                }
            } else if self.editor.is_some() && !self.any_input_focused(window, cx) {
                self.editor = None;
                cx.notify();
                return;
            } else if self.active == ActiveTab::Home && self.home.search_focused(window, cx) {
                self.home.clear_search(window, cx);
                cx.notify();
                return;
            }
        }

        // Home search list Up/Down navigation.
        if self.active == ActiveTab::Home
            && self.dialog.is_none()
            && self.home.search_focused(window, cx)
        {
            if key == "down" {
                self.home.move_selection(1);
                cx.notify();
                return;
            } else if key == "up" {
                self.home.move_selection(-1);
                cx.notify();
                return;
            }
        }

        // Local RDP hotkeys (mstsc conventions: Ctrl+Alt+Enter/Home/End).
        if mods.control && mods.alt && !event.is_held {
            let hotkey = match key {
                "enter" | "return" => Some(Hotkey::ToggleFullscreen),
                "home" => Some(Hotkey::ShowSessionBar),
                "end" => Some(Hotkey::SendCtrlAltDel),
                _ => None,
            };
            if let Some(hotkey) = hotkey {
                self.handle_hotkey(hotkey, window);
                cx.notify();
                return;
            }
        }

        let Some(session) = self.keyboard_session(window, cx) else {
            return;
        };
        if key.eq_ignore_ascii_case("pause") {
            session.send_raw_input(input::pause_sequence());
            return;
        }
        if let Some(scancode) = keymap::scancode(key) {
            session.key(scancode, true, event.is_held);
        }
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.keyboard_session(window, cx) else {
            return;
        };
        if let Some(scancode) = keymap::scancode(event.keystroke.key.as_str()) {
            session.key(scancode, false, false);
        }
    }

    fn is_live_session_focused(&self, window: &Window, cx: &App) -> bool {
        if self.dialog.is_some() || self.any_input_focused(window, cx) {
            return false;
        }
        match self.active {
            ActiveTab::Session(id) => self
                .tab(id)
                .is_some_and(|t| t.is_live() && t.cert_prompt.is_none()),
            ActiveTab::Home => false,
        }
    }

    fn wants_shortcut_capture(&self, window: &Window) -> bool {
        let ActiveTab::Session(id) = self.active else {
            return false;
        };
        let Some(tab) = self.tab(id) else {
            return false;
        };
        let enabled = match self.settings.settings.capture_shortcuts {
            ShortcutCapture::Fullscreen => window.is_fullscreen(),
            ShortcutCapture::Always => true,
            ShortcutCapture::Never => false,
        };
        let bar_showing = tab.bar.visible_until.is_some() || tab.bar.engaged;
        enabled
            && self.window_focused
            && self.dialog.is_none()
            && tab.is_live()
            && tab.cert_prompt.is_none()
            && !bar_showing
    }

    fn release_all_input(&mut self) {
        for tab in &mut self.tabs {
            tab.release_all();
        }
        self.pointer_grab = None;
        self.buttons_down = 0;
    }

    fn handle_hotkey(&mut self, hotkey: Hotkey, window: &mut Window) {
        match hotkey {
            Hotkey::ToggleFullscreen => window.toggle_fullscreen(),
            Hotkey::ShowSessionBar => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.release_all();
                    tab.bar.visible_until = Some(Instant::now() + Duration::from_secs(4));
                }
            }
            Hotkey::SendCtrlAltDel => {
                if let Some(tab) = self.active_tab_mut()
                    && tab.is_live()
                {
                    tab.release_all();
                    tab.send_ctrl_alt_del();
                }
            }
        }
    }

    fn sync_session_focus(&mut self) {
        let wanted = match self.active {
            ActiveTab::Session(id) if self.window_focused => Some(id),
            _ => None,
        };
        if wanted == self.focused_session {
            return;
        }
        if let Some(old) = self.focused_session.and_then(|id| self.tab(id)) {
            old.handle.send(crate::session::SessionCmd::Focus(false));
        }
        if let Some(new) = wanted.and_then(|id| self.tab(id)) {
            new.handle.send(crate::session::SessionCmd::Focus(true));
        }
        self.focused_session = wanted;
    }

    // ── Host, editor, dialog, credential & session actions ────────────────────

    fn session_area_px(&self, window: &Window) -> (f32, f32) {
        let scale = window.scale_factor();
        if let Some((bounds, _)) = *self.viewport_slot.0.borrow() {
            return (
                f32::from(bounds.size.width) * scale,
                f32::from(bounds.size.height) * scale,
            );
        }
        let vp = window.viewport_size();
        let h = if window.is_fullscreen() {
            f32::from(vp.height)
        } else {
            (f32::from(vp.height) - TAB_STRIP_HEIGHT).max(200.0)
        };
        (f32::from(vp.width) * scale, h * scale)
    }

    pub fn connect_host_id(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(profile) = self.profiles.get(id).cloned() {
            self.request_connect(profile, true, None, window, cx);
        }
    }

    pub fn quick_connect(
        &mut self,
        host: &str,
        port: u16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (profile, saved) = self.profile_for(host, port);
        self.request_connect(profile, saved, None, window, cx);
    }

    fn profile_for(&self, host: &str, port: u16) -> (HostProfile, bool) {
        self.profiles
            .hosts()
            .iter()
            .find(|h| h.address.eq_ignore_ascii_case(host) && h.port == port)
            .map_or_else(
                || (HostProfile::new(host, port), false),
                |p| (p.clone(), true),
            )
    }

    pub fn open_new_host_editor(
        &mut self,
        prefill: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut profile = HostProfile::new("", 3389);
        if let Some((host, port)) = prefill.as_deref().and_then(parse_endpoint) {
            profile.address = host.clone();
            profile.name = host;
            profile.port = port;
        } else {
            profile.name.clear();
        }
        self.editor = Some(editor::EditorState::new(profile, window, cx));
        cx.notify();
    }

    pub fn edit_host_id(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(profile) = self.profiles.get(id).cloned() {
            self.editor = Some(editor::EditorState::edit(profile, window, cx));
            cx.notify();
        }
    }

    pub fn duplicate_host(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(mut profile) = self.profiles.get(id).cloned() {
            profile.id = Uuid::new_v4();
            profile.name = format!("{} copy", profile.display_name());
            profile.save_password = false;
            profile.last_connected = None;
            self.editor = Some(editor::EditorState::new(profile, window, cx));
            cx.notify();
        }
    }

    pub fn prompt_delete_host(&mut self, id: Uuid) {
        if let Some(profile) = self.profiles.get(id) {
            self.dialog = Some(Dialog::ConfirmDelete {
                id,
                name: profile.display_name().to_owned(),
            });
        }
    }

    pub fn delete_host(&mut self, id: Uuid) {
        self.dialog = None;
        if let Some(removed) = self.profiles.remove(id) {
            secrets::delete_password_in_background(removed.id);
            self.previews.remove(removed.id);
            self.watch_hosts();
            self.toasts
                .info(format!("Deleted {}", removed.display_name()));
        }
        if self.editor.as_ref().is_some_and(|e| e.profile_id() == id) {
            self.editor = None;
        }
    }

    pub fn save_editor(&mut self, connect: bool, window: &mut Window, cx: &mut Context<Self>) {
        let validated = self.editor.as_mut().and_then(|ed| ed.validate(cx));
        let Some((profile, password)) = validated else {
            cx.notify();
            return;
        };
        match password {
            editor::PasswordChange::Keep => {}
            editor::PasswordChange::Set(password) => {
                let request = self
                    .secret_worker
                    .submit(SecretOperation::Save(SecretId::Host(profile.id), password));
                self.pending_secrets
                    .insert(request, PendingSecret::HostWrite);
            }
            editor::PasswordChange::Forget => secrets::delete_password_in_background(profile.id),
        }
        self.profiles.upsert(profile.clone());
        self.watch_hosts();
        self.editor = None;
        self.toasts
            .info(format!("Saved {}", profile.display_name()));
        if connect {
            self.request_connect(profile, true, None, window, cx);
        }
        cx.notify();
    }

    fn request_connect(
        &mut self,
        profile: HostProfile,
        saved: bool,
        tab: Option<TabId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (snapshot, target) = match self.credentials.connection(&profile, saved) {
            Ok(connection) => connection,
            Err(error) => {
                self.dialog = Some(Dialog::Credentials(Box::new(CredentialsDialog::new(
                    profile,
                    saved,
                    tab,
                    Some(error.to_string()),
                    window,
                    cx,
                ))));
                cx.notify();
                return;
            }
        };
        if let Some(target) = target {
            let request = self.secret_worker.submit(SecretOperation::Load(target));
            self.pending_secrets.insert(
                request,
                PendingSecret::Connect {
                    profile: snapshot,
                    saved,
                    tab,
                },
            );
        } else {
            self.dialog = Some(Dialog::Credentials(Box::new(CredentialsDialog::new(
                profile, saved, tab, None, window, cx,
            ))));
            cx.notify();
        }
    }

    pub fn submit_credentials_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::Credentials(dlg)) = &self.dialog else {
            return;
        };
        if dlg.busy || !dlg.can_connect(&self.credentials) {
            return;
        }
        let profile = dlg.profile.clone();
        let saved = dlg.saved;
        let tab = dlg.tab;
        let password = dlg.password_value(cx);
        let remember = dlg.remember;
        let update_profile_password = dlg.update_profile_password;
        let use_stored_password = dlg.use_stored_password && password.is_empty();

        if use_stored_password && profile.credential_profile_id.is_some() {
            if saved && let Some(mut stored) = self.profiles.get(profile.id).cloned() {
                stored.credential_profile_id = profile.credential_profile_id;
                self.profiles.upsert(stored);
            }
            self.dialog = None;
            self.request_connect(profile, saved, tab, window, cx);
        } else if update_profile_password && let Some(id) = profile.credential_profile_id {
            let request = self.secret_worker.submit(SecretOperation::Save(
                SecretId::Credential(id),
                password.clone(),
            ));
            self.pending_secrets.insert(
                request,
                PendingSecret::UpdatePassword(SignIn {
                    profile,
                    saved,
                    tab,
                    password,
                    remember,
                }),
            );
            if let Some(Dialog::Credentials(state)) = &mut self.dialog {
                state.set_busy(true);
            }
        } else {
            self.finish_sign_in(
                SignIn {
                    profile,
                    saved,
                    tab,
                    password,
                    remember,
                },
                window,
                cx,
            );
        }
        cx.notify();
    }

    pub fn handle_credential_action(&mut self, action: credentials::Action) {
        if self.credential_manager.busy {
            return;
        }
        let (operation, pending) = match action {
            credentials::Action::Save {
                mut profile,
                password,
            } => {
                if let Err(error) = profile.validate() {
                    self.credential_manager.finish(Err(error));
                    return;
                }
                let operation = password.map_or(SecretOperation::Keep, |password| {
                    SecretOperation::Save(SecretId::Credential(profile.id), password)
                });
                (operation, PendingSecret::SaveCredential(profile))
            }
            credentials::Action::Delete(id) => (
                SecretOperation::Delete(SecretId::Credential(id)),
                PendingSecret::DeleteCredential(id),
            ),
        };
        self.credential_manager.busy = true;
        self.credential_manager.error = None;
        let request = self.secret_worker.submit(operation);
        self.pending_secrets.insert(request, pending);
    }

    fn poll_secrets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Some(completion) = self.secret_worker.try_recv() {
            let Some(pending) = self.pending_secrets.remove(&completion.request) else {
                continue;
            };
            match pending {
                PendingSecret::Connect {
                    profile,
                    saved,
                    tab,
                } => match completion.result {
                    Ok(Some(password)) => match self.credentials.resolve(&profile) {
                        Ok(snapshot) => self.launch(snapshot, saved, password, tab, window, cx),
                        Err(error) => {
                            self.dialog = Some(Dialog::Credentials(Box::new(CredentialsDialog::new(
                                profile,
                                saved,
                                tab,
                                Some(error.to_string()),
                                window,
                                cx,
                            ))));
                        }
                    },
                    result => {
                        let reason = match result {
                            Err(error) => format!(
                                "Couldn't read the saved password: {error}. Enter a password to continue."
                            ),
                            _ => "No password is saved for this account. Enter one to continue."
                                .to_owned(),
                        };
                        self.dialog = Some(Dialog::Credentials(Box::new(CredentialsDialog::new(
                            profile,
                            saved,
                            tab,
                            Some(reason),
                            window,
                            cx,
                        ))));
                    }
                },
                PendingSecret::SaveCredential(profile) => {
                    let result = completion
                        .result
                        .and_then(|_| self.credentials.upsert(profile));
                    if result.is_ok() {
                        self.toasts.info("Saved credential profile");
                    } else if let Err(error) = &result {
                        self.toasts
                            .error(format!("Couldn't save credential profile: {error}"));
                    }
                    self.credential_manager.finish(result);
                }
                PendingSecret::DeleteCredential(id) => {
                    let result = completion.result.and_then(|_| {
                        self.profiles.unassign_credentials(id)?;
                        self.credentials.remove(id)
                    });
                    if result.is_ok() {
                        if let Some(editor) = &mut self.editor {
                            editor.unassign_credentials(id, window, cx);
                        }
                        self.toasts.info("Deleted credential profile");
                    } else if let Err(error) = &result {
                        self.toasts
                            .error(format!("Couldn't delete credential profile: {error}"));
                    }
                    self.credential_manager.finish(result);
                }
                PendingSecret::UpdatePassword(sign_in) => match completion.result {
                    Ok(_) => {
                        self.toasts.info("Updated profile password");
                        self.finish_sign_in(sign_in, window, cx);
                    }
                    Err(error) => {
                        let mut dialog = CredentialsDialog::new(
                            sign_in.profile,
                            sign_in.saved,
                            sign_in.tab,
                            None,
                            window,
                            cx,
                        );
                        dialog.password_save_failed(
                            sign_in.password,
                            format!(
                                "Couldn't update the profile password: {error}. Retry or uncheck Update profile password to use it once."
                            ),
                            window,
                            cx,
                        );
                        self.dialog = Some(Dialog::Credentials(Box::new(dialog)));
                    }
                },
                PendingSecret::HostWrite => {
                    if let Err(error) = completion.result {
                        self.toasts
                            .error(format!("Couldn't save the host password: {error}"));
                    }
                }
            }
        }
    }

    fn finish_sign_in(&mut self, sign_in: SignIn, window: &mut Window, cx: &mut Context<Self>) {
        let SignIn {
            profile,
            saved,
            tab,
            password,
            remember,
        } = sign_in;
        let snapshot = match self.credentials.resolve(&profile) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.dialog = Some(Dialog::Credentials(Box::new(CredentialsDialog::new(
                    profile,
                    saved,
                    tab,
                    Some(error.to_string()),
                    window,
                    cx,
                ))));
                return;
            }
        };
        if saved {
            let mut stored = self
                .profiles
                .get(profile.id)
                .cloned()
                .unwrap_or_else(|| profile.clone());
            remember_assignment(&mut stored, &profile, remember);
            if profile.credential_profile_id.is_none() && remember {
                let request = self.secret_worker.submit(SecretOperation::Save(
                    SecretId::Host(profile.id),
                    password.clone(),
                ));
                self.pending_secrets
                    .insert(request, PendingSecret::HostWrite);
            }
            self.profiles.upsert(stored);
        }
        self.dialog = None;
        self.launch(snapshot, saved, password, tab, window, cx);
    }

    fn launch(
        &mut self,
        profile: HostProfile,
        saved: bool,
        password: Zeroizing<String>,
        tab: Option<TabId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let desktop = SessionTab::desired_desktop(
            &profile,
            self.session_area_px(window),
            window.scale_factor(),
        );
        let start_fullscreen = profile.display.start_fullscreen;
        let session_cx = SessionContext {
            hardware_video_decoding: self.settings.settings.hardware_video_decoding,
            gpu: self.gpu.clone(),
            waker: &self.waker,
            known_hosts: &self.known_hosts,
        };

        let result = match tab.and_then(|id| self.tabs.iter_mut().find(|t| t.id == id)) {
            Some(existing) => {
                existing.profile = profile;
                existing.saved = saved;
                existing
                    .restart(Some(password), desktop, &session_cx)
                    .map(|()| existing.id)
            }
            None => {
                SessionTab::start(profile, saved, password, desktop, &session_cx).map(|new_tab| {
                    let id = new_tab.id;
                    self.tabs.push(new_tab);
                    id
                })
            }
        };

        match result {
            Ok(id) => {
                self.active = ActiveTab::Session(id);
                self.failure_details_open = false;
                window.focus(&self.focus_handle, cx);
                if start_fullscreen && !window.is_fullscreen() {
                    window.toggle_fullscreen();
                }
            }
            Err(e) => self
                .toasts
                .error(format!("Couldn't start the connection: {e}")),
        }
    }

    fn open_launch(&mut self, launch: LaunchRequest, window: &mut Window, cx: &mut Context<Self>) {
        let target = launch.target.trim();
        let path = std::path::Path::new(target);
        let (mut profile, saved) = if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("rdp"))
        {
            match rdpfile::import_file(path) {
                Ok(imported) => {
                    self.warn_unsupported(&imported.unsupported);
                    (imported.profile, false)
                }
                Err(e) => {
                    self.toasts
                        .error(format!("Couldn't open {}: {e}", path.display()));
                    return;
                }
            }
        } else {
            let Some((host, port)) = parse_endpoint(target) else {
                self.toasts
                    .error(format!("“{target}” isn't a valid address."));
                return;
            };
            self.profile_for(&host, port)
        };
        let explicit_user = launch.user.is_some();
        match self
            .credentials
            .for_launch(&profile, launch.user, launch.password.is_some())
        {
            Ok(snapshot) => profile = snapshot,
            Err(error) => {
                self.dialog = Some(Dialog::Credentials(Box::new(CredentialsDialog::new(
                    profile,
                    saved,
                    None,
                    Some(error.to_string()),
                    window,
                    cx,
                ))));
                return;
            }
        }
        match launch.password {
            Some(password) if !profile.username.is_empty() => {
                self.launch(profile, saved, password, None, window, cx)
            }
            _ => self.request_connect(profile, saved && !explicit_user, None, window, cx),
        }
    }

    fn import_rdp_file(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match rdpfile::import_file(path) {
            Ok(imported) => {
                let name = imported.profile.display_name().to_owned();
                self.warn_unsupported(&imported.unsupported);
                self.editor = Some(editor::EditorState::new(imported.profile, window, cx));
                self.active = ActiveTab::Home;
                self.toasts
                    .info(format!("Imported {name}. Review the settings, then save."));
            }
            Err(e) => self
                .toasts
                .error(format!("Couldn't import {}: {e}", path.display())),
        }
    }

    fn warn_unsupported(&mut self, features: &[&str]) {
        if !features.is_empty() {
            self.toasts.error(format!(
                "Not supported yet, so ignored: {}",
                features.join(", ")
            ));
        }
    }

    fn capture_preview(&mut self, id: TabId) {
        if !self.settings.settings.desktop_previews {
            return;
        }
        if let Some(tab) = self.tabs.iter().find(|t| t.id == id)
            && tab.saved
            && tab.is_live()
        {
            let gpu = self.gpu.as_ref();
            self.previews
                .capture(tab.profile.id, &tab.handle.frame, gpu);
        }
    }

    pub fn disconnect_tab(&mut self, id: TabId) {
        self.capture_preview(id);
        if let Some(tab) = self.tab_mut(id) {
            tab.disconnect();
        }
    }

    pub fn minimize_tab(&mut self, id: TabId) {
        self.capture_preview(id);
        self.active = ActiveTab::Home;
    }

    pub fn close_tab(&mut self, id: TabId, window: &mut Window) {
        self.capture_preview(id);
        if let Some(index) = self.tabs.iter().position(|t| t.id == id) {
            let mut tab = self.tabs.remove(index);
            tab.disconnect();
            if self.active == ActiveTab::Session(id) {
                self.active = match self.tabs.get(index.saturating_sub(1)).or(self.tabs.first()) {
                    Some(t) => ActiveTab::Session(t.id),
                    None => ActiveTab::Home,
                };
            }
            if self.pointer_grab == Some(id) {
                self.pointer_grab = None;
                self.buttons_down = 0;
            }
            if self.bar_menu_open == Some(id) {
                self.bar_menu_open = None;
            }
        }
        if self.tabs.is_empty() && window.is_fullscreen() {
            window.toggle_fullscreen();
        }
    }

    pub fn reconnect_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tab(id) {
            let profile = self
                .profiles
                .get(tab.profile.id)
                .filter(|_| tab.saved)
                .unwrap_or(&tab.profile)
                .clone();
            if profile.credential_profile_id.is_some()
                || tab.profile.credential_profile_id.is_some()
            {
                let saved = tab.saved;
                self.request_connect(profile, saved, Some(id), window, cx);
                return;
            }
        }
        let area = self.session_area_px(window);
        let scale = window.scale_factor();
        let session_cx = SessionContext {
            hardware_video_decoding: self.settings.settings.hardware_video_decoding,
            gpu: self.gpu.clone(),
            waker: &self.waker,
            known_hosts: &self.known_hosts,
        };
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            let desktop = SessionTab::desired_desktop(&tab.profile, area, scale);
            if let Err(e) = tab.restart(None, desktop, &session_cx) {
                self.toasts.error(format!("Couldn't reconnect: {e}"));
            }
        }
        cx.notify();
    }

    pub fn retry_tab_now(&self, id: TabId) {
        if let Some(tab) = self.tab(id) {
            tab.retry_now();
        }
    }

    pub fn change_tab_credentials(
        &mut self,
        id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tab(id) {
            let failure = match &tab.phase {
                Phase::Ended(Ended::Failed(f)) => Some(f.title.clone()),
                _ => None,
            };
            let profile = self
                .profiles
                .get(tab.profile.id)
                .filter(|_| tab.saved)
                .unwrap_or(&tab.profile)
                .clone();
            let saved = tab.saved;
            self.dialog = Some(Dialog::Credentials(Box::new(CredentialsDialog::new(
                profile,
                saved,
                Some(id),
                failure,
                window,
                cx,
            ))));
            cx.notify();
        }
    }

    pub fn edit_tab_host(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tab(id)
            && tab.saved
            && let Some(profile) = self.profiles.get(tab.profile.id).cloned()
        {
            self.editor = Some(editor::EditorState::edit(profile, window, cx));
            self.active = ActiveTab::Home;
            cx.notify();
        }
    }

    pub fn answer_tab_certificate(&mut self, id: TabId, trust: bool) {
        if let Some(tab) = self.tab_mut(id) {
            tab.answer_certificate(trust);
        }
    }

    pub fn send_tab_ctrl_alt_del(&mut self, id: TabId) {
        if let Some(tab) = self.tab_mut(id) {
            tab.send_ctrl_alt_del();
        }
    }

    pub fn send_tab_keys(&mut self, id: TabId, keys: &[Scancode]) {
        if let Some(tab) = self.tab_mut(id) {
            tab.send_combo(keys);
        }
    }

    // ── Top tab strip rendering ───────────────────────────────────────────────

    fn render_tab_strip(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let home_selected = self.active == ActiveTab::Home;
        let hover = p.hover;
        let raised = p.raised;

        let home_chip = div()
            .id("tab-home")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .h(px(32.0))
            .px(px(12.0))
            .rounded(radius::ROW)
            .bg(if home_selected {
                p.surface
            } else {
                transparent_black()
            })
            .border_1()
            .border_color(if home_selected {
                p.hairline
            } else {
                transparent_black()
            })
            .text_size(size::BODY)
            .font_weight(FontWeight::MEDIUM)
            .text_color(if home_selected { p.text } else { p.muted })
            .cursor_pointer()
            .when(!home_selected, |el| el.hover(move |s| s.bg(hover)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.active = ActiveTab::Home;
                cx.notify();
            }))
            .child(icon_svg(
                icons::SQUARES_FOUR,
                px(15.0),
                if home_selected { p.text } else { p.muted },
            ))
            .child("Hosts");

        let session_chips = self.tabs.iter().map(|tab| {
            let tab_id = tab.id;
            let selected = self.active == ActiveTab::Session(tab_id);
            let status = tab_status_color(tab, p);
            let title = tab.title().to_owned();
            let grp = gpui_kit::SharedString::from(format!("tab-chip-{}", tab_id.raw()));

            div()
                .id(("tab-session", tab_id.raw()))
                .group(grp.clone())
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .h(px(32.0))
                .min_w(px(80.0))
                .max_w(px(220.0))
                .pl(px(12.0))
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
                .font_weight(FontWeight::MEDIUM)
                .text_color(if selected { p.text } else { p.muted })
                .cursor_pointer()
                .when(!selected, |el| el.hover(move |s| s.bg(hover)))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _, window, cx| {
                        this.close_tab(tab_id, window);
                        cx.notify();
                    }),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.active = ActiveTab::Session(tab_id);
                    window.focus(&this.focus_handle, cx);
                    cx.notify();
                }))
                // Status dot
                .child(div().size(px(7.0)).flex_shrink_0().rounded_full().bg(status))
                // Title
                .child(div().flex_1().min_w_0().truncate().child(title))
                // Close button
                .child(
                    div()
                        .id(("tab-close", tab_id.raw()))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(20.0))
                        .flex_shrink_0()
                        .rounded(px(4.0))
                        .when(!selected, |el| {
                            el.invisible().group_hover(grp, |s| s.visible())
                        })
                        .hover(move |s| s.bg(raised))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_tab(tab_id, window);
                            cx.notify();
                        }))
                        .child(icon_svg(icons::X, px(12.0), p.muted)),
                )
        });

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .w_full()
            .h(px(TAB_STRIP_HEIGHT))
            .flex_shrink_0()
            .px(px(10.0))
            .bg(p.canvas)
            .child(home_chip)
            .children(session_chips)
    }
}

impl Render for RelayApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frames_shown += 1;
        self.textures.flush_dropped(window);
        self.apply_theme(window, cx);
        self.sync_session_focus();

        let p = self.palette(window);

        // Keep window title synced with active tab.
        let title = match self.active {
            ActiveTab::Session(id) => self.tab(id).map_or_else(
                || APP_NAME.to_owned(),
                |t| format!("{} — {APP_NAME}", t.title()),
            ),
            ActiveTab::Home => APP_NAME.to_owned(),
        };
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }

        // Dynamic resize for active live session.
        if let ActiveTab::Session(id) = self.active {
            let area_px = self.session_area_px(window);
            let scale = window.scale_factor();
            if let Some(tab) = self.tab_mut(id)
                && tab.is_live()
            {
                let desired = SessionTab::desired_desktop(&tab.profile, area_px, scale);
                tab.follow_size(desired);
            }
        }

        let in_session = matches!(self.active, ActiveTab::Session(_));
        let show_strip = !(window.is_fullscreen() && in_session);

        let main_view = match self.active {
            ActiveTab::Home => home::render_home(self, &p, window, cx),
            ActiveTab::Session(id) => session_view::render_session(self, id, &p, window, cx),
        };

        let dialog_layer = dialogs::render_dialog(self, &p, cx);
        let toast_layer = self.toasts.render(&p);

        div()
            .id("relay-root")
            .track_focus(&self.focus_handle)
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(p.canvas)
            .text_color(p.text)
            .font_family(theme::FONT_SANS)
            .text_size(size::BODY)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .when(show_strip, |el| el.child(self.render_tab_strip(&p, cx)))
            .child(main_view)
            .when_some(dialog_layer, |el, dlg| el.child(dlg))
            .when_some(toast_layer, |el, toast| el.child(toast))
    }
}

impl Drop for RelayApp {
    fn drop(&mut self) {
        for tab in &mut self.tabs {
            tab.disconnect();
        }
    }
}

/// Remember the link, never the shared account's resolved username or password.
fn remember_assignment(stored: &mut HostProfile, selected: &HostProfile, remember_password: bool) {
    stored.credential_profile_id = selected.credential_profile_id;
    if selected.credential_profile_id.is_none() {
        stored.username.clone_from(&selected.username);
        stored.domain.clone_from(&selected.domain);
        stored.save_password = remember_password;
    }
}

fn tab_status_color(tab: &SessionTab, palette: &Palette) -> Hsla {
    match &tab.phase {
        Phase::Live => widgets::Signal::Rtt(tab.current_rtt().unwrap_or_default()).color(palette),
        Phase::Connecting => palette.accent,
        Phase::Reconnecting { .. } => palette.fair,
        Phase::Ended(Ended::Failed(_)) => palette.poor,
        Phase::Ended(_) => palette.muted,
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn remembering_a_shared_assignment_preserves_manual_credentials() {
        let mut stored = HostProfile::new("host", 3389);
        stored.username = "local".into();
        stored.domain = "LOCAL".into();
        let mut selected = stored.clone();
        let id = Uuid::new_v4();
        selected.credential_profile_id = Some(id);
        selected.username = "shared".into();
        selected.domain = "CORP".into();
        remember_assignment(&mut stored, &selected, true);
        assert_eq!(stored.credential_profile_id, Some(id));
        assert_eq!(stored.username, "local");
        assert_eq!(stored.domain, "LOCAL");
        assert!(!stored.save_password);
        selected.credential_profile_id = None;
        remember_assignment(&mut stored, &selected, true);
        assert_eq!(stored.credential_profile_id, None);
        assert_eq!(stored.username, "shared");
        assert!(stored.save_password);
    }
}
