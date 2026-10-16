//! Translating local pointer and wheel input into RDP input operations.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ironrdp_input::{MouseButton as RdpButton, MousePosition, Operation, WheelRotations};
use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use smallvec::SmallVec;
use winit::event::{MouseButton, MouseScrollDelta, TouchPhase};

/// Rotation units for one wheel notch.
const NOTCH: f64 = 120.0;
/// Touchpads report pixels; this many pixels count as one notch.
const PIXELS_PER_NOTCH: f64 = 50.0;
/// The wire format holds a 9-bit signed value.
const MAX_UNITS_PER_EVENT: i16 = 255;
/// The touchpad speed when the fingers lift comes from this much of their last movement.
const VELOCITY_WINDOW: Duration = Duration::from_millis(80);
/// A glide starts only if the fingers moved this recently before lifting, not after resting.
const MAX_REST: Duration = Duration::from_millis(60);
/// A glide starts above this speed and ends below the second, in touchpad pixels per second.
const GLIDE_START: f64 = 100.0;
const GLIDE_END: f64 = 15.0;
/// A glide slows down exponentially with this time constant, as local toolkits' do.
const GLIDE_DECAY: f64 = 0.325;
/// Glide steps are sent this often.
pub const GLIDE_STEP: Duration = Duration::from_millis(16);

pub fn rdp_button(button: MouseButton) -> Option<RdpButton> {
    match button {
        MouseButton::Left => Some(RdpButton::Left),
        MouseButton::Right => Some(RdpButton::Right),
        MouseButton::Middle => Some(RdpButton::Middle),
        MouseButton::Back => Some(RdpButton::X1),
        MouseButton::Forward => Some(RdpButton::X2),
        MouseButton::Other(_) => None,
    }
}

/// How touchpad scrolling behaves (Settings).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchpadScrolling {
    /// Multiplies the distance touchpad scrolling covers.
    pub speed: f64,
    /// Keep scrolling after the fingers lift, slowing down, as local apps do.
    pub momentum: bool,
}

impl Default for TouchpadScrolling {
    fn default() -> Self {
        Self {
            speed: 1.0,
            momentum: true,
        }
    }
}

/// Turns local scrolling into wheel rotations. Accumulates fractional scrolling (touchpads) so
/// slow two-finger scrolls still arrive, and continues a touchpad scroll after the fingers
/// lift: compositors leave that momentum to applications.
#[derive(Default)]
pub struct WheelAccumulator {
    vertical: f64,
    horizontal: f64,
    /// The touchpad's recent movement: when, and how far (x, y) in pixels.
    recent: VecDeque<(Instant, f64, f64)>,
    glide: Option<Glide>,
}

/// Scrolling that continues after the fingers lifted.
struct Glide {
    /// Pixels per second.
    velocity: (f64, f64),
    /// When the last step was taken.
    last: Instant,
}

impl WheelAccumulator {
    pub fn operations(
        &mut self,
        delta: MouseScrollDelta,
        phase: TouchPhase,
        now: Instant,
        touchpad: TouchpadScrolling,
    ) -> SmallVec<[Operation; 4]> {
        // Any new scrolling takes over from a glide.
        self.glide = None;
        match delta {
            MouseScrollDelta::LineDelta(x, y) => {
                self.recent.clear();
                self.rotate(f64::from(x) * NOTCH, f64::from(y) * NOTCH)
            }
            MouseScrollDelta::PixelDelta(p) => {
                if phase == TouchPhase::Started {
                    self.recent.clear();
                }
                if p.x != 0.0 || p.y != 0.0 {
                    self.recent.push_back((now, p.x, p.y));
                }
                while self
                    .recent
                    .front()
                    .is_some_and(|(at, ..)| now.duration_since(*at) > VELOCITY_WINDOW)
                {
                    self.recent.pop_front();
                }
                if phase == TouchPhase::Ended {
                    self.start_glide(now, touchpad);
                }
                self.pixels(p.x, p.y, touchpad.speed)
            }
        }
    }

    /// Starts a glide at the speed of the fingers' last movement, if they were moving.
    fn start_glide(&mut self, now: Instant, touchpad: TouchpadScrolling) {
        let recent = std::mem::take(&mut self.recent);
        let (Some((first, ..)), Some((last, ..))) = (recent.front(), recent.back()) else {
            return;
        };
        if !touchpad.momentum || now.duration_since(*last) > MAX_REST {
            return;
        }
        // The movement spans from just before the first event to now.
        let span = now.duration_since(*first).max(GLIDE_STEP).as_secs_f64();
        let (x, y) = recent
            .iter()
            .fold((0.0, 0.0), |(x, y), (_, dx, dy)| (x + dx, y + dy));
        let velocity = (x / span, y / span);
        if velocity.0.hypot(velocity.1) > GLIDE_START {
            self.glide = Some(Glide {
                velocity,
                last: now,
            });
        }
    }

    /// When the glide's next step is due.
    pub fn next_step(&self) -> Option<Instant> {
        self.glide.as_ref().map(|glide| glide.last + GLIDE_STEP)
    }

    /// The glide's movement since its last step, slowing down.
    pub fn glide(&mut self, now: Instant, touchpad: TouchpadScrolling) -> SmallVec<[Operation; 4]> {
        let Some(glide) = &mut self.glide else {
            return SmallVec::new();
        };
        let elapsed = now.duration_since(glide.last).as_secs_f64();
        let decay = (-elapsed / GLIDE_DECAY).exp();
        // The distance an exponentially slowing scroll covers in `elapsed`.
        let travel = GLIDE_DECAY * (1.0 - decay);
        let (dx, dy) = (glide.velocity.0 * travel, glide.velocity.1 * travel);
        glide.velocity = (glide.velocity.0 * decay, glide.velocity.1 * decay);
        glide.last = now;
        if glide.velocity.0.hypot(glide.velocity.1) < GLIDE_END {
            self.glide = None;
        }
        self.pixels(dx, dy, touchpad.speed)
    }

    fn pixels(&mut self, x: f64, y: f64, speed: f64) -> SmallVec<[Operation; 4]> {
        let scale = NOTCH / PIXELS_PER_NOTCH * speed;
        self.rotate(x * scale, y * scale)
    }

    fn rotate(&mut self, dx: f64, dy: f64) -> SmallVec<[Operation; 4]> {
        // winit: positive y scrolls content up (wheel away from the user), matching RDP.
        // Horizontal is mirrored: positive x in winit moves content right, i.e. scrolls left.
        self.vertical += dy;
        self.horizontal -= dx;

        let mut ops = SmallVec::new();
        push_rotations(&mut ops, &mut self.vertical, true);
        push_rotations(&mut ops, &mut self.horizontal, false);
        ops
    }

    /// Stops a glide, as a click does locally.
    pub fn stop_glide(&mut self) {
        self.glide = None;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

fn push_rotations(ops: &mut SmallVec<[Operation; 4]>, accumulated: &mut f64, is_vertical: bool) {
    let mut whole = accumulated.trunc() as i32;
    *accumulated -= f64::from(whole);
    while whole != 0 {
        let step = whole.clamp(
            -i32::from(MAX_UNITS_PER_EVENT),
            i32::from(MAX_UNITS_PER_EVENT),
        );
        whole -= step;
        ops.push(Operation::WheelRotations(WheelRotations {
            is_vertical,
            rotation_units: step as i16,
        }));
    }
}

/// Pause is the one key with an E1-prefixed scancode, which `ironrdp_input` can't express.
/// It has no release of its own: the sequence is press-and-release in one go.
pub fn pause_sequence() -> SmallVec<[FastPathInputEvent; 2]> {
    let mut events = SmallVec::new();
    events.push(FastPathInputEvent::KeyboardEvent(
        KeyboardFlags::EXTENDED1,
        0x1D,
    ));
    events.push(FastPathInputEvent::KeyboardEvent(
        KeyboardFlags::empty(),
        0x45,
    ));
    events.push(FastPathInputEvent::KeyboardEvent(
        KeyboardFlags::EXTENDED1 | KeyboardFlags::RELEASE,
        0x1D,
    ));
    events.push(FastPathInputEvent::KeyboardEvent(
        KeyboardFlags::RELEASE,
        0x45,
    ));
    events
}

/// Where the remote desktop is drawn, used to map pointer positions into remote pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RemoteViewport {
    /// Image rectangle in egui points.
    pub rect: egui::Rect,
    pub remote_width: u16,
    pub remote_height: u16,
}

impl RemoteViewport {
    /// Maps a position in points to remote pixels, clamped to the desktop.
    pub fn to_remote(&self, pos: egui::Pos2) -> MousePosition {
        let w = self.rect.width().max(1.0);
        let h = self.rect.height().max(1.0);
        let x = ((pos.x - self.rect.min.x) / w * f32::from(self.remote_width)).floor();
        let y = ((pos.y - self.rect.min.y) / h * f32::from(self.remote_height)).floor();
        MousePosition {
            x: x.clamp(0.0, f32::from(self.remote_width.saturating_sub(1))) as u16,
            y: y.clamp(0.0, f32::from(self.remote_height.saturating_sub(1))) as u16,
        }
    }

    pub fn contains(&self, pos: egui::Pos2) -> bool {
        self.rect.contains(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel_units(ops: &[Operation]) -> Vec<(bool, i16)> {
        ops.iter()
            .map(|op| match op {
                Operation::WheelRotations(r) => (r.is_vertical, r.rotation_units),
                _ => panic!("unexpected op"),
            })
            .collect()
    }

    fn scroll(acc: &mut WheelAccumulator, delta: MouseScrollDelta) -> SmallVec<[Operation; 4]> {
        let touchpad = TouchpadScrolling::default();
        acc.operations(delta, TouchPhase::Moved, Instant::now(), touchpad)
    }

    fn pixels(y: f64) -> MouseScrollDelta {
        MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, y))
    }

    fn total(ops: &[Operation]) -> i32 {
        wheel_units(ops).iter().map(|(_, u)| i32::from(*u)).sum()
    }

    #[test]
    fn line_scroll_is_one_notch() {
        let mut acc = WheelAccumulator::default();
        let ops = scroll(&mut acc, MouseScrollDelta::LineDelta(0.0, 1.0));
        assert_eq!(wheel_units(&ops), vec![(true, 120)]);
        let ops = scroll(&mut acc, MouseScrollDelta::LineDelta(0.0, -1.0));
        assert_eq!(wheel_units(&ops), vec![(true, -120)]);
    }

    #[test]
    fn large_scrolls_are_split() {
        let mut acc = WheelAccumulator::default();
        let ops = scroll(&mut acc, MouseScrollDelta::LineDelta(0.0, 3.0));
        assert_eq!(wheel_units(&ops), vec![(true, 255), (true, 105)]);
    }

    #[test]
    fn small_pixel_scrolls_accumulate() {
        let mut acc = WheelAccumulator::default();
        assert!(scroll(&mut acc, pixels(0.2)).is_empty());
        let total: i32 = (0..10).map(|_| total(&scroll(&mut acc, pixels(0.2)))).sum();
        assert!(total > 0);
    }

    /// Touchpad scrolling at 1,000 px/s for 45 ms, lifting the fingers right away.
    fn flick(acc: &mut WheelAccumulator, touchpad: TouchpadScrolling, start: Instant) -> Instant {
        let ms = Duration::from_millis;
        for i in 0..5 {
            let phase = if i == 0 {
                TouchPhase::Started
            } else {
                TouchPhase::Moved
            };
            acc.operations(pixels(10.0), phase, start + ms(10 * i), touchpad);
        }
        let lift = start + ms(45);
        acc.operations(pixels(0.0), TouchPhase::Ended, lift, touchpad);
        lift
    }

    #[test]
    fn a_flick_glides_and_slows_to_a_stop() {
        let touchpad = TouchpadScrolling::default();
        let mut acc = WheelAccumulator::default();
        let lift = flick(&mut acc, touchpad, Instant::now());
        let mut steps = Vec::new();
        while let Some(at) = acc.next_step() {
            assert!(at - lift < Duration::from_secs(3), "the glide ends");
            steps.push(total(&acc.glide(at, touchpad)));
        }
        // About 1,111 px/s for the time constant, 0.325 s, at 120 units per 50 px.
        let glided: i32 = steps.iter().sum();
        assert!((750..950).contains(&glided), "{glided}");
        assert!(
            steps[0] > *steps.last().unwrap(),
            "it slows down: {steps:?}"
        );
    }

    #[test]
    fn resting_fingers_or_momentum_off_do_not_glide() {
        let ms = Duration::from_millis;
        let start = Instant::now();
        let mut acc = WheelAccumulator::default();
        let touchpad = TouchpadScrolling::default();
        acc.operations(pixels(30.0), TouchPhase::Started, start, touchpad);
        acc.operations(pixels(0.0), TouchPhase::Ended, start + ms(200), touchpad);
        assert!(acc.next_step().is_none());

        let off = TouchpadScrolling {
            momentum: false,
            ..touchpad
        };
        flick(&mut acc, off, start);
        assert!(acc.next_step().is_none());
    }

    #[test]
    fn new_scrolling_stops_a_glide() {
        let touchpad = TouchpadScrolling::default();
        let mut acc = WheelAccumulator::default();
        let lift = flick(&mut acc, touchpad, Instant::now());
        assert!(acc.next_step().is_some());
        acc.operations(pixels(1.0), TouchPhase::Started, lift, touchpad);
        assert!(acc.next_step().is_none());
    }

    #[test]
    fn speed_scales_touchpads_not_wheels() {
        let fast = TouchpadScrolling {
            speed: 2.0,
            momentum: true,
        };
        let mut acc = WheelAccumulator::default();
        let now = Instant::now();
        let notch = MouseScrollDelta::LineDelta(0.0, 1.0);
        assert_eq!(
            total(&acc.operations(notch, TouchPhase::Moved, now, fast)),
            120
        );
        let swipe = acc.operations(pixels(PIXELS_PER_NOTCH), TouchPhase::Moved, now, fast);
        assert_eq!(total(&swipe), 240);
    }

    #[test]
    fn viewport_maps_scaled_positions() {
        let vp = RemoteViewport {
            rect: egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(960.0, 540.0)),
            remote_width: 1920,
            remote_height: 1080,
        };
        assert_eq!(
            vp.to_remote(egui::pos2(100.0, 50.0)),
            MousePosition { x: 0, y: 0 }
        );
        assert_eq!(
            vp.to_remote(egui::pos2(580.0, 320.0)),
            MousePosition { x: 960, y: 540 }
        );
        assert_eq!(
            vp.to_remote(egui::pos2(5000.0, -10.0)),
            MousePosition { x: 1919, y: 0 }
        );
    }
}
