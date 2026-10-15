//! One RDP connection, running on its own OS thread with a single-threaded tokio runtime.
//!
//! The UI talks to a session through [`SessionHandle`]: commands go in over an unbounded channel,
//! events come back over a std channel (each one wakes the UI event loop), and decoded pixels
//! are shared through [`SharedFrame`].

mod active;
mod clipboard;
mod connect;
mod drive;
pub mod errors;
pub mod framebuffer;
pub(crate) mod gfx;
#[cfg(test)]
mod integration_tests;
mod netinfo;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ironrdp_graphics::pointer::DecodedPointer;
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use parking_lot::Mutex;
use smallvec::SmallVec;
use tokio::sync::{mpsc, oneshot};
use winit::event_loop::EventLoopProxy;
use zeroize::Zeroizing;

use crate::app::UserEvent;
use crate::store::known_hosts::KnownHosts;
use crate::store::profiles::{AdvancedSettings, DeviceSettings, ExperienceSettings};

pub use self::errors::Failure;
pub use self::framebuffer::SharedFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(u64);

impl SessionId {
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// The remote desktop size we ask for, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopRequest {
    pub width: u16,
    pub height: u16,
    /// Percent; 100 means no scaling.
    pub scale_factor: u32,
}

impl DesktopRequest {
    /// Clamps to what servers accept: 200..=8192 and an even width (MS-RDPEDISP 2.2.2.2.1).
    pub fn normalized(self) -> Self {
        let width = (self.width.clamp(200, 8192)) & !1;
        let height = self.height.clamp(200, 8192);
        // `clamp`, not `if < 100 { 100 } else { min(500) }`: LLVM 20–23 miscompiles that shape
        // (a branch-only argument `range` attribute survives speculation), returning the raw
        // input. The test below guards it.
        let scale_factor = self.scale_factor.clamp(100, 500);
        Self {
            width,
            height,
            scale_factor,
        }
    }
}

pub struct ConnectParams {
    pub hardware_video_decoding: bool,
    pub gpu: Option<Arc<crate::gpu::GpuContext>>,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub domain: String,
    pub password: Zeroizing<String>,
    pub desktop: DesktopRequest,
    /// Whether the remote desktop follows the window size.
    pub dynamic_resize: bool,
    pub experience: ExperienceSettings,
    pub devices: DeviceSettings,
    pub advanced: AdvancedSettings,
    pub known_hosts: Arc<Mutex<KnownHosts>>,
}

/// Connection steps, in order. Shown as the connect track in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// DNS + TCP.
    Network,
    /// RDP negotiation + TLS + certificate check.
    Security,
    /// CredSSP/NLA, licensing, capability exchange.
    SignIn,
    /// Waiting for the first picture.
    Desktop,
}

impl Stage {
    pub const ALL: [Stage; 4] = [
        Stage::Network,
        Stage::Security,
        Stage::SignIn,
        Stage::Desktop,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Stage::Network => "Reach host",
            Stage::Security => "Secure channel",
            Stage::SignIn => "Sign in",
            Stage::Desktop => "Open desktop",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CertificateInfo {
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    pub subject: String,
    pub issuer: String,
    pub not_after: String,
    /// Set when this host was trusted before with a different certificate.
    pub previous_fingerprint: Option<String>,
}

pub struct CertificatePrompt {
    pub info: CertificateInfo,
    pub reply: oneshot::Sender<bool>,
}

#[derive(Debug, Clone)]
pub enum PointerUpdate {
    Default,
    Hidden,
    Bitmap(Arc<DecodedPointer>),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// Smoothed TCP round-trip time.
    pub rtt: Option<Duration>,
    /// Screen updates applied per second.
    pub updates_per_sec: f32,
    pub bytes_in_per_sec: u64,
    pub bytes_out_per_sec: u64,
}

#[derive(Debug, Clone)]
pub enum Ended {
    /// The user disconnected.
    Closed,
    /// The server ended the session (logoff, admin action, another user signing in...).
    ServerEnded(String),
    Failed(Failure),
}

pub enum SessionEvent {
    DecoderStatus {
        name: String,
        reason: Option<String>,
    },
    StageStarted(Stage),
    StageCompleted(Stage, Duration),
    Certificate(CertificatePrompt),
    /// Session is live at this remote desktop size.
    Connected {
        width: u16,
        height: u16,
    },
    DesktopResized {
        width: u16,
        height: u16,
    },
    Pointer(PointerUpdate),
    Reconnecting {
        attempt: u32,
        retry_in: Duration,
        reason: String,
    },
    Stats(Stats),
    Ended(Ended),
}

pub enum SessionCmd {
    Input(SmallVec<[FastPathInputEvent; 2]>),
    Resize(DesktopRequest),
    Disconnect,
    /// Skip the wait and retry now (while reconnecting).
    RetryNow,
    /// The session's tab gained or lost keyboard focus (drives clipboard sync).
    Focus(bool),
    /// From the clipboard backend to the protocol.
    Clipboard(ironrdp_cliprdr::backend::ClipboardMessage),
}

/// Sends events to the UI and wakes its event loop.
#[derive(Clone)]
struct EventSink {
    tx: std::sync::mpsc::Sender<SessionEvent>,
    proxy: EventLoopProxy<UserEvent>,
}

impl EventSink {
    fn send(&self, event: SessionEvent) {
        if self.tx.send(event).is_ok() {
            let _ = self.proxy.send_event(UserEvent::Wake);
        }
    }

    fn wake(&self) {
        let _ = self.proxy.send_event(UserEvent::Wake);
    }
}

pub struct SessionHandle {
    pub id: SessionId,
    pub frame: Arc<SharedFrame>,
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
    events: std::sync::mpsc::Receiver<SessionEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SessionHandle {
    pub fn spawn(params: ConnectParams, proxy: EventLoopProxy<UserEvent>) -> anyhow::Result<Self> {
        let id = SessionId::next();
        let frame = Arc::new(SharedFrame::new());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = std::sync::mpsc::channel();
        let sink = EventSink {
            tx: event_tx,
            proxy,
        };

        let thread_frame = Arc::clone(&frame);
        // The session sends itself commands too (clipboard replies come back this way).
        let self_tx = cmd_tx.clone();
        let thread = std::thread::Builder::new()
            .name(format!("rdp-{}", params.host))
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        sink.send(SessionEvent::Ended(Ended::Failed(Failure::internal(e))));
                        return;
                    }
                };

                let panic_sink = sink.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    runtime.block_on(active::run_session(
                        params,
                        cmd_rx,
                        self_tx,
                        sink,
                        thread_frame,
                    ));
                }));

                if let Err(panic) = result {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_owned())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_owned());
                    tracing::error!(%message, "session thread panicked");
                    panic_sink.send(SessionEvent::Ended(Ended::Failed(Failure::internal(
                        message,
                    ))));
                }
            })?;

        Ok(Self {
            id,
            frame,
            cmd_tx,
            events,
            thread: Some(thread),
        })
    }

    pub fn send(&self, cmd: SessionCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn try_event(&self) -> Option<SessionEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(SessionCmd::Disconnect);
        // Never block the UI on a stuck network call: let the thread finish on its own.
        drop(self.thread.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_request_is_clamped_and_even() {
        let r = DesktopRequest {
            width: 1367,
            height: 50,
            scale_factor: 0,
        }
        .normalized();
        assert_eq!((r.width, r.height, r.scale_factor), (1366, 200, 100));

        let r = DesktopRequest {
            width: 9000,
            height: 9000,
            scale_factor: 900,
        }
        .normalized();
        assert_eq!((r.width, r.height, r.scale_factor), (8192, 8192, 500));
    }
}
