//! Resolve alacritty/vte colors to RGB for rendering.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// 8-bit RGB used by the styled grid export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl CellRgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub const fn from_u32(rgb: u32) -> Self {
        Self {
            r: ((rgb >> 16) & 0xff) as u8,
            g: ((rgb >> 8) & 0xff) as u8,
            b: (rgb & 0xff) as u8,
        }
    }
}

impl From<Rgb> for CellRgb {
    fn from(rgb: Rgb) -> Self {
        Self {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        }
    }
}

/// Default terminal colors when the cell uses Named Foreground/Background.
/// Hosts should paint the pane with `DEFAULT_BG` so default cells blend in.
pub const DEFAULT_FG: CellRgb = CellRgb::from_u32(0xe8_ea_f0);
pub const DEFAULT_BG: CellRgb = CellRgb::from_u32(0x14_17_1f);

/// Classic ANSI 16 + xterm 256 cube/gray ramp.
pub fn indexed_color(index: u8) -> CellRgb {
    match index {
        0 => CellRgb::from_u32(0x00_00_00),
        1 => CellRgb::from_u32(0xcd_00_00),
        2 => CellRgb::from_u32(0x00_cd_00),
        3 => CellRgb::from_u32(0xcd_cd_00),
        4 => CellRgb::from_u32(0x00_00_ee),
        5 => CellRgb::from_u32(0xcd_00_cd),
        6 => CellRgb::from_u32(0x00_cd_cd),
        7 => CellRgb::from_u32(0xe5_e5_e5),
        8 => CellRgb::from_u32(0x7f_7f_7f),
        9 => CellRgb::from_u32(0xff_00_00),
        10 => CellRgb::from_u32(0x00_ff_00),
        11 => CellRgb::from_u32(0xff_ff_00),
        12 => CellRgb::from_u32(0x5c_5c_ff),
        13 => CellRgb::from_u32(0xff_00_ff),
        14 => CellRgb::from_u32(0x00_ff_ff),
        15 => CellRgb::from_u32(0xff_ff_ff),
        16..=231 => {
            let i = index - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            let ramp = |c: u8| -> u8 {
                if c == 0 {
                    0
                } else {
                    55 + 40 * c
                }
            };
            CellRgb::new(ramp(r), ramp(g), ramp(b))
        }
        232..=255 => {
            let level = 8 + 10 * (index - 232);
            CellRgb::new(level, level, level)
        }
    }
}

fn named_color(name: NamedColor) -> CellRgb {
    match name {
        NamedColor::Black => indexed_color(0),
        NamedColor::Red => indexed_color(1),
        NamedColor::Green => indexed_color(2),
        NamedColor::Yellow => indexed_color(3),
        NamedColor::Blue => indexed_color(4),
        NamedColor::Magenta => indexed_color(5),
        NamedColor::Cyan => indexed_color(6),
        NamedColor::White => indexed_color(7),
        NamedColor::BrightBlack => indexed_color(8),
        NamedColor::BrightRed => indexed_color(9),
        NamedColor::BrightGreen => indexed_color(10),
        NamedColor::BrightYellow => indexed_color(11),
        NamedColor::BrightBlue => indexed_color(12),
        NamedColor::BrightMagenta => indexed_color(13),
        NamedColor::BrightCyan => indexed_color(14),
        NamedColor::BrightWhite => indexed_color(15),
        NamedColor::Foreground | NamedColor::BrightForeground => DEFAULT_FG,
        NamedColor::Background => DEFAULT_BG,
        NamedColor::Cursor => DEFAULT_FG,
        NamedColor::DimForeground => CellRgb::from_u32(0x8b_93_9e),
        NamedColor::DimBlack => CellRgb::from_u32(0x1a_1a_1a),
        NamedColor::DimRed => CellRgb::from_u32(0x7f_00_00),
        NamedColor::DimGreen => CellRgb::from_u32(0x00_7f_00),
        NamedColor::DimYellow => CellRgb::from_u32(0x7f_7f_00),
        NamedColor::DimBlue => CellRgb::from_u32(0x00_00_7f),
        NamedColor::DimMagenta => CellRgb::from_u32(0x7f_00_7f),
        NamedColor::DimCyan => CellRgb::from_u32(0x00_7f_7f),
        NamedColor::DimWhite => CellRgb::from_u32(0x7f_7f_7f),
    }
}

/// Resolve a cell color, consulting OSC-overridden terminal palette first.
pub fn resolve_color(color: Color, term_colors: &Colors, bold: bool) -> CellRgb {
    match color {
        Color::Named(mut named) => {
            if bold {
                named = named.to_bright();
            }
            if let Some(rgb) = term_colors[named] {
                return rgb.into();
            }
            named_color(named)
        }
        Color::Spec(rgb) => rgb.into(),
        Color::Indexed(idx) => {
            if let Some(rgb) = term_colors[usize::from(idx)] {
                return rgb.into();
            }
            // Bold on 0–7 often means bright 8–15.
            let idx = if bold && idx < 8 { idx + 8 } else { idx };
            indexed_color(idx)
        }
    }
}

/// Blend `fg` toward `bg` for DIM attribute.
pub fn dim_toward(fg: CellRgb, bg: CellRgb) -> CellRgb {
    CellRgb::new(
        ((u16::from(fg.r) + u16::from(bg.r)) / 2) as u8,
        ((u16::from(fg.g) + u16::from(bg.g)) / 2) as u8,
        ((u16::from(fg.b) + u16::from(bg.b)) / 2) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_and_gray() {
        assert_eq!(indexed_color(16), CellRgb::new(0, 0, 0));
        assert_eq!(indexed_color(231), CellRgb::new(255, 255, 255));
        assert_eq!(indexed_color(232), CellRgb::new(8, 8, 8));
        assert_eq!(indexed_color(255), CellRgb::new(238, 238, 238));
    }
}
