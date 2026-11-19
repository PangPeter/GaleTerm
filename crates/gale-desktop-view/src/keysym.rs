//! Map gpui keystrokes to X11 keysyms (what RFB KeyEvent expects).

use gpui_kit::Keystroke;

/// Best-effort keysym for `keystroke`. Returns `None` for modifiers-only.
pub fn keysym_from_keystroke(keystroke: &Keystroke) -> Option<u32> {
    let key = keystroke.key.as_str();
    let mapped = match key {
        "return" | "enter" => 0xff0d,
        "tab" => 0xff09,
        "escape" => 0xff1b,
        "backspace" => 0xff08,
        "delete" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" => 0xff55,
        "pagedown" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "space" => 0x0020,
        "f1" => 0xffbe,
        "f2" => 0xffbf,
        "f3" => 0xffc0,
        "f4" => 0xffc1,
        "f5" => 0xffc2,
        "f6" => 0xffc3,
        "f7" => 0xffc4,
        "f8" => 0xffc5,
        "f9" => 0xffc6,
        "f10" => 0xffc7,
        "f11" => 0xffc8,
        "f12" => 0xffc9,
        "shift" => 0xffe1,
        "control" | "ctrl" => 0xffe3,
        "alt" | "option" => 0xffe9,
        "meta" | "cmd" | "super" | "win" => 0xffe7,
        "caps_lock" => 0xffe5,
        other if other.chars().count() == 1 => {
            let mut ch = other.chars().next()?;
            if keystroke.modifiers.shift {
                // gpui already gives the shifted glyph for printable keys when
                // shift is held; keep as-is. For letters that stayed lower, upcase.
                if ch.is_ascii_lowercase() {
                    ch = ch.to_ascii_uppercase();
                }
            }
            return Some(u32::from(ch));
        }
        _ => return None,
    };
    Some(mapped)
}
