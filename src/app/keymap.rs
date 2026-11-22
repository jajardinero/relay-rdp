//! Keystroke -> RDP scancode (PC/XT set 1), plus modifier scancode constants.

use ironrdp_input::Scancode;

const fn base(code: u8) -> Option<Scancode> {
    Some(Scancode::from_u8(false, code))
}

const fn ext(code: u8) -> Option<Scancode> {
    Some(Scancode::from_u8(true, code))
}

/// Maps a GPUI keystroke `key` string (from `KeyDownEvent`/`KeyUpEvent`) to a PC/XT Set 1 scancode.
///
/// Shifted symbols map back to their physical key because Shift is already tracked via
/// `ModifiersChangedEvent`.
pub fn scancode(key: &str) -> Option<Scancode> {
    match key.to_ascii_lowercase().as_str() {
        "escape" => base(0x01),
        "1" | "!" => base(0x02),
        "2" | "@" => base(0x03),
        "3" | "#" => base(0x04),
        "4" | "$" => base(0x05),
        "5" | "%" => base(0x06),
        "6" | "^" => base(0x07),
        "7" | "&" => base(0x08),
        "8" | "*" => base(0x09),
        "9" | "(" => base(0x0A),
        "0" | ")" => base(0x0B),
        "-" | "_" => base(0x0C),
        "=" | "+" => base(0x0D),
        "backspace" => base(0x0E),
        "tab" => base(0x0F),
        "q" => base(0x10),
        "w" => base(0x11),
        "e" => base(0x12),
        "r" => base(0x13),
        "t" => base(0x14),
        "y" => base(0x15),
        "u" => base(0x16),
        "i" => base(0x17),
        "o" => base(0x18),
        "p" => base(0x19),
        "[" | "{" => base(0x1A),
        "]" | "}" => base(0x1B),
        "enter" | "return" => base(0x1C),
        "control_l" | "control" => base(0x1D),
        "a" => base(0x1E),
        "s" => base(0x1F),
        "d" => base(0x20),
        "f" => base(0x21),
        "g" => base(0x22),
        "h" => base(0x23),
        "j" => base(0x24),
        "k" => base(0x25),
        "l" => base(0x26),
        ";" | ":" => base(0x27),
        "'" | "\"" => base(0x28),
        "`" | "~" => base(0x29),
        "shift_l" | "shift" => base(0x2A),
        "\\" | "|" => base(0x2B),
        "z" => base(0x2C),
        "x" => base(0x2D),
        "c" => base(0x2E),
        "v" => base(0x2F),
        "b" => base(0x30),
        "n" => base(0x31),
        "m" => base(0x32),
        "," | "<" => base(0x33),
        "." | ">" => base(0x34),
        "/" | "?" => base(0x35),
        "shift_r" => base(0x36),
        "multiply" => base(0x37),
        "alt_l" | "alt" => base(0x38),
        "space" | " " => base(0x39),
        "caps_lock" | "capslock" => base(0x3A),
        "f1" => base(0x3B),
        "f2" => base(0x3C),
        "f3" => base(0x3D),
        "f4" => base(0x3E),
        "f5" => base(0x3F),
        "f6" => base(0x40),
        "f7" => base(0x41),
        "f8" => base(0x42),
        "f9" => base(0x43),
        "f10" => base(0x44),
        "num_lock" | "numlock" => base(0x45),
        "scroll_lock" | "scrolllock" => base(0x46),
        "subtract" => base(0x4A),
        "add" => base(0x4E),
        "decimal" => base(0x53),
        "f11" => base(0x57),
        "f12" => base(0x58),
        "f13" => base(0x64),
        "f14" => base(0x65),
        "f15" => base(0x66),
        "f16" => base(0x67),
        "f17" => base(0x68),
        "f18" => base(0x69),
        "f19" => base(0x6A),
        "f20" => base(0x6B),
        "f21" => base(0x6C),
        "f22" => base(0x6D),
        "f23" => base(0x6E),
        "f24" => base(0x76),

        "control_r" => ext(0x1D),
        "divide" => ext(0x35),
        "print" | "printscreen" => ext(0x37),
        "alt_r" | "iso_level3_shift" => ext(0x38),
        "home" => ext(0x47),
        "up" => ext(0x48),
        "pageup" => ext(0x49),
        "left" => ext(0x4B),
        "right" => ext(0x4D),
        "end" => ext(0x4F),
        "down" => ext(0x50),
        "pagedown" => ext(0x51),
        "insert" => ext(0x52),
        "delete" => ext(0x53),
        "super_l" | "meta_l" => ext(0x5B),
        "super_r" | "meta_r" => ext(0x5C),
        "menu" => ext(0x5D),
        "xf86audioprev" => ext(0x10),
        "xf86audionext" => ext(0x19),
        "xf86audiomute" => ext(0x20),
        "xf86audioplay" => ext(0x22),
        "xf86audiostop" => ext(0x24),
        "xf86audiolowervolume" => ext(0x2E),
        "xf86audioraisevolume" => ext(0x30),
        "back" => ext(0x6A),
        "forward" => ext(0x69),

        // Pause uses the E1 prefix and is sent specially (see `input::pause_sequence`).
        _ => None,
    }
}

pub mod codes {
    use ironrdp_input::Scancode;

    pub const CONTROL_LEFT: Scancode = Scancode::from_u8(false, 0x1D);
    pub const ALT_LEFT: Scancode = Scancode::from_u8(false, 0x38);
    pub const SHIFT_LEFT: Scancode = Scancode::from_u8(false, 0x2A);
    pub const CAPS_LOCK: Scancode = Scancode::from_u8(false, 0x3A);
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
        assert_eq!(scancode("a").unwrap().as_u16(), 0x1E);
        assert_eq!(scancode("A").unwrap().as_u16(), 0x1E);
        assert_eq!(scancode("enter").unwrap().as_u16(), 0x1C);
        assert_eq!(scancode("up").unwrap().as_u16(), 0xE048);
        assert_eq!(scancode("home").unwrap().as_u16(), 0xE047);
        assert_eq!(scancode("delete").unwrap().as_u16(), 0xE053);
        assert_eq!(scancode("!").unwrap().as_u16(), 0x02);
        assert!(scancode("pause").is_none());
    }

    #[test]
    fn no_two_primary_keys_share_a_scancode() {
        let keys = [
            "escape", "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "-", "=", "backspace",
            "tab", "q", "w", "e", "r", "t", "y", "u", "i", "o", "p", "[", "]", "enter", "a", "s",
            "d", "f", "g", "h", "j", "k", "l", ";", "'", "`", "\\", "z", "x", "c", "v", "b", "n",
            "m", ",", ".", "/", "space", "capslock", "f1", "f2", "f3", "f4", "f5", "f6", "f7",
            "f8", "f9", "f10", "f11", "f12", "numlock", "scrolllock", "print", "home", "up",
            "pageup", "left", "right", "end", "down", "pagedown", "insert", "delete", "menu",
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
