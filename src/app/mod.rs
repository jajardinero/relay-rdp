//! The native shell: winit event loop, wgpu painter, egui integration and input routing.
//!
//! We drive winit directly (instead of eframe) because a remote desktop needs raw physical key
//! codes for every key, including modifiers, lock keys and the numpad, which egui's own key
//! events don't carry.

pub mod gpu;
mod inhibit;
pub mod input;
pub mod keymap;

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::ViewportId;
use egui_wgpu::winit::Painter;
use egui_wgpu::{RendererOptions, WgpuConfiguration, WgpuSetup, wgpu};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{CursorIcon, CustomCursor, Fullscreen, Window, WindowId};

use crate::session::PointerUpdate;
use crate::ui::{Hotkey, LaunchRequest, Ui, WindowRequest};
use crate::{APP_ID, APP_NAME};

#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    /// A session produced events or pixels.
    Wake,
}

pub fn run(launch: Option<LaunchRequest>) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use winit::platform::wayland::EventLoopBuilderExtWayland as _;
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .with_wayland()
        .build()
        .context("Relay requires a running Wayland session; X11 is not supported")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(event_loop.create_proxy(), launch);
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct Gfx {
    // Field order matters: the inhibitor borrows winit's Wayland connection and must be dropped
    // before the window.
    inhibitor: Option<inhibit::ShortcutInhibitor>,
    window: Arc<Window>,
    painter: Painter,
    egui_state: egui_winit::State,
}

/// What the window cursor currently shows while it is over a remote desktop.
#[derive(PartialEq)]
enum AppliedCursor {
    Egui,
    Hidden,
    Default,
    Bitmap(usize),
}

struct App {
    egui_ctx: egui::Context,
    ui: Ui,
    gfx: Option<Gfx>,
    textures: gpu::RemoteTextures,
    modifiers: ModifiersState,
    next_repaint: Option<Instant>,
    cursor_cache: HashMap<usize, CustomCursor>,
    applied_cursor: AppliedCursor,
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>, launch: Option<LaunchRequest>) -> Self {
        let egui_ctx = egui::Context::default();
        let ui = Ui::new(&egui_ctx, proxy, launch);
        Self {
            egui_ctx,
            ui,
            gfx: None,
            textures: gpu::RemoteTextures::default(),
            modifiers: ModifiersState::empty(),
            next_repaint: None,
            cursor_cache: HashMap::new(),
            applied_cursor: AppliedCursor::Egui,
        }
    }

    fn init_graphics(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<()> {
        let attrs = Window::default_attributes()
            .with_title(APP_NAME)
            .with_inner_size(LogicalSize::new(1280.0, 800.0))
            .with_min_inner_size(LogicalSize::new(720.0, 480.0));
        let attrs =
            winit::platform::wayland::WindowAttributesExtWayland::with_name(attrs, APP_ID, APP_ID);
        let window = Arc::new(event_loop.create_window(attrs)?);

        let mut setup = WgpuSetup::from_display_handle(event_loop.owned_display_handle());
        // Hybrid laptops: drawing a 2D UI on the integrated GPU avoids waking the discrete one
        // and a cross-GPU copy per frame. `WGPU_POWER_PREF` still overrides this.
        if let WgpuSetup::CreateNew(create) = &mut setup
            && std::env::var_os("WGPU_POWER_PREF").is_none()
        {
            create.power_preference = wgpu::PowerPreference::LowPower;
        }
        let config = WgpuConfiguration {
            surface: if self.ui.settings().low_latency {
                egui_wgpu::SurfaceConfig::LOW_LATENCY
            } else {
                egui_wgpu::SurfaceConfig::HIGH_THROUGHPUT
            },
            wgpu_setup: pollster::block_on(crate::gpu::configure(setup, Arc::clone(&window)))?,
            ..Default::default()
        };

        let mut painter = pollster::block_on(Painter::new(
            self.egui_ctx.clone(),
            config,
            false,
            RendererOptions::default(),
        ));
        pollster::block_on(painter.set_window(ViewportId::ROOT, Some(Arc::clone(&window))))?;
        if let Some(rs) = painter.render_state() {
            self.ui.gpu = Some(crate::gpu::GpuContext::new(&rs));
            let info = rs.adapter.get_info();
            tracing::info!(adapter = %info.name, backend = ?info.backend, "graphics ready");
        }

        let egui_state = egui_winit::State::new(
            self.egui_ctx.clone(),
            ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            painter.max_texture_side(),
        );

        window.request_redraw();
        let inhibitor = inhibit::ShortcutInhibitor::new(&window);
        self.gfx = Some(Gfx {
            inhibitor,
            window,
            painter,
            egui_state,
        });
        Ok(())
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let Some(gfx) = &mut self.gfx else { return };
        self.next_repaint = None;

        self.ui.poll();
        self.ui
            .set_fullscreen_state(gfx.window.fullscreen().is_some());

        if let Some(render_state) = gfx.painter.render_state() {
            self.textures
                .sync(&render_state, &self.ui.frames(), self.ui.texture_filters());
        }

        let raw_input = gfx.egui_state.take_egui_input(&gfx.window);
        let ui = &mut self.ui;
        let textures = &self.textures;
        let mut output = self
            .egui_ctx
            .run_ui(raw_input, |root| ui.show(root, textures));

        let remote_cursor = self.ui.remote_cursor();
        if remote_cursor.is_some() {
            // Over the remote desktop the remote cursor replaces egui's (applied below).
            output.platform_output.cursor_icon = egui::CursorIcon::None;
        }
        gfx.egui_state
            .handle_platform_output(&gfx.window, output.platform_output);
        apply_cursor(
            event_loop,
            &gfx.window,
            remote_cursor,
            &mut self.cursor_cache,
            &mut self.applied_cursor,
        );

        let primitives = self
            .egui_ctx
            .tessellate(output.shapes, output.pixels_per_point);
        let clear = self.ui.clear_color();
        gfx.painter.paint_and_update_textures(
            ViewportId::ROOT,
            output.pixels_per_point,
            clear,
            &primitives,
            &mut output.textures_delta,
            Vec::new(),
            &gfx.window,
        );

        if let Some(inhibitor) = &mut gfx.inhibitor {
            inhibitor.set(self.ui.wants_shortcut_capture());
        }

        for request in self.ui.take_window_requests() {
            match request {
                WindowRequest::SetFullscreen(on) => {
                    gfx.window
                        .set_fullscreen(on.then_some(Fullscreen::Borderless(None)));
                }
                WindowRequest::SetTitle(title) => gfx.window.set_title(&title),
                WindowRequest::Quit => {
                    self.ui.shutdown();
                    event_loop.exit();
                }
            }
        }

        let delay = output
            .viewport_output
            .get(&ViewportId::ROOT)
            .map_or(Duration::MAX, |v| v.repaint_delay);
        if delay.is_zero() {
            gfx.window.request_redraw();
        } else if delay < Duration::from_secs(3600) {
            self.next_repaint = Some(Instant::now() + delay);
        }
    }

    fn request_redraw(&self) {
        if let Some(gfx) = &self.gfx {
            gfx.window.request_redraw();
        }
    }

    fn handle_keyboard(&mut self, event: &winit::event::KeyEvent) -> bool {
        let pressed = event.state == ElementState::Pressed;
        let PhysicalKey::Code(code) = event.physical_key else {
            return false;
        };

        // Local shortcuts, using the mstsc conventions so they don't clash with the remote.
        let ctrl_alt = self.modifiers.control_key() && self.modifiers.alt_key();
        if pressed && ctrl_alt && !event.repeat {
            let hotkey = match code {
                KeyCode::Enter | KeyCode::NumpadEnter => Some(Hotkey::ToggleFullscreen),
                KeyCode::Home => Some(Hotkey::ShowSessionBar),
                KeyCode::End => Some(Hotkey::SendCtrlAltDel),
                _ => None,
            };
            if let Some(hotkey) = hotkey {
                self.ui.hotkey(hotkey);
                self.request_redraw();
                return true;
            }
        }

        let Some(session) = self.ui.keyboard_session(&self.egui_ctx) else {
            return false;
        };
        if code == KeyCode::Pause {
            if pressed {
                session.send_raw_input(input::pause_sequence());
            }
            return true;
        }
        if let Some(scancode) = keymap::scancode(code) {
            session.key(scancode, pressed, event.repeat);
        }
        true
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none()
            && let Err(e) = self.init_graphics(event_loop)
        {
            tracing::error!(error = %e, "failed to initialise graphics");
            event_loop.exit();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wake => self.request_redraw(),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.gfx.is_none() {
            return;
        }
        let ppp = self.egui_ctx.pixels_per_point();

        match &event {
            WindowEvent::RedrawRequested => {
                self.redraw(event_loop);
                return;
            }
            WindowEvent::CloseRequested => {
                self.ui.shutdown();
                event_loop.exit();
                return;
            }
            WindowEvent::Resized(size) => {
                if let (Some(gfx), Some(w), Some(h)) = (
                    &mut self.gfx,
                    NonZeroU32::new(size.width),
                    NonZeroU32::new(size.height),
                ) {
                    gfx.painter.on_window_resized(ViewportId::ROOT, w, h);
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::Focused(focused) => {
                if !focused {
                    self.ui.release_all_input();
                }
                self.ui.set_window_focus(*focused);
            }
            WindowEvent::KeyboardInput {
                event: key_event,
                is_synthetic,
                ..
            } => {
                if *is_synthetic || self.handle_keyboard(key_event) {
                    return;
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let pos = egui::pos2(position.x as f32 / ppp, position.y as f32 / ppp);
                let to_remote = self.ui.pointer_moved(&self.egui_ctx, pos);
                let wants_repaint = !to_remote || self.ui.wants_pointer_repaints();
                let Some(gfx) = &mut self.gfx else { return };
                let response = gfx.egui_state.on_window_event(&gfx.window, &event);
                // While the remote desktop has the pointer, its picture updates drive repaints;
                // repainting the UI for every mouse move would only burn GPU time.
                if response.repaint && wants_repaint {
                    gfx.window.request_redraw();
                }
                return;
            }
            WindowEvent::CursorLeft { .. } => {
                self.ui.pointer_left();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = *state == ElementState::Pressed;
                if self.ui.pointer_button(&self.egui_ctx, *button, pressed) {
                    // Still let egui see releases so it never thinks a button is stuck.
                    if !pressed && let Some(gfx) = &mut self.gfx {
                        let _ = gfx.egui_state.on_window_event(&gfx.window, &event);
                    }
                    return;
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.ui.pointer_wheel(&self.egui_ctx, *delta) {
                    return;
                }
            }
            _ => {}
        }

        let Some(gfx) = &mut self.gfx else { return };
        let response = gfx.egui_state.on_window_event(&gfx.window, &event);
        if response.repaint {
            gfx.window.request_redraw();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Mouse moves are coalesced per event batch: one move per batch, not one per event.
        self.ui.flush_pointer();

        match self.next_repaint {
            Some(at) if at <= Instant::now() => {
                self.next_repaint = None;
                self.request_redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.ui.shutdown();
        if let Some(gfx) = &mut self.gfx {
            gfx.painter.destroy();
        }
    }
}

/// Shows the remote computer's cursor while the pointer is over its desktop. Cursor changes are
/// sent to the compositor only when they actually change.
fn apply_cursor(
    event_loop: &ActiveEventLoop,
    window: &Window,
    remote: Option<PointerUpdate>,
    cache: &mut HashMap<usize, CustomCursor>,
    applied: &mut AppliedCursor,
) {
    let wanted = match &remote {
        None => AppliedCursor::Egui,
        Some(PointerUpdate::Hidden) => AppliedCursor::Hidden,
        Some(PointerUpdate::Default) => AppliedCursor::Default,
        Some(PointerUpdate::Bitmap(p)) => AppliedCursor::Bitmap(Arc::as_ptr(p) as usize),
    };
    if *applied == wanted {
        return;
    }

    match &remote {
        // egui-winit restores its own cursor on the next frame.
        None => {}
        Some(PointerUpdate::Hidden) => window.set_cursor_visible(false),
        Some(PointerUpdate::Default) => {
            window.set_cursor(CursorIcon::Default);
            window.set_cursor_visible(true);
        }
        Some(PointerUpdate::Bitmap(pointer)) => {
            let key = Arc::as_ptr(pointer) as usize;
            if !cache.contains_key(&key) {
                if cache.len() > 64 {
                    cache.clear();
                }
                match make_cursor(event_loop, pointer) {
                    Some(cursor) => {
                        cache.insert(key, cursor);
                    }
                    None => {
                        window.set_cursor(CursorIcon::Default);
                        window.set_cursor_visible(true);
                        *applied = wanted;
                        return;
                    }
                }
            }
            window.set_cursor(cache[&key].clone());
            window.set_cursor_visible(true);
        }
    }
    *applied = wanted;
}

fn make_cursor(
    event_loop: &ActiveEventLoop,
    pointer: &ironrdp_graphics::pointer::DecodedPointer,
) -> Option<CustomCursor> {
    if pointer.width == 0 || pointer.height == 0 {
        return None;
    }
    let source = CustomCursor::from_rgba(
        pointer.bitmap_data.clone(),
        pointer.width,
        pointer.height,
        pointer.hotspot_x.min(pointer.width - 1),
        pointer.hotspot_y.min(pointer.height - 1),
    )
    .map_err(|e| tracing::debug!(error = %e, "unusable remote cursor"))
    .ok()?;
    Some(event_loop.create_custom_cursor(source))
}
