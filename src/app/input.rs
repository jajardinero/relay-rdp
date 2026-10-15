//! Translating local pointer and wheel input into RDP input operations.

use ironrdp_input::{MouseButton as RdpButton, MousePosition, Operation, WheelRotations};
use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use smallvec::SmallVec;
use winit::event::{MouseButton, MouseScrollDelta};

/// Rotation units for one wheel notch.
const NOTCH: f64 = 120.0;
/// Touchpads report pixels; this many pixels count as one notch.
const PIXELS_PER_NOTCH: f64 = 50.0;
/// The wire format holds a 9-bit signed value.
const MAX_UNITS_PER_EVENT: i16 = 255;

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

/// Accumulates fractional scrolling (touchpads) so slow two-finger scrolls still arrive.
#[derive(Default)]
pub struct WheelAccumulator {
    vertical: f64,
    horizontal: f64,
}

impl WheelAccumulator {
    pub fn operations(&mut self, delta: MouseScrollDelta) -> SmallVec<[Operation; 4]> {
        let (dx, dy) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (f64::from(x) * NOTCH, f64::from(y) * NOTCH),
            MouseScrollDelta::PixelDelta(p) => (
                p.x * NOTCH / PIXELS_PER_NOTCH,
                p.y * NOTCH / PIXELS_PER_NOTCH,
            ),
        };
        // winit: positive y scrolls content up (wheel away from the user), matching RDP.
        // Horizontal is mirrored: positive x in winit moves content right, i.e. scrolls left.
        self.vertical += dy;
        self.horizontal -= dx;

        let mut ops = SmallVec::new();
        push_rotations(&mut ops, &mut self.vertical, true);
        push_rotations(&mut ops, &mut self.horizontal, false);
        ops
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

    #[test]
    fn line_scroll_is_one_notch() {
        let mut acc = WheelAccumulator::default();
        let ops = acc.operations(MouseScrollDelta::LineDelta(0.0, 1.0));
        assert_eq!(wheel_units(&ops), vec![(true, 120)]);
        let ops = acc.operations(MouseScrollDelta::LineDelta(0.0, -1.0));
        assert_eq!(wheel_units(&ops), vec![(true, -120)]);
    }

    #[test]
    fn large_scrolls_are_split() {
        let mut acc = WheelAccumulator::default();
        let ops = acc.operations(MouseScrollDelta::LineDelta(0.0, 3.0));
        assert_eq!(wheel_units(&ops), vec![(true, 255), (true, 105)]);
    }

    #[test]
    fn small_pixel_scrolls_accumulate() {
        let mut acc = WheelAccumulator::default();
        let tiny = MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, 0.2));
        assert!(acc.operations(tiny).is_empty());
        let mut total = 0;
        for _ in 0..10 {
            total += wheel_units(&acc.operations(tiny))
                .iter()
                .map(|(_, u)| i32::from(*u))
                .sum::<i32>();
        }
        assert!(total > 0);
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
