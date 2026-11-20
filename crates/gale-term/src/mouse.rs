//! Encode mouse events for terminal mouse reporting (X10 / SGR).
//!
//! Modes come from DECRQM private modes 1000/1002/1003 + 1006 (SGR).

use crate::screen::InputModes;

/// Which mouse button / wheel edge is being reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButtonKind {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

/// Press, release, or motion while a button is held (or any motion in 1003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    /// Drag / motion; `button` is the held button, or `None` for 1003 free motion.
    Motion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Default for MouseModifiers {
    fn default() -> Self {
        Self {
            shift: false,
            alt: false,
            ctrl: false,
        }
    }
}

/// Encode one mouse report. Coordinates are **0-based** cell indices.
/// Returns `None` if mouse reporting is off or the event should be ignored.
pub fn encode_mouse(
    modes: InputModes,
    action: MouseAction,
    button: Option<MouseButtonKind>,
    col: usize,
    row: usize,
    mods: MouseModifiers,
) -> Option<Vec<u8>> {
    if !modes.mouse_click && !modes.mouse_drag && !modes.mouse_motion {
        return None;
    }

    // Shift is reserved for local selection override (caller should not call us).
    if mods.shift {
        return None;
    }

    match action {
        MouseAction::Press | MouseAction::Release => {
            if !modes.mouse_click && !modes.mouse_drag && !modes.mouse_motion {
                return None;
            }
        }
        MouseAction::Motion => {
            if modes.mouse_motion {
                // 1003: all motion
            } else if modes.mouse_drag {
                // 1002: only while a button is held
                if button.is_none() {
                    return None;
                }
            } else {
                return None;
            }
        }
    }

    let mut cb = match (action, button) {
        (MouseAction::Release, _) if !modes.sgr_mouse => 3, // X10 release
        (MouseAction::Release, Some(MouseButtonKind::Left)) => 0,
        (MouseAction::Release, Some(MouseButtonKind::Middle)) => 1,
        (MouseAction::Release, Some(MouseButtonKind::Right)) => 2,
        (MouseAction::Release, _) => 0,
        (_, Some(MouseButtonKind::Left)) => 0,
        (_, Some(MouseButtonKind::Middle)) => 1,
        (_, Some(MouseButtonKind::Right)) => 2,
        (_, Some(MouseButtonKind::WheelUp)) => 64,
        (_, Some(MouseButtonKind::WheelDown)) => 65,
        (MouseAction::Motion, None) => 3, // 1003 free motion
        _ => return None,
    };

    if matches!(action, MouseAction::Motion) && button.is_some() {
        // Drag motion: button code + 32
        if cb < 64 {
            cb += 32;
        }
    }

    if mods.shift {
        cb += 4;
    }
    if mods.alt {
        cb += 8;
    }
    if mods.ctrl {
        cb += 16;
    }

    // Terminal coords are 1-based.
    let cx = col.saturating_add(1);
    let cy = row.saturating_add(1);

    if modes.sgr_mouse {
        let suffix = if matches!(action, MouseAction::Release) {
            b'm'
        } else {
            b'M'
        };
        let mut out = format!("\x1b[<{cb};{cx};{cy}").into_bytes();
        out.push(suffix);
        Some(out)
    } else {
        // X10: limited to cols/rows that fit in a byte after +32.
        if cx > 223 || cy > 223 {
            return None;
        }
        Some(vec![
            0x1b,
            b'[',
            b'M',
            (cb as u8).saturating_add(32),
            (cx as u8).saturating_add(32),
            (cy as u8).saturating_add(32),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sgr_modes() -> InputModes {
        InputModes {
            mouse_click: true,
            sgr_mouse: true,
            ..InputModes::default()
        }
    }

    #[test]
    fn sgr_left_press_release() {
        let modes = sgr_modes();
        let press = encode_mouse(
            modes,
            MouseAction::Press,
            Some(MouseButtonKind::Left),
            0,
            0,
            MouseModifiers::default(),
        )
        .unwrap();
        assert_eq!(press, b"\x1b[<0;1;1M");
        let release = encode_mouse(
            modes,
            MouseAction::Release,
            Some(MouseButtonKind::Left),
            4,
            2,
            MouseModifiers::default(),
        )
        .unwrap();
        assert_eq!(release, b"\x1b[<0;5;3m");
    }

    #[test]
    fn x10_left_press() {
        let modes = InputModes {
            mouse_click: true,
            ..InputModes::default()
        };
        let press = encode_mouse(
            modes,
            MouseAction::Press,
            Some(MouseButtonKind::Left),
            0,
            0,
            MouseModifiers::default(),
        )
        .unwrap();
        assert_eq!(press, b"\x1b[M !!");
    }

    #[test]
    fn drag_requires_1002() {
        let click_only = InputModes {
            mouse_click: true,
            sgr_mouse: true,
            ..InputModes::default()
        };
        assert!(encode_mouse(
            click_only,
            MouseAction::Motion,
            Some(MouseButtonKind::Left),
            1,
            1,
            MouseModifiers::default(),
        )
        .is_none());

        let drag = InputModes {
            mouse_drag: true,
            sgr_mouse: true,
            ..InputModes::default()
        };
        let bytes = encode_mouse(
            drag,
            MouseAction::Motion,
            Some(MouseButtonKind::Left),
            1,
            1,
            MouseModifiers::default(),
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[<32;2;2M");
    }

    #[test]
    fn wheel_sgr() {
        let modes = sgr_modes();
        let up = encode_mouse(
            modes,
            MouseAction::Press,
            Some(MouseButtonKind::WheelUp),
            0,
            0,
            MouseModifiers::default(),
        )
        .unwrap();
        assert_eq!(up, b"\x1b[<64;1;1M");
    }
}
