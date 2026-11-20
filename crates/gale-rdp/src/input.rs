//! Map desktop-pane events (RFB button mask + X11 keysyms) to IronRDP input ops.

use ironrdp::pdu::input::fast_path::FastPathInputEvent;
use ironrdp_input::{
    Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations,
};

use crate::client::RdpInput;

/// Tracks button / key state so we only emit edge transitions IronRDP expects.
pub(crate) struct InputMapper {
    db: Database,
    buttons: u8,
}

impl InputMapper {
    pub fn new() -> Self {
        Self {
            db: Database::new(),
            buttons: 0,
        }
    }

    pub fn map(&mut self, input: &RdpInput) -> Vec<FastPathInputEvent> {
        let ops = match input {
            RdpInput::Pointer { x, y, buttons } => pointer_ops(&mut self.buttons, *x, *y, *buttons),
            RdpInput::Key { down, keysym } => key_ops(*down, *keysym),
            // Prefer one-char chunks via `map_paste_char` in the session loop.
            RdpInput::CutText(text) => text.chars().flat_map(paste_char_ops).collect(),
            RdpInput::CutImage(_) | RdpInput::Refresh | RdpInput::Close => Vec::new(),
        };
        if ops.is_empty() {
            return Vec::new();
        }
        self.db.apply(ops).into_vec()
    }

    /// Type a single clipboard character (scancode-first for xrdp compatibility).
    pub fn map_paste_char(&mut self, ch: char) -> Vec<FastPathInputEvent> {
        let ops = paste_char_ops(ch);
        if ops.is_empty() {
            return Vec::new();
        }
        self.db.apply(ops).into_vec()
    }
}

fn paste_char_ops(ch: char) -> Vec<Operation> {
    match ch {
        '\n' | '\r' => tap(Scancode::from_u8(false, 0x1c), false),
        '\t' => tap(Scancode::from_u8(false, 0x0f), false),
        ch => {
            if let Some((code, shift)) = us_scancode_table(ch) {
                tap(Scancode::from_u8(false, code), shift)
            } else {
                // Non-ASCII: Unicode events (Windows RDP); xrdp may ignore these.
                vec![
                    Operation::UnicodeKeyPressed(ch),
                    Operation::UnicodeKeyReleased(ch),
                ]
            }
        }
    }
}

fn tap(scancode: Scancode, shift: bool) -> Vec<Operation> {
    let shift_sc = Scancode::from_u8(false, 0x2a);
    let mut ops = Vec::with_capacity(4);
    if shift {
        ops.push(Operation::KeyPressed(shift_sc));
    }
    ops.push(Operation::KeyPressed(scancode));
    ops.push(Operation::KeyReleased(scancode));
    if shift {
        ops.push(Operation::KeyReleased(shift_sc));
    }
    ops
}

/// US QWERTY set-1 scancode + whether Shift is required.
fn us_scancode_table(ch: char) -> Option<(u8, bool)> {
    // Letter scancodes are not sequential; use an explicit map.
    const LETTERS: [(char, u8); 26] = [
        ('a', 0x1e),
        ('b', 0x30),
        ('c', 0x2e),
        ('d', 0x20),
        ('e', 0x12),
        ('f', 0x21),
        ('g', 0x22),
        ('h', 0x23),
        ('i', 0x17),
        ('j', 0x24),
        ('k', 0x25),
        ('l', 0x26),
        ('m', 0x32),
        ('n', 0x31),
        ('o', 0x18),
        ('p', 0x19),
        ('q', 0x10),
        ('r', 0x13),
        ('s', 0x1f),
        ('t', 0x14),
        ('u', 0x16),
        ('v', 0x2f),
        ('w', 0x11),
        ('x', 0x2d),
        ('y', 0x15),
        ('z', 0x2c),
    ];
    if ch.is_ascii_alphabetic() {
        let lower = ch.to_ascii_lowercase();
        let code = LETTERS.iter().find(|(c, _)| *c == lower)?.1;
        return Some((code, ch.is_ascii_uppercase()));
    }
    match ch {
        '1' => Some((0x02, false)),
        '2' => Some((0x03, false)),
        '3' => Some((0x04, false)),
        '4' => Some((0x05, false)),
        '5' => Some((0x06, false)),
        '6' => Some((0x07, false)),
        '7' => Some((0x08, false)),
        '8' => Some((0x09, false)),
        '9' => Some((0x0a, false)),
        '0' => Some((0x0b, false)),
        '!' => Some((0x02, true)),
        '@' => Some((0x03, true)),
        '#' => Some((0x04, true)),
        '$' => Some((0x05, true)),
        '%' => Some((0x06, true)),
        '^' => Some((0x07, true)),
        '&' => Some((0x08, true)),
        '*' => Some((0x09, true)),
        '(' => Some((0x0a, true)),
        ')' => Some((0x0b, true)),
        '-' => Some((0x0c, false)),
        '_' => Some((0x0c, true)),
        '=' => Some((0x0d, false)),
        '+' => Some((0x0d, true)),
        '[' => Some((0x1a, false)),
        '{' => Some((0x1a, true)),
        ']' => Some((0x1b, false)),
        '}' => Some((0x1b, true)),
        ';' => Some((0x27, false)),
        ':' => Some((0x27, true)),
        '\'' => Some((0x28, false)),
        '"' => Some((0x28, true)),
        '`' => Some((0x29, false)),
        '~' => Some((0x29, true)),
        '\\' => Some((0x2b, false)),
        '|' => Some((0x2b, true)),
        ',' => Some((0x33, false)),
        '<' => Some((0x33, true)),
        '.' => Some((0x34, false)),
        '>' => Some((0x34, true)),
        '/' => Some((0x35, false)),
        '?' => Some((0x35, true)),
        ' ' => Some((0x39, false)),
        _ => None,
    }
}

fn pointer_ops(prev: &mut u8, x: u16, y: u16, buttons: u8) -> Vec<Operation> {
    let mut ops = vec![Operation::MouseMove(MousePosition { x, y })];
    for (bit, button) in [
        (1u8 << 0, MouseButton::Left),
        (1 << 1, MouseButton::Middle),
        (1 << 2, MouseButton::Right),
    ] {
        let was = *prev & bit != 0;
        let now = buttons & bit != 0;
        if !was && now {
            ops.push(Operation::MouseButtonPressed(button));
        } else if was && !now {
            ops.push(Operation::MouseButtonReleased(button));
        }
    }
    let wheel_up = buttons & (1 << 3) != 0;
    let wheel_down = buttons & (1 << 4) != 0;
    if wheel_up {
        ops.push(Operation::WheelRotations(WheelRotations {
            is_vertical: true,
            rotation_units: 1,
        }));
    }
    if wheel_down {
        ops.push(Operation::WheelRotations(WheelRotations {
            is_vertical: true,
            rotation_units: -1,
        }));
    }
    *prev = buttons & 0b111; // sticky buttons only; wheel is pulse
    ops
}

fn key_ops(down: bool, keysym: u32) -> Vec<Operation> {
    if let Some(scancode) = scancode_from_keysym(keysym) {
        return vec![if down {
            Operation::KeyPressed(scancode)
        } else {
            Operation::KeyReleased(scancode)
        }];
    }
    if let Some(ch) = char_from_keysym(keysym) {
        // Prefer scancodes for ASCII so xrdp sees real key events.
        if let Some((code, shift)) = us_scancode_table(ch) {
            let sc = Scancode::from_u8(false, code);
            if down {
                let mut ops = Vec::new();
                if shift {
                    ops.push(Operation::KeyPressed(Scancode::from_u8(false, 0x2a)));
                }
                ops.push(Operation::KeyPressed(sc));
                return ops;
            }
            let mut ops = vec![Operation::KeyReleased(sc)];
            if shift {
                ops.push(Operation::KeyReleased(Scancode::from_u8(false, 0x2a)));
            }
            return ops;
        }
        return vec![if down {
            Operation::UnicodeKeyPressed(ch)
        } else {
            Operation::UnicodeKeyReleased(ch)
        }];
    }
    Vec::new()
}

fn char_from_keysym(keysym: u32) -> Option<char> {
    if (0x20..0x7f).contains(&keysym) || (0xa0..0x0100).contains(&keysym) {
        char::from_u32(keysym)
    } else if (0x0100_0000..0x0110_0000).contains(&keysym) {
        char::from_u32(keysym - 0x0100_0000)
    } else {
        None
    }
}

fn scancode_from_keysym(keysym: u32) -> Option<Scancode> {
    let (extended, code) = match keysym {
        0xff08 => (false, 0x0e), // BackSpace
        0xff09 => (false, 0x0f), // Tab
        0xff0d => (false, 0x1c), // Return
        0xff1b => (false, 0x01), // Escape
        0xff50 => (true, 0x47),  // Home
        0xff51 => (true, 0x4b),  // Left
        0xff52 => (true, 0x48),  // Up
        0xff53 => (true, 0x4d),  // Right
        0xff54 => (true, 0x50),  // Down
        0xff55 => (true, 0x49),  // Page_Up
        0xff56 => (true, 0x51),  // Page_Down
        0xff57 => (true, 0x4f),  // End
        0xff63 => (true, 0x52),  // Insert
        0xffff => (true, 0x53),  // Delete
        0xffe1 | 0xffe2 => (false, 0x2a), // Shift_L / Shift_R → LShift
        0xffe3 => (false, 0x1d), // Control_L
        0xffe4 => (true, 0x1d),  // Control_R
        0xffe9 => (false, 0x38), // Alt_L
        0xffea => (true, 0x38),  // Alt_R
        0xffe7 | 0xffe8 | 0xffeb | 0xffec => (true, 0x5b), // Super / Meta → LWin
        0xffe5 => (false, 0x3a), // Caps_Lock
        0x0020 => (false, 0x39), // space
        0xffbe..=0xffc9 => {
            let idx = (keysym - 0xffbe) as u8;
            (false, 0x3b + idx)
        }
        _ => return None,
    };
    Some(Scancode::from_u8(extended, code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_hello_uses_scancodes() {
        let mut mapper = InputMapper::new();
        let events = mapper.map(&RdpInput::CutText("Hi!".into()));
        assert!(!events.is_empty());
        // Should not be empty even on xrdp-style servers that ignore Unicode.
        assert!(events.len() >= 6);
    }

    #[test]
    fn us_letters_map() {
        assert_eq!(us_scancode_table('a'), Some((0x1e, false)));
        assert_eq!(us_scancode_table('A'), Some((0x1e, true)));
        assert_eq!(us_scancode_table('!'), Some((0x02, true)));
    }
}
