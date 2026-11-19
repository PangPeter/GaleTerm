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
            RdpInput::CutText(_) | RdpInput::Refresh | RdpInput::Close => Vec::new(),
        };
        if ops.is_empty() {
            return Vec::new();
        }
        self.db.apply(ops).into_vec()
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
            // F1..F12
            let idx = (keysym - 0xffbe) as u8;
            (false, 0x3b + idx)
        }
        _ => return None,
    };
    Some(Scancode::from_u8(extended, code))
}
