//! The native shell: GPUI Kit application, window lifecycle, wake signaling and input helpers.

pub mod gpu;
pub mod inhibit;
pub mod input;
pub mod keymap;

use std::sync::Arc;

use gpui_kit::{
    AppContext as _, Bounds, Size, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use tokio::sync::Notify;

use crate::ui::assets::RelayAssets;
use crate::ui::{LaunchRequest, RelayApp};
use crate::{APP_ID, APP_NAME};

/// Wakes the GPUI event loop from background threads (RDP sessions, prober, file picker,
/// keyring worker, preview saver). Multiple wakeups coalesce into a single wakeup.
#[derive(Clone)]
pub struct UiWaker {
    notify: Arc<Notify>,
}

impl UiWaker {
    pub fn new() -> (Self, Arc<Notify>) {
        let notify = Arc::new(Notify::new());
        (
            Self {
                notify: Arc::clone(&notify),
            },
            notify,
        )
    }

    /// A waker that does nothing, for headless unit and integration tests.
    #[cfg(test)]
    pub fn noop() -> Self {
        Self {
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn wake(&self) {
        self.notify.notify_one();
    }
}

pub fn run(launch: Option<LaunchRequest>) -> anyhow::Result<()> {
    let mut launch = launch;
    gpui_kit::application()
        .with_assets(RelayAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            crate::ui::theme::install_fonts(cx);

            let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(Size {
                    width: px(720.0),
                    height: px(480.0),
                }),
                titlebar: Some(TitlebarOptions {
                    title: Some(APP_NAME.into()),
                    ..Default::default()
                }),
                app_id: Some(APP_ID.to_owned()),
                focus: true,
                show: true,
                is_resizable: true,
                is_minimizable: true,
                ..Default::default()
            };

            let pending = launch.take();
            if let Err(error) = gpui_kit::open_window(options, cx, move |window, cx| {
                cx.new(|cx| RelayApp::new(pending, window, cx))
            }) {
                tracing::error!(%error, "failed to open window");
                cx.quit();
            }
        });
    Ok(())
}
