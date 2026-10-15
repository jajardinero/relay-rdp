//! Physical key → RDP scancode (PC/XT set 1), keyed on the key's *position* rather than the
//! symbol it produces, so the remote computer applies its own keyboard layout exactly as if the
//! keyboard were plugged into it.

use ironrdp_input::Scancode;
use winit::keyboard::KeyCode;

const fn base(code: u8) -> Option<Scancode> {
    Some(Scancode::from_u8(false, code))
}

const fn ext(code: u8) -> Option<Scancode> {
    Some(Scancode::from_u8(true, code))
}

pub fn scancode(key: KeyCode) -> Option<Scancode> {
    use KeyCode as K;
    match key {
        K::Escape => base(0x01),
        K::Digit1 => base(0x02),
        K::Digit2 => base(0x03),
        K::Digit3 => base(0x04),
        K::Digit4 => base(0x05),
        K::Digit5 => base(0x06),
        K::Digit6 => base(0x07),
        K::Digit7 => base(0x08),
        K::Digit8 => base(0x09),
        K::Digit9 => base(0x0A),
        K::Digit0 => base(0x0B),
        K::Minus => base(0x0C),
        K::Equal => base(0x0D),
        K::Backspace => base(0x0E),
        K::Tab => base(0x0F),
        K::KeyQ => base(0x10),
        K::KeyW => base(0x11),
        K::KeyE => base(0x12),
        K::KeyR => base(0x13),
        K::KeyT => base(0x14),
        K::KeyY => base(0x15),
        K::KeyU => base(0x16),
        K::KeyI => base(0x17),
        K::KeyO => base(0x18),
        K::KeyP => base(0x19),
        K::BracketLeft => base(0x1A),
        K::BracketRight => base(0x1B),
        K::Enter => base(0x1C),
        K::ControlLeft => base(0x1D),
        K::KeyA => base(0x1E),
        K::KeyS => base(0x1F),
        K::KeyD => base(0x20),
        K::KeyF => base(0x21),
        K::KeyG => base(0x22),
        K::KeyH => base(0x23),
        K::KeyJ => base(0x24),
        K::KeyK => base(0x25),
        K::KeyL => base(0x26),
        K::Semicolon => base(0x27),
        K::Quote => base(0x28),
        K::Backquote => base(0x29),
        K::ShiftLeft => base(0x2A),
        K::Backslash => base(0x2B),
        K::KeyZ => base(0x2C),
        K::KeyX => base(0x2D),
        K::KeyC => base(0x2E),
        K::KeyV => base(0x2F),
        K::KeyB => base(0x30),
        K::KeyN => base(0x31),
        K::KeyM => base(0x32),
        K::Comma => base(0x33),
        K::Period => base(0x34),
        K::Slash => base(0x35),
        K::ShiftRight => base(0x36),
        K::NumpadMultiply => base(0x37),
        K::AltLeft => base(0x38),
        K::Space => base(0x39),
        K::CapsLock => base(0x3A),
        K::F1 => base(0x3B),
        K::F2 => base(0x3C),
        K::F3 => base(0x3D),
        K::F4 => base(0x3E),
        K::F5 => base(0x3F),
        K::F6 => base(0x40),
        K::F7 => base(0x41),
        K::F8 => base(0x42),
        K::F9 => base(0x43),
        K::F10 => base(0x44),
        K::NumLock => base(0x45),
        K::ScrollLock => base(0x46),
        K::Numpad7 => base(0x47),
        K::Numpad8 => base(0x48),
        K::Numpad9 => base(0x49),
        K::NumpadSubtract => base(0x4A),
        K::Numpad4 => base(0x4B),
        K::Numpad5 => base(0x4C),
        K::Numpad6 => base(0x4D),
        K::NumpadAdd => base(0x4E),
        K::Numpad1 => base(0x4F),
        K::Numpad2 => base(0x50),
        K::Numpad3 => base(0x51),
        K::Numpad0 => base(0x52),
        K::NumpadDecimal => base(0x53),
        K::IntlBackslash => base(0x56),
        K::F11 => base(0x57),
        K::F12 => base(0x58),
        K::NumpadEqual => base(0x59),
        K::F13 => base(0x64),
        K::F14 => base(0x65),
        K::F15 => base(0x66),
        K::F16 => base(0x67),
        K::F17 => base(0x68),
        K::F18 => base(0x69),
        K::F19 => base(0x6A),
        K::F20 => base(0x6B),
        K::F21 => base(0x6C),
        K::F22 => base(0x6D),
        K::F23 => base(0x6E),
        K::KanaMode => base(0x70),
        K::Lang2 => base(0x71),
        K::Lang1 => base(0x72),
        K::IntlRo => base(0x73),
        K::F24 => base(0x76),
        K::Convert => base(0x79),
        K::NonConvert => base(0x7B),
        K::IntlYen => base(0x7D),
        K::NumpadComma => base(0x7E),

        K::MediaTrackPrevious => ext(0x10),
        K::MediaTrackNext => ext(0x19),
        K::NumpadEnter => ext(0x1C),
        K::ControlRight => ext(0x1D),
        K::AudioVolumeMute => ext(0x20),
        K::LaunchApp2 => ext(0x21),
        K::MediaPlayPause => ext(0x22),
        K::MediaStop => ext(0x24),
        K::AudioVolumeDown => ext(0x2E),
        K::AudioVolumeUp => ext(0x30),
        K::BrowserHome => ext(0x32),
        K::NumpadDivide => ext(0x35),
        K::PrintScreen => ext(0x37),
        K::AltRight => ext(0x38),
        K::Home => ext(0x47),
        K::ArrowUp => ext(0x48),
        K::PageUp => ext(0x49),
        K::ArrowLeft => ext(0x4B),
        K::ArrowRight => ext(0x4D),
        K::End => ext(0x4F),
        K::ArrowDown => ext(0x50),
        K::PageDown => ext(0x51),
        K::Insert => ext(0x52),
        K::Delete => ext(0x53),
        K::SuperLeft => ext(0x5B),
        K::SuperRight => ext(0x5C),
        K::ContextMenu => ext(0x5D),
        K::Power => ext(0x5E),
        K::Sleep => ext(0x5F),
        K::WakeUp => ext(0x63),
        K::BrowserSearch => ext(0x65),
        K::BrowserFavorites => ext(0x66),
        K::BrowserRefresh => ext(0x67),
        K::BrowserStop => ext(0x68),
        K::BrowserForward => ext(0x69),
        K::BrowserBack => ext(0x6A),
        K::LaunchApp1 => ext(0x6B),
        K::LaunchMail => ext(0x6C),
        K::MediaSelect => ext(0x6D),

        // Pause uses the E1 prefix and is sent specially (see `input::pause_sequence`).
        _ => None,
    }
}

pub mod codes {
    use ironrdp_input::Scancode;

    pub const CONTROL_LEFT: Scancode = Scancode::from_u8(false, 0x1D);
    pub const ALT_LEFT: Scancode = Scancode::from_u8(false, 0x38);
    pub const SHIFT_LEFT: Scancode = Scancode::from_u8(false, 0x2A);
    pub const TAB: Scancode = Scancode::from_u8(false, 0x0F);
    pub const DELETE: Scancode = Scancode::from_u8(true, 0x53);
    pub const ESCAPE: Scancode = Scancode::from_u8(false, 0x01);
    pub const SUPER_LEFT: Scancode = Scancode::from_u8(true, 0x5B);
    pub const PRINT_SCREEN: Scancode = Scancode::from_u8(true, 0x37);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_keys() {
        assert_eq!(scancode(KeyCode::KeyA).unwrap().as_u16(), 0x1E);
        assert_eq!(scancode(KeyCode::Enter).unwrap().as_u16(), 0x1C);
        assert_eq!(scancode(KeyCode::NumpadEnter).unwrap().as_u16(), 0xE01C);
        assert_eq!(scancode(KeyCode::ControlRight).unwrap().as_u16(), 0xE01D);
        assert_eq!(scancode(KeyCode::SuperLeft).unwrap().as_u16(), 0xE05B);
        assert_eq!(scancode(KeyCode::ArrowUp).unwrap().as_u16(), 0xE048);
        assert_eq!(scancode(KeyCode::Numpad8).unwrap().as_u16(), 0x48);
        assert!(scancode(KeyCode::Pause).is_none());
    }

    #[test]
    fn no_two_keys_share_a_scancode() {
        let keys = [
            KeyCode::Escape,
            KeyCode::Digit1,
            KeyCode::Digit0,
            KeyCode::Minus,
            KeyCode::Equal,
            KeyCode::Backspace,
            KeyCode::Tab,
            KeyCode::KeyQ,
            KeyCode::KeyP,
            KeyCode::BracketLeft,
            KeyCode::BracketRight,
            KeyCode::Enter,
            KeyCode::ControlLeft,
            KeyCode::KeyA,
            KeyCode::KeyL,
            KeyCode::Semicolon,
            KeyCode::Quote,
            KeyCode::Backquote,
            KeyCode::ShiftLeft,
            KeyCode::Backslash,
            KeyCode::KeyZ,
            KeyCode::KeyM,
            KeyCode::Comma,
            KeyCode::Period,
            KeyCode::Slash,
            KeyCode::ShiftRight,
            KeyCode::NumpadMultiply,
            KeyCode::AltLeft,
            KeyCode::Space,
            KeyCode::CapsLock,
            KeyCode::F1,
            KeyCode::F10,
            KeyCode::F11,
            KeyCode::F12,
            KeyCode::NumLock,
            KeyCode::ScrollLock,
            KeyCode::Numpad0,
            KeyCode::Numpad9,
            KeyCode::NumpadSubtract,
            KeyCode::NumpadAdd,
            KeyCode::NumpadDecimal,
            KeyCode::IntlBackslash,
            KeyCode::NumpadEnter,
            KeyCode::ControlRight,
            KeyCode::NumpadDivide,
            KeyCode::PrintScreen,
            KeyCode::AltRight,
            KeyCode::Home,
            KeyCode::ArrowUp,
            KeyCode::PageUp,
            KeyCode::ArrowLeft,
            KeyCode::ArrowRight,
            KeyCode::End,
            KeyCode::ArrowDown,
            KeyCode::PageDown,
            KeyCode::Insert,
            KeyCode::Delete,
            KeyCode::SuperLeft,
            KeyCode::SuperRight,
            KeyCode::ContextMenu,
        ];
        let mut seen = std::collections::HashSet::new();
        for key in keys {
            let code = scancode(key)
                .unwrap_or_else(|| panic!("{key:?} unmapped"))
                .as_u16();
            assert!(seen.insert(code), "{key:?} reuses scancode {code:#06x}");
        }
    }
}
