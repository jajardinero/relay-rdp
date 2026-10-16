//! Application UI: tab strip, host library, sessions, dialogs.

pub mod dialogs;
pub mod editor;
pub mod home;
pub mod session_view;
pub mod tab;
pub mod theme;
pub mod toasts;
pub mod widgets;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use egui::{Color32, CornerRadius, Sense, Stroke, vec2};
use egui_phosphor::regular as icon;
use egui_wgpu::wgpu::FilterMode;
use ironrdp_input::Operation;
use parking_lot::Mutex;
use uuid::Uuid;
use winit::event::{MouseButton, MouseScrollDelta, TouchPhase};
use winit::event_loop::EventLoopProxy;
use zeroize::Zeroizing;

use self::dialogs::{CredentialsDialog, Dialog};
use self::tab::{Phase, SessionContext, SessionTab, TabId};
use self::theme::{Palette, size};
use crate::APP_NAME;
use crate::app::UserEvent;
use crate::app::gpu::RemoteTextures;
use crate::app::input::{self, RemoteViewport};
use crate::session::{Ended, PointerUpdate, SessionEvent, SessionId, SharedFrame, Stage};
use crate::store::known_hosts::KnownHosts;
use crate::store::profiles::{HostProfile, ProfileStore, parse_endpoint};
use crate::store::settings::{AppSettings, SettingsStore, ShortcutCapture, ThemePreference};
use crate::store::{rdpfile, secrets};
use crate::util::pick::{Picked, Picker};
use crate::util::probe::Prober;

pub const TAB_STRIP_HEIGHT: f32 = 44.0;

pub enum WindowRequest {
    SetFullscreen(bool),
    SetTitle(String),
    #[allow(dead_code)]
    Quit,
}

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

/// Result of a background keyring lookup.
struct PasswordLookup {
    profile: HostProfile,
    saved: bool,
    tab: Option<TabId>,
    password: Option<Zeroizing<String>>,
}

pub struct Ui {
    pub(crate) gpu: Option<Arc<crate::gpu::GpuContext>>,
    proxy: EventLoopProxy<UserEvent>,
    settings: SettingsStore,
    profiles: ProfileStore,
    known_hosts: Arc<Mutex<KnownHosts>>,
    prober: Prober,
    picker: Picker,

    tabs: Vec<SessionTab>,
    active: ActiveTab,
    home: home::HomeState,
    editor: Option<editor::EditorState>,
    dialog: Option<Dialog>,
    toasts: toasts::Toasts,
    requests: Vec<WindowRequest>,
    pending_launch: Option<LaunchRequest>,

    fullscreen: bool,
    applied_theme: Option<(bool, bool)>,
    title: String,
    pixels_per_point: f32,
    /// The area available to a session last frame, in points.
    session_area: Option<egui::Rect>,
    viewport: Option<(TabId, RemoteViewport)>,
    filters: HashMap<SessionId, FilterMode>,

    pointer_pos: Option<egui::Pos2>,
    pointer_over_remote: bool,
    pointer_grab: Option<TabId>,
    buttons_down: u8,
    pending_move: Option<(TabId, ironrdp_input::MousePosition)>,
    surrender_focus: bool,

    password_tx: std::sync::mpsc::Sender<PasswordLookup>,
    password_rx: std::sync::mpsc::Receiver<PasswordLookup>,
    /// The egui context, kept for work triggered outside a frame (keyring results).
    last_ctx: Option<egui::Context>,
    window_focused: bool,
    /// The session told it has focus (clipboard sync follows it).
    focused_session: Option<TabId>,
    frames_shown: u64,
}

impl Ui {
    pub fn new(
        ctx: &egui::Context,
        proxy: EventLoopProxy<UserEvent>,
        launch: Option<LaunchRequest>,
    ) -> Self {
        theme::install_fonts(ctx);
        let (password_tx, password_rx) = std::sync::mpsc::channel();
        let prober = Prober::new(proxy.clone());
        let picker = Picker::new(proxy.clone());
        let ui = Self {
            gpu: None,
            settings: SettingsStore::load(),
            profiles: ProfileStore::load(),
            known_hosts: Arc::new(Mutex::new(KnownHosts::load())),
            prober,
            picker,
            proxy,
            tabs: Vec::new(),
            active: ActiveTab::Home,
            home: home::HomeState::default(),
            editor: None,
            dialog: None,
            toasts: toasts::Toasts::default(),
            requests: Vec::new(),
            pending_launch: launch,
            fullscreen: false,
            applied_theme: None,
            title: String::new(),
            pixels_per_point: 1.0,
            session_area: None,
            viewport: None,
            filters: HashMap::new(),
            pointer_pos: None,
            pointer_over_remote: false,
            pointer_grab: None,
            buttons_down: 0,
            pending_move: None,
            surrender_focus: false,
            password_tx,
            password_rx,
            last_ctx: None,
            window_focused: true,
            focused_session: None,
            frames_shown: 0,
        };
        ui.watch_hosts();
        ui
    }

    pub fn settings(&self) -> &AppSettings {
        &self.settings.settings
    }

    fn palette(&self) -> Palette {
        let dark = match self.settings.settings.theme {
            ThemePreference::Dark => true,
            ThemePreference::Light => false,
            ThemePreference::System => !matches!(dark_light::detect(), Ok(dark_light::Mode::Light)),
        };
        if dark { theme::DARK } else { theme::LIGHT }
    }

    pub fn clear_color(&self) -> [f32; 4] {
        let c = self.palette().canvas;
        [
            f32::from(c.r()) / 255.0,
            f32::from(c.g()) / 255.0,
            f32::from(c.b()) / 255.0,
            1.0,
        ]
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        let palette = self.palette();
        let key = (palette.dark, self.settings.settings.reduce_motion);
        if self.applied_theme != Some(key) {
            theme::apply(ctx, &palette, key.1);
            self.applied_theme = Some(key);
        }
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

    // ── Per-frame plumbing used by the app shell ──────────────────────────────

    /// Drains session events and background results.
    pub fn poll(&mut self) {
        for tab in &mut self.tabs {
            while let Some(event) = tab.handle.try_event() {
                let went_live = matches!(event, SessionEvent::StageCompleted(Stage::Desktop, _));
                tab.handle_event(event);
                if went_live && tab.saved {
                    self.profiles.mark_connected(tab.profile.id);
                }
            }
        }

        // Tabs the user disconnected on purpose simply close.
        let closed: Vec<TabId> = self
            .tabs
            .iter()
            .filter(|t| matches!(t.phase, Phase::Ended(Ended::Closed)))
            .map(|t| t.id)
            .collect();
        for id in closed {
            self.close_tab(id);
        }

        while let Some(picked) = self.picker.try_recv() {
            match picked {
                Picked::RdpFile(path) => self.import_rdp_file(&path),
                Picked::SharedFolder(path) => {
                    if let Some(editor) = &mut self.editor {
                        editor.add_shared_folder(path);
                    }
                }
            }
        }

        while let Ok(lookup) = self.password_rx.try_recv() {
            match lookup.password {
                Some(password) if !lookup.profile.username.is_empty() => {
                    self.launch(lookup.profile, lookup.saved, password, lookup.tab);
                }
                _ => {
                    self.dialog = Some(Dialog::Credentials(CredentialsDialog::new(
                        lookup.profile,
                        lookup.saved,
                        lookup.tab,
                        None,
                    )));
                }
            }
        }
    }

    pub fn frames(&self) -> Vec<(SessionId, Arc<SharedFrame>)> {
        self.tabs
            .iter()
            .map(|t| (t.handle.id, Arc::clone(&t.handle.frame)))
            .collect()
    }

    pub fn texture_filters(&self) -> &HashMap<SessionId, FilterMode> {
        &self.filters
    }

    pub fn set_fullscreen_state(&mut self, fullscreen: bool) {
        self.fullscreen = fullscreen;
    }

    pub fn take_window_requests(&mut self) -> Vec<WindowRequest> {
        std::mem::take(&mut self.requests)
    }

    pub fn shutdown(&mut self) {
        for tab in &mut self.tabs {
            tab.disconnect();
        }
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

    // ── Input routing ─────────────────────────────────────────────────────────

    /// The session that should receive keyboard input, if any.
    pub fn keyboard_session(&mut self, ctx: &egui::Context) -> Option<&mut SessionTab> {
        if self.dialog.is_some() || ctx.egui_wants_keyboard_input() {
            return None;
        }
        let tab = self.active_tab_mut()?;
        (tab.is_live() && tab.cert_prompt.is_none()).then_some(tab)
    }

    fn pointer_target(
        &self,
        ctx: &egui::Context,
        pos: egui::Pos2,
    ) -> Option<(TabId, RemoteViewport)> {
        let (tab_id, viewport) = self.viewport?;
        if self.pointer_grab == Some(tab_id) {
            return Some((tab_id, viewport));
        }
        if self.dialog.is_some() || !viewport.contains(pos) {
            return None;
        }
        let tab = self.tab(tab_id)?;
        if !tab.is_live() || tab.cert_prompt.is_some() {
            return None;
        }
        // Floating UI (session bar, menus, toasts) sits on higher layers and keeps the pointer.
        if ctx
            .layer_id_at(pos)
            .is_some_and(|layer| layer.order != egui::Order::Background)
        {
            return None;
        }
        Some((tab_id, viewport))
    }

    /// Returns true when the move went to a remote desktop.
    pub fn pointer_moved(&mut self, ctx: &egui::Context, pos: egui::Pos2) -> bool {
        self.pointer_pos = Some(pos);
        match self.pointer_target(ctx, pos) {
            Some((tab_id, viewport)) => {
                self.pointer_over_remote = true;
                self.pending_move = Some((tab_id, viewport.to_remote(pos)));
                true
            }
            None => {
                self.pointer_over_remote = false;
                false
            }
        }
    }

    pub fn pointer_left(&mut self) {
        self.pointer_pos = None;
        self.pointer_over_remote = false;
        self.pending_move = None;
    }

    pub fn flush_pointer(&mut self) {
        if let Some((tab_id, position)) = self.pending_move.take()
            && let Some(tab) = self.tab_mut(tab_id)
        {
            tab.send_ops([Operation::MouseMove(position)]);
        }
    }

    /// Returns true when the click went to a remote desktop.
    pub fn pointer_button(
        &mut self,
        ctx: &egui::Context,
        button: MouseButton,
        pressed: bool,
    ) -> bool {
        let Some(pos) = self.pointer_pos else {
            return false;
        };
        let target = if pressed || self.pointer_grab.is_none() {
            self.pointer_target(ctx, pos)
        } else {
            self.viewport
                .filter(|(id, _)| Some(*id) == self.pointer_grab)
        };
        let Some((tab_id, viewport)) = target else {
            return false;
        };

        self.flush_pointer();
        if pressed {
            self.buttons_down = self.buttons_down.saturating_add(1);
            self.pointer_grab = Some(tab_id);
            self.surrender_focus = true;
        } else {
            self.buttons_down = self.buttons_down.saturating_sub(1);
            if self.buttons_down == 0 {
                self.pointer_grab = None;
            }
        }

        let Some(rdp_button) = input::rdp_button(button) else {
            return true;
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
        true
    }

    pub fn pointer_wheel(
        &mut self,
        ctx: &egui::Context,
        delta: MouseScrollDelta,
        phase: TouchPhase,
    ) -> bool {
        let Some(pos) = self.pointer_pos else {
            return false;
        };
        let Some((tab_id, _)) = self.pointer_target(ctx, pos) else {
            return false;
        };
        self.flush_pointer();
        let touchpad = self.touchpad_scrolling();
        if let Some(tab) = self.tab_mut(tab_id) {
            let ops = tab.wheel.operations(delta, phase, Instant::now(), touchpad);
            tab.send_ops(ops);
        }
        true
    }

    fn touchpad_scrolling(&self) -> input::TouchpadScrolling {
        let settings = &self.settings.settings;
        input::TouchpadScrolling {
            speed: f64::from(settings.touchpad_scroll_speed),
            momentum: settings.touchpad_momentum,
        }
    }

    /// Continues touchpad scrolling that is gliding after the fingers lifted.
    pub fn glide(&mut self, now: Instant) {
        let touchpad = self.touchpad_scrolling();
        for tab in &mut self.tabs {
            if tab.wheel.next_step().is_some_and(|at| at <= now) {
                let ops = tab.wheel.glide(now, touchpad);
                tab.send_ops(ops);
            }
        }
    }

    /// When the next glide step is due.
    pub fn next_glide_step(&self) -> Option<Instant> {
        self.tabs
            .iter()
            .filter_map(|tab| tab.wheel.next_step())
            .min()
    }

    /// While the pointer is over a remote desktop, UI repaints on mouse moves are only needed
    /// near the top edge, where the session bar lives.
    pub fn wants_pointer_repaints(&self) -> bool {
        let (Some(pos), Some((tab_id, viewport))) = (self.pointer_pos, self.viewport) else {
            return true;
        };
        let bar_active = self.tab(tab_id).is_some_and(|t| {
            t.bar.engaged || t.bar.edge_since.is_some() || t.bar.visible_until.is_some()
        });
        bar_active || pos.y <= viewport.rect.top() + session_view::EDGE_ZONE
    }

    /// Whether system shortcuts should go to the remote computer right now. Never while the
    /// session bar shows, so Ctrl+Alt+Home always gives the desktop its shortcuts back.
    pub fn wants_shortcut_capture(&self) -> bool {
        let ActiveTab::Session(id) = self.active else {
            return false;
        };
        let Some(tab) = self.tab(id) else {
            return false;
        };
        let enabled = match self.settings.settings.capture_shortcuts {
            ShortcutCapture::Fullscreen => self.fullscreen,
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

    pub fn remote_cursor(&self) -> Option<PointerUpdate> {
        if !self.pointer_over_remote && self.pointer_grab.is_none() {
            return None;
        }
        let (tab_id, _) = self.viewport?;
        self.tab(tab_id).map(|t| t.cursor.clone())
    }

    pub fn release_all_input(&mut self) {
        for tab in &mut self.tabs {
            tab.release_all();
        }
        self.pointer_grab = None;
        self.buttons_down = 0;
    }

    pub fn hotkey(&mut self, hotkey: Hotkey) {
        match hotkey {
            Hotkey::ToggleFullscreen => self
                .requests
                .push(WindowRequest::SetFullscreen(!self.fullscreen)),
            Hotkey::ShowSessionBar => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.release_all();
                    tab.bar.visible_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
                }
            }
            Hotkey::SendCtrlAltDel => {
                if let Some(tab) = self.active_tab_mut()
                    && tab.is_live()
                {
                    // The local Ctrl+Alt are physically down; release them first so the
                    // server sees a clean Ctrl+Alt+Del.
                    tab.release_all();
                    tab.send_ctrl_alt_del();
                }
            }
        }
    }

    // ── Connecting ────────────────────────────────────────────────────────────

    /// Physical pixel size available for a session's desktop.
    fn session_area_px(&self, ctx: &egui::Context) -> egui::Vec2 {
        let area = self.session_area.unwrap_or_else(|| {
            let mut r = ctx.content_rect();
            if !self.fullscreen {
                r.min.y += TAB_STRIP_HEIGHT;
            }
            r
        });
        area.size() * self.pixels_per_point
    }

    /// Connect to a profile: use the keyring password if one is saved, otherwise ask.
    fn request_connect(&mut self, profile: HostProfile, saved: bool, tab: Option<TabId>) {
        if saved && profile.save_password && !profile.username.is_empty() {
            let tx = self.password_tx.clone();
            let proxy = self.proxy.clone();
            std::thread::spawn(move || {
                let password = secrets::load_password(profile.id).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "keyring lookup failed");
                    None
                });
                let _ = tx.send(PasswordLookup {
                    profile,
                    saved,
                    tab,
                    password,
                });
                let _ = proxy.send_event(UserEvent::Wake);
            });
        } else {
            self.dialog = Some(Dialog::Credentials(CredentialsDialog::new(
                profile, saved, tab, None,
            )));
        }
    }

    fn launch(
        &mut self,
        profile: HostProfile,
        saved: bool,
        password: Zeroizing<String>,
        tab: Option<TabId>,
    ) {
        let Some(ctx) = self.ctx_for_launch() else {
            return;
        };
        let desktop = SessionTab::desired_desktop(
            &profile,
            self.session_area_px(&ctx),
            self.pixels_per_point,
        );
        let start_fullscreen = profile.display.start_fullscreen;
        let cx = SessionContext {
            hardware_video_decoding: self.settings.settings.hardware_video_decoding,
            gpu: self.gpu.clone(),
            proxy: &self.proxy,
            known_hosts: &self.known_hosts,
        };

        let result = match tab.and_then(|id| self.tabs.iter_mut().find(|t| t.id == id)) {
            Some(existing) => {
                existing.profile = profile;
                existing.saved = saved;
                existing
                    .restart(Some(password), desktop, &cx)
                    .map(|()| existing.id)
            }
            None => SessionTab::start(profile, saved, password, desktop, &cx).map(|new_tab| {
                let id = new_tab.id;
                self.tabs.push(new_tab);
                id
            }),
        };

        match result {
            Ok(id) => {
                self.active = ActiveTab::Session(id);
                if start_fullscreen && !self.fullscreen {
                    self.requests.push(WindowRequest::SetFullscreen(true));
                }
            }
            Err(e) => self
                .toasts
                .error(format!("Couldn't start the connection: {e}")),
        }
    }

    /// `launch` is called from both frame code and `poll`; it only needs the context for sizes.
    fn ctx_for_launch(&self) -> Option<egui::Context> {
        self.last_ctx.clone()
    }

    fn open_launch(&mut self, launch: LaunchRequest) {
        let target = launch.target.trim();
        let path = std::path::Path::new(target);
        let (mut profile, saved) = if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("rdp"))
        {
            // Like `mstsc file.rdp`: connect with the file's settings, without saving a host.
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
        if let Some(user) = launch.user {
            profile.username = user;
        }
        match launch.password {
            Some(password) if !profile.username.is_empty() => {
                self.launch(profile, saved, password, None)
            }
            _ => self.request_connect(profile, saved, None),
        }
    }

    /// Loads a `.rdp` file into the host editor, so it can be reviewed and saved.
    fn import_rdp_file(&mut self, path: &std::path::Path) {
        match rdpfile::import_file(path) {
            Ok(imported) => {
                let name = imported.profile.display_name().to_owned();
                self.warn_unsupported(&imported.unsupported);
                self.editor = Some(editor::EditorState::new(imported.profile));
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

    /// The saved host with this address (so its settings and password apply), or a new one.
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

    fn quick_connect(&mut self, host: &str, port: u16) {
        let (profile, saved) = self.profile_for(host, port);
        self.request_connect(profile, saved, None);
    }

    fn close_tab(&mut self, id: TabId) {
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
        }
        if self.tabs.is_empty() && self.fullscreen {
            self.requests.push(WindowRequest::SetFullscreen(false));
        }
    }

    // ── Frame ─────────────────────────────────────────────────────────────────

    pub fn show(&mut self, root: &mut egui::Ui, textures: &RemoteTextures) {
        let ctx = &root.ctx().clone();
        self.last_ctx = Some(ctx.clone());
        self.pixels_per_point = ctx.pixels_per_point();
        self.apply_theme(ctx);
        let palette = Palette::of_ctx(ctx);

        if self.surrender_focus {
            self.surrender_focus = false;
            if let Some(id) = ctx.memory(|m| m.focused()) {
                ctx.memory_mut(|m| m.surrender_focus(id));
            }
        }

        // Wait a couple of frames so the window has its real size before we pick a desktop
        // size; otherwise the server would be asked to resize right after connecting.
        self.frames_shown += 1;
        if self.frames_shown > 2
            && let Some(launch) = self.pending_launch.take()
        {
            self.open_launch(launch);
        }

        let title = match self.active {
            ActiveTab::Session(id) => self.tab(id).map_or_else(
                || APP_NAME.to_owned(),
                |t| format!("{} — {APP_NAME}", t.title()),
            ),
            ActiveTab::Home => APP_NAME.to_owned(),
        };
        if title != self.title {
            self.requests.push(WindowRequest::SetTitle(title.clone()));
            self.title = title;
        }

        let in_session = matches!(self.active, ActiveTab::Session(_));
        let show_strip = !(self.fullscreen && in_session);

        if show_strip {
            egui::Panel::top("tab-strip")
                .exact_size(TAB_STRIP_HEIGHT)
                .show_separator_line(false)
                .frame(
                    egui::Frame::new()
                        .fill(palette.canvas)
                        .inner_margin(egui::Margin::symmetric(10, 6)),
                )
                .show(root, |ui| self.tab_strip(ui, &palette));
        }

        self.viewport = None;
        self.filters.clear();

        if self.active == ActiveTab::Home
            && let Some(state) = &mut self.editor
        {
            let actions = editor::show(root, state, &self.profiles);
            for action in actions {
                self.handle_editor_action(action);
            }
        }

        egui::CentralPanel::no_frame().show(root, |ui| {
            self.session_area = Some(ui.max_rect());
            match self.active {
                ActiveTab::Home => self.home_screen(ui),
                ActiveTab::Session(id) => self.session_screen(ui, id, textures),
            }
        });

        self.show_dialog(ctx);
        self.toasts.show(ctx);
        self.sync_session_focus();
    }

    pub fn set_window_focus(&mut self, focused: bool) {
        self.window_focused = focused;
        self.sync_session_focus();
    }

    /// Tells sessions when they gain or lose focus; the focused one keeps its clipboard in sync.
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

    fn tab_strip(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;

            let home_selected = self.active == ActiveTab::Home;
            if tab_chip(
                ui,
                palette,
                home_selected,
                None,
                &format!("{}  Hosts", icon::SQUARES_FOUR),
                false,
            )
            .clicked
            {
                self.active = ActiveTab::Home;
            }

            let mut close = None;
            let mut select = None;
            for tab in &self.tabs {
                let selected = self.active == ActiveTab::Session(tab.id);
                let status = tab_status_color(tab, palette);
                let chip = tab_chip(ui, palette, selected, Some(status), tab.title(), true);
                if chip.clicked {
                    select = Some(tab.id);
                }
                if chip.close_clicked {
                    close = Some(tab.id);
                }
            }
            if let Some(id) = select {
                self.active = ActiveTab::Session(id);
            }
            if let Some(id) = close {
                self.close_tab(id);
            }
        });
    }

    fn home_screen(&mut self, ui: &mut egui::Ui) {
        let actions = home::show(ui, &mut self.home, &self.profiles, &self.prober);
        for action in actions {
            self.handle_home_action(action);
        }
    }

    fn session_screen(&mut self, ui: &mut egui::Ui, id: TabId, textures: &RemoteTextures) {
        let ppp = self.pixels_per_point;
        let fullscreen = self.fullscreen;
        let others: Vec<(TabId, String)> = self
            .tabs
            .iter()
            .map(|t| (t.id, t.title().to_owned()))
            .collect();
        let area_px = ui.max_rect().size() * ppp;
        let pointer = self.pointer_pos;

        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
            self.active = ActiveTab::Home;
            return;
        };

        if tab.is_live() {
            let desired = SessionTab::desired_desktop(&tab.profile, area_px, ppp);
            tab.follow_size(desired);
        }

        let out = session_view::show(
            ui,
            tab,
            textures,
            session_view::ViewContext {
                pixels_per_point: ppp,
                fullscreen,
                pointer,
                tabs: &others,
            },
        );

        if let Some(viewport) = out.viewport {
            self.viewport = Some((id, viewport));
        }
        if let Some(filter) = out.filter {
            self.filters.insert(tab.handle.id, filter);
        }
        for action in out.actions {
            self.handle_session_action(id, action);
        }
    }

    fn handle_session_action(&mut self, id: TabId, action: session_view::Action) {
        use session_view::Action;
        match action {
            Action::ToggleFullscreen => self
                .requests
                .push(WindowRequest::SetFullscreen(!self.fullscreen)),
            Action::Disconnect => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.disconnect();
                }
            }
            Action::CloseTab => self.close_tab(id),
            Action::Reconnect => {
                let Some(ctx) = self.last_ctx.clone() else {
                    return;
                };
                let area = self.session_area_px(&ctx);
                let ppp = self.pixels_per_point;
                let cx = SessionContext {
                    hardware_video_decoding: self.settings.settings.hardware_video_decoding,
                    gpu: self.gpu.clone(),
                    proxy: &self.proxy,
                    known_hosts: &self.known_hosts,
                };
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                    let desktop = SessionTab::desired_desktop(&tab.profile, area, ppp);
                    if let Err(e) = tab.restart(None, desktop, &cx) {
                        self.toasts.error(format!("Couldn't reconnect: {e}"));
                    }
                }
            }
            Action::RetryNow => {
                if let Some(tab) = self.tab(id) {
                    tab.retry_now();
                }
            }
            Action::ChangeCredentials => {
                if let Some(tab) = self.tab(id) {
                    let failure = match &tab.phase {
                        Phase::Ended(Ended::Failed(f)) => Some(f.title.clone()),
                        _ => None,
                    };
                    self.dialog = Some(Dialog::Credentials(CredentialsDialog::new(
                        tab.profile.clone(),
                        tab.saved,
                        Some(id),
                        failure,
                    )));
                }
            }
            Action::EditHost => {
                if let Some(tab) = self.tab(id)
                    && tab.saved
                    && let Some(profile) = self.profiles.get(tab.profile.id).cloned()
                {
                    self.editor = Some(editor::EditorState::edit(profile));
                    self.active = ActiveTab::Home;
                }
            }
            Action::TrustCertificate(trust) => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.answer_certificate(trust);
                }
            }
            Action::SendCtrlAltDel => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.send_ctrl_alt_del();
                }
            }
            Action::SendKeys(keys) => {
                if let Some(tab) = self.tab_mut(id) {
                    tab.send_combo(&keys);
                }
            }
            Action::SwitchTo(other) => self.active = ActiveTab::Session(other),
            Action::GoHome => self.active = ActiveTab::Home,
        }
    }

    fn handle_home_action(&mut self, action: home::Action) {
        use home::Action;
        match action {
            Action::Connect(id) => {
                if let Some(profile) = self.profiles.get(id).cloned() {
                    self.request_connect(profile, true, None);
                }
            }
            Action::QuickConnect(host, port) => self.quick_connect(&host, port),
            Action::New(prefill) => {
                let mut profile = HostProfile::new("", 3389);
                if let Some((host, port)) = prefill.as_deref().and_then(parse_endpoint) {
                    profile.address = host.clone();
                    profile.name = host;
                    profile.port = port;
                } else {
                    profile.name.clear();
                }
                self.editor = Some(editor::EditorState::new(profile));
            }
            Action::Edit(id) => {
                if let Some(profile) = self.profiles.get(id).cloned() {
                    self.editor = Some(editor::EditorState::edit(profile));
                }
            }
            Action::Duplicate(id) => {
                if let Some(mut profile) = self.profiles.get(id).cloned() {
                    profile.id = Uuid::new_v4();
                    profile.name = format!("{} copy", profile.display_name());
                    profile.save_password = false;
                    profile.last_connected = None;
                    self.editor = Some(editor::EditorState::new(profile));
                }
            }
            Action::ToggleFavorite(id) => self.profiles.toggle_favorite(id),
            Action::Delete(id) => {
                if let Some(profile) = self.profiles.get(id) {
                    self.dialog = Some(Dialog::ConfirmDelete {
                        id,
                        name: profile.display_name().to_owned(),
                    });
                }
            }
            Action::OpenSettings => self.dialog = Some(Dialog::Settings),
            Action::ImportRdpFile => self.picker.rdp_file(),
        }
    }

    fn handle_editor_action(&mut self, action: editor::Action) {
        match action {
            editor::Action::Cancel => self.editor = None,
            editor::Action::PickFolder => self.picker.shared_folder(),
            editor::Action::Save {
                profile,
                password,
                connect,
            } => {
                match password {
                    editor::PasswordChange::Keep => {}
                    editor::PasswordChange::Set(password) => {
                        secrets::save_password_in_background(profile.id, password);
                    }
                    editor::PasswordChange::Forget => {
                        secrets::delete_password_in_background(profile.id)
                    }
                }
                self.profiles.upsert(profile.clone());
                self.watch_hosts();
                self.editor = None;
                self.toasts
                    .info(format!("Saved {}", profile.display_name()));
                if connect {
                    self.request_connect(profile, true, None);
                }
            }
        }
    }

    fn show_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        let outcome = dialogs::show(ctx, dialog, &mut self.settings.settings);
        match outcome {
            dialogs::Outcome::Open => {}
            dialogs::Outcome::Close => self.dialog = None,
            dialogs::Outcome::SettingsChanged => {
                self.settings.save();
                self.applied_theme = None;
            }
            dialogs::Outcome::Connect {
                profile,
                saved,
                tab,
                password,
                remember,
            } => {
                self.dialog = None;
                if saved {
                    let mut stored = self
                        .profiles
                        .get(profile.id)
                        .cloned()
                        .unwrap_or_else(|| profile.clone());
                    stored.username = profile.username.clone();
                    stored.domain = profile.domain.clone();
                    if remember {
                        stored.save_password = true;
                        secrets::save_password_in_background(profile.id, password.clone());
                    }
                    self.profiles.upsert(stored);
                }
                self.launch(profile, saved, password, tab);
            }
            dialogs::Outcome::Delete(id) => {
                self.dialog = None;
                if let Some(removed) = self.profiles.remove(id) {
                    secrets::delete_password_in_background(removed.id);
                    self.watch_hosts();
                    self.toasts
                        .info(format!("Deleted {}", removed.display_name()));
                }
                if self.editor.as_ref().is_some_and(|e| e.profile_id() == id) {
                    self.editor = None;
                }
            }
        }
    }
}

struct ChipResponse {
    clicked: bool,
    close_clicked: bool,
}

/// A tab in the tab strip: status dot, title, and a close button on session tabs.
fn tab_chip(
    ui: &mut egui::Ui,
    palette: &Palette,
    selected: bool,
    status: Option<Color32>,
    title: &str,
    closable: bool,
) -> ChipResponse {
    let font = theme::font_medium(size::BODY);
    let galley = ui
        .painter()
        .layout_no_wrap(title.to_owned(), font, palette.text);
    let dot = if status.is_some() { 14.0 } else { 0.0 };
    let close = if closable { 22.0 } else { 0.0 };
    let width = (galley.size().x + dot + close + 24.0).clamp(80.0, 220.0);
    let (rect, response) = ui.allocate_exact_size(vec2(width, 32.0), Sense::click());

    let hovered = response.hovered();
    let fill = if selected {
        palette.surface
    } else if hovered {
        palette.hover()
    } else {
        Color32::TRANSPARENT
    };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(theme::radius::ROW), fill);
    if selected {
        painter.rect_stroke(
            rect,
            CornerRadius::same(theme::radius::ROW),
            Stroke::new(1.0, palette.hairline),
            egui::StrokeKind::Inside,
        );
    }

    let mut x = rect.left() + 12.0;
    if let Some(color) = status {
        painter.circle_filled(egui::pos2(x + 3.0, rect.center().y), 3.5, color);
        x += dot;
    }
    let text_color = if selected || hovered {
        palette.text
    } else {
        palette.muted
    };
    let max_text = rect.right() - close - 10.0 - x;
    painter
        .with_clip_rect(egui::Rect::from_min_max(
            egui::pos2(x, rect.top()),
            egui::pos2(x + max_text, rect.bottom()),
        ))
        .galley_with_override_text_color(
            egui::pos2(x, rect.center().y - galley.size().y / 2.0),
            galley,
            text_color,
        );

    let mut close_clicked = false;
    if closable {
        let close_rect = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 16.0, rect.center().y),
            vec2(20.0, 20.0),
        );
        let close_response = ui.interact(close_rect, response.id.with("close"), Sense::click());
        if hovered || selected || close_response.hovered() {
            if close_response.hovered() {
                painter.rect_filled(close_rect, CornerRadius::same(4), palette.raised);
            }
            painter.text(
                close_rect.center(),
                egui::Align2::CENTER_CENTER,
                icon::X,
                theme::font(12.0),
                palette.muted,
            );
        }
        close_clicked = close_response.clicked();
        // Middle-click closes, like browser tabs.
        close_clicked |= response.middle_clicked();
    }

    ChipResponse {
        clicked: response.clicked() && !close_clicked,
        close_clicked,
    }
}

fn tab_status_color(tab: &SessionTab, palette: &Palette) -> Color32 {
    match &tab.phase {
        Phase::Live => widgets::Signal::Rtt(tab.current_rtt().unwrap_or_default()).color(palette),
        Phase::Connecting => palette.accent,
        Phase::Reconnecting { .. } => palette.fair,
        Phase::Ended(Ended::Failed(_)) => palette.poor,
        Phase::Ended(_) => palette.muted,
    }
}
