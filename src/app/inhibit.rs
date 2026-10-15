//! Sending system shortcuts (Super, Alt+Tab, …) to the remote computer on Wayland.
//!
//! Compositors keep their own shortcuts unless a client asks them not to, through the
//! `keyboard-shortcuts-inhibit` protocol. winit doesn't expose it, so we bind it ourselves on
//! winit's Wayland connection, against the window's surface.

use raw_window_handle::{HasDisplayHandle as _, HasWindowHandle as _, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1 as Manager;
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::{
    self, ZwpKeyboardShortcutsInhibitorV1 as Inhibitor,
};
use winit::window::Window;

struct State;

pub struct ShortcutInhibitor {
    connection: Connection,
    queue: EventQueue<State>,
    manager: Manager,
    seat: WlSeat,
    surface: WlSurface,
    active: Option<Inhibitor>,
}

impl ShortcutInhibitor {
    /// `None` on X11 or when the compositor lacks the protocol.
    pub fn new(window: &Window) -> Option<Self> {
        let RawDisplayHandle::Wayland(display) = window.display_handle().ok()?.as_raw() else {
            return None;
        };
        let RawWindowHandle::Wayland(handle) = window.window_handle().ok()?.as_raw() else {
            return None;
        };

        // SAFETY: winit owns the display and keeps it alive for as long as the window exists,
        // and this object never outlives the window (it is dropped first, see `App`).
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<State>(&connection).ok()?;
        let qh = queue.handle();

        let manager: Manager = globals
            .bind(&qh, 1..=1, ())
            .map_err(
                |e| tracing::info!(error = %e, "compositor can't pass system shortcuts through"),
            )
            .ok()?;
        let seat: WlSeat = globals.bind(&qh, 1..=1, ()).ok()?;

        // SAFETY: the pointer is winit's live `wl_surface` for this window.
        let surface_id =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), handle.surface.as_ptr().cast()) }
                .ok()?;
        let surface = WlSurface::from_id(&connection, surface_id).ok()?;

        Some(Self {
            connection,
            queue,
            manager,
            seat,
            surface,
            active: None,
        })
    }

    /// Captures (or releases) the compositor's shortcuts. Cheap to call every frame.
    pub fn set(&mut self, capture: bool) {
        match (capture, self.active.is_some()) {
            (true, false) => {
                let qh = self.queue.handle();
                self.active =
                    Some(
                        self.manager
                            .inhibit_shortcuts(&self.surface, &self.seat, &qh, ()),
                    );
                tracing::debug!("capturing system shortcuts");
            }
            (false, true) => {
                if let Some(inhibitor) = self.active.take() {
                    inhibitor.destroy();
                }
                tracing::debug!("released system shortcuts");
            }
            _ => return,
        }
        let _ = self.connection.flush();
        let _ = self.queue.dispatch_pending(&mut State);
    }
}

impl Drop for ShortcutInhibitor {
    fn drop(&mut self) {
        self.set(false);
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: <WlSurface as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<Manager, ()> for State {
    fn event(
        _: &mut Self,
        _: &Manager,
        _: <Manager as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<Inhibitor, ()> for State {
    fn event(
        _: &mut Self,
        _: &Inhibitor,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        tracing::debug!(?event, "shortcut capture state");
    }
}
