//! One session tab: the connection handle plus everything the UI tracks about it.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ironrdp_input::{Database, Operation, Scancode};
use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use parking_lot::Mutex;
use smallvec::SmallVec;
use winit::event_loop::EventLoopProxy;
use zeroize::Zeroizing;

use crate::app::UserEvent;
use crate::app::input::WheelAccumulator;
use crate::app::keymap::codes;
use crate::session::errors::FailureKind;
use crate::session::{
    CertificatePrompt, ConnectParams, DesktopRequest, Ended, PointerUpdate, Rendering, SessionCmd,
    SessionEvent, SessionHandle, Stage, Stats,
};
use crate::store::known_hosts::KnownHosts;
use crate::store::profiles::{HostProfile, ResolutionMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(u64);

impl TabId {
    fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StageState {
    Pending,
    Active(Instant),
    Done(Duration),
    Failed,
}

pub enum Phase {
    Connecting,
    Live,
    Reconnecting {
        attempt: u32,
        retry_at: Instant,
        reason: String,
    },
    Ended(Ended),
}

/// RTT samples kept for the session bar sparkline (one per second).
const RTT_HISTORY: usize = 60;

pub struct SessionTab {
    pub id: TabId,
    pub handle: SessionHandle,
    pub profile: HostProfile,
    /// Whether `profile` is in the saved library (false for quick connections).
    pub saved: bool,
    password: Zeroizing<String>,
    pub phase: Phase,
    pub stages: [StageState; 4],
    pub cert_prompt: Option<CertificatePrompt>,
    pub remote_size: Option<(u16, u16)>,
    pub cursor: PointerUpdate,
    pub stats: Stats,
    /// How the picture is rendered, for the session bar label.
    pub rendering: Option<Rendering>,
    pub rtt_history: VecDeque<f32>,
    pub last_request: Option<DesktopRequest>,
    /// Was the session ever live? Decides between the connect track and the "disconnected" card.
    pub was_live: bool,
    pub bar: BarState,
    db: Database,
    pub wheel: WheelAccumulator,
}

#[derive(Default)]
pub struct BarState {
    /// Keep the bar visible until this time.
    pub visible_until: Option<Instant>,
    /// When the pointer started resting at the top edge.
    pub edge_since: Option<Instant>,
    /// True while a menu from the bar is open or the pointer is over it.
    pub engaged: bool,
}

pub struct SessionContext<'a> {
    pub hardware_video_decoding: bool,
    pub gpu: Option<Arc<crate::gpu::GpuContext>>,
    pub proxy: &'a EventLoopProxy<UserEvent>,
    pub known_hosts: &'a Arc<Mutex<KnownHosts>>,
}

impl SessionTab {
    pub fn start(
        profile: HostProfile,
        saved: bool,
        password: Zeroizing<String>,
        desktop: DesktopRequest,
        cx: &SessionContext<'_>,
    ) -> anyhow::Result<Self> {
        let handle = spawn(&profile, &password, desktop, cx)?;
        Ok(Self {
            id: TabId::next(),
            handle,
            profile,
            saved,
            password,
            phase: Phase::Connecting,
            stages: [StageState::Pending; 4],
            cert_prompt: None,
            remote_size: None,
            cursor: PointerUpdate::Default,
            stats: Stats::default(),
            rendering: None,
            rtt_history: VecDeque::with_capacity(RTT_HISTORY),
            last_request: Some(desktop),
            was_live: false,
            bar: BarState::default(),
            db: Database::new(),
            wheel: WheelAccumulator::default(),
        })
    }

    /// Starts a fresh connection in this tab (after a failure or disconnect).
    pub fn restart(
        &mut self,
        password: Option<Zeroizing<String>>,
        desktop: DesktopRequest,
        cx: &SessionContext<'_>,
    ) -> anyhow::Result<()> {
        if let Some(password) = password {
            self.password = password;
        }
        let handle = spawn(&self.profile, &self.password, desktop, cx)?;
        self.handle = handle;
        self.phase = Phase::Connecting;
        self.rendering = None;
        self.stages = [StageState::Pending; 4];
        self.cert_prompt = None;
        self.remote_size = None;
        self.cursor = PointerUpdate::Default;
        self.last_request = Some(desktop);
        self.was_live = false;
        self.db = Database::new();
        self.wheel.reset();
        Ok(())
    }

    pub fn title(&self) -> &str {
        self.profile.display_name()
    }

    pub fn is_live(&self) -> bool {
        matches!(self.phase, Phase::Live)
    }

    pub fn failure_kind(&self) -> Option<FailureKind> {
        match &self.phase {
            Phase::Ended(Ended::Failed(f)) => Some(f.kind),
            _ => None,
        }
    }

    pub fn handle_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Rendering(rendering) => self.rendering = Some(rendering),
            SessionEvent::StageStarted(stage) => {
                self.stages[stage_index(stage)] = StageState::Active(Instant::now());
            }
            SessionEvent::StageCompleted(stage, took) => {
                self.stages[stage_index(stage)] = StageState::Done(took);
                if stage == Stage::Desktop {
                    self.go_live();
                }
            }
            SessionEvent::Certificate(prompt) => self.cert_prompt = Some(prompt),
            SessionEvent::Connected { width, height } => {
                self.remote_size = Some((width, height));
                // After a reconnect the picture comes back right away; no connect track.
                if self.was_live {
                    self.go_live();
                }
            }
            SessionEvent::DesktopResized { width, height } => {
                self.remote_size = Some((width, height))
            }
            SessionEvent::Pointer(pointer) => self.cursor = pointer,
            // The host library records it (`Ui::poll`).
            SessionEvent::HostOs(_) => {}
            SessionEvent::Reconnecting {
                attempt,
                retry_in,
                reason,
            } => {
                self.db = Database::new();
                self.phase = Phase::Reconnecting {
                    attempt,
                    retry_at: Instant::now() + retry_in,
                    reason,
                };
            }
            SessionEvent::Stats(stats) => {
                self.stats = stats;
                if let Some(rtt) = stats.rtt {
                    if self.rtt_history.len() == RTT_HISTORY {
                        self.rtt_history.pop_front();
                    }
                    self.rtt_history.push_back(rtt.as_secs_f32() * 1000.0);
                }
            }
            SessionEvent::Ended(ended) => {
                if let Ended::Failed(failure) = &ended {
                    tracing::info!(title = %failure.title, detail = %failure.detail, "session failed");
                    if let Some(active) = self
                        .stages
                        .iter_mut()
                        .find(|s| matches!(s, StageState::Active(_)))
                    {
                        *active = StageState::Failed;
                    }
                }
                self.cert_prompt = None;
                self.phase = Phase::Ended(ended);
            }
        }
    }

    fn go_live(&mut self) {
        let first = !self.was_live;
        self.phase = Phase::Live;
        self.was_live = true;
        if first {
            // Show the session bar briefly so people learn where it lives.
            self.bar.visible_until = Some(Instant::now() + Duration::from_millis(2500));
        }
    }

    /// The desktop size to request for a session area of `physical` pixels.
    pub fn desired_desktop(
        profile: &HostProfile,
        physical: egui::Vec2,
        pixels_per_point: f32,
    ) -> DesktopRequest {
        let scale_factor = profile
            .display
            .scale_percent
            .unwrap_or_else(|| (pixels_per_point * 100.0).round() as u32);
        let (width, height) = match profile.display.resolution {
            ResolutionMode::MatchWindow => (physical.x.round() as u16, physical.y.round() as u16),
            ResolutionMode::Fixed => (profile.display.fixed_width, profile.display.fixed_height),
        };
        DesktopRequest {
            width,
            height,
            scale_factor,
        }
        .normalized()
    }

    /// Asks the server to follow the window size, if it changed.
    pub fn follow_size(&mut self, desired: DesktopRequest) {
        if self.profile.display.resolution != ResolutionMode::MatchWindow {
            return;
        }
        if self.last_request != Some(desired) {
            self.last_request = Some(desired);
            self.handle.send(SessionCmd::Resize(desired));
        }
    }

    pub fn send_ops(&mut self, ops: impl IntoIterator<Item = Operation>) {
        let events = self.db.apply(ops);
        if !events.is_empty() {
            self.handle.send(SessionCmd::Input(events));
        }
    }

    pub fn send_raw_input(&self, events: SmallVec<[FastPathInputEvent; 2]>) {
        self.handle.send(SessionCmd::Input(events));
    }

    pub fn key(&mut self, scancode: Scancode, pressed: bool, repeat: bool) {
        if pressed && repeat && self.db.is_key_pressed(scancode) {
            // Auto-repeat is a repeated key-down without a key-up, as on a real keyboard.
            let (extended, code) = scancode.as_u8();
            let flags = if extended {
                KeyboardFlags::EXTENDED
            } else {
                KeyboardFlags::empty()
            };
            let mut events = SmallVec::new();
            events.push(FastPathInputEvent::KeyboardEvent(flags, code));
            self.send_raw_input(events);
            return;
        }
        let op = if pressed {
            Operation::KeyPressed(scancode)
        } else {
            Operation::KeyReleased(scancode)
        };
        self.send_ops([op]);
    }

    /// Releases every key and button we told the server is down. Called on focus loss so the
    /// remote side never ends up with a stuck Alt or Ctrl.
    pub fn release_all(&mut self) {
        let events = self.db.release_all();
        if !events.is_empty() {
            self.handle.send(SessionCmd::Input(events));
        }
        self.wheel.reset();
    }

    pub fn send_combo(&mut self, keys: &[Scancode]) {
        let presses = keys.iter().map(|k| Operation::KeyPressed(*k));
        let releases = keys.iter().rev().map(|k| Operation::KeyReleased(*k));
        let ops: Vec<Operation> = presses.chain(releases).collect();
        self.send_ops(ops);
    }

    pub fn send_ctrl_alt_del(&mut self) {
        self.send_combo(&[codes::CONTROL_LEFT, codes::ALT_LEFT, codes::DELETE]);
    }

    pub fn disconnect(&mut self) {
        self.handle.send(SessionCmd::Disconnect);
    }

    pub fn retry_now(&self) {
        self.handle.send(SessionCmd::RetryNow);
    }

    pub fn answer_certificate(&mut self, trust: bool) {
        if let Some(prompt) = self.cert_prompt.take() {
            let _ = prompt.reply.send(trust);
        }
    }

    pub fn current_rtt(&self) -> Option<Duration> {
        self.stats.rtt
    }
}

/// Accepts `DOMAIN\user` typed into the user name field when no domain is set.
/// `user@domain` (UPN) is passed through unchanged; servers accept it as is.
fn split_account(username: &str, domain: &str) -> (String, String) {
    if domain.is_empty()
        && let Some((domain, user)) = username.split_once('\\')
    {
        return (user.to_owned(), domain.to_owned());
    }
    (username.to_owned(), domain.to_owned())
}

fn stage_index(stage: Stage) -> usize {
    Stage::ALL
        .iter()
        .position(|s| *s == stage)
        .expect("stage in ALL")
}

fn spawn(
    profile: &HostProfile,
    password: &Zeroizing<String>,
    desktop: DesktopRequest,
    cx: &SessionContext<'_>,
) -> anyhow::Result<SessionHandle> {
    let (username, domain) = split_account(profile.username.trim(), profile.domain.trim());
    let params = ConnectParams {
        hardware_video_decoding: cx.hardware_video_decoding,
        gpu: cx.gpu.clone(),
        host: profile.address.trim().to_owned(),
        port: profile.port,
        username,
        domain,
        password: password.clone(),
        desktop,
        dynamic_resize: profile.display.resolution == ResolutionMode::MatchWindow,
        experience: profile.experience.clone(),
        devices: profile.devices.clone(),
        advanced: profile.advanced.clone(),
        known_hosts: Arc::clone(cx.known_hosts),
    };
    SessionHandle::spawn(params, cx.proxy.clone())
}
