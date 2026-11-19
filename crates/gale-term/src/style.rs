//! Styled grid export: consecutive cells merged into runs with resolved RGB.

use crate::palette::{self, CellRgb, DEFAULT_BG, DEFAULT_FG};

/// Resolved style for a contiguous run of cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellStyle {
    pub fg: CellRgb,
    pub bg: CellRgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

impl CellStyle {
    pub const fn plain() -> Self {
        Self {
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
        }
    }

    pub fn is_default_chrome(self) -> bool {
        self == Self::plain()
    }
}

/// One contiguous styled span within a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledRun {
    pub text: String,
    pub style: CellStyle,
}

/// One visible screen row as merged runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StyledLine {
    pub runs: Vec<StyledRun>,
}

impl StyledLine {
    pub fn is_visually_empty(&self) -> bool {
        self.runs.iter().all(|run| {
            run.style.is_default_chrome() && run.text.chars().all(|c| c == ' ')
        })
    }

    pub fn plain_text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }
}

/// Push `ch` into `runs`, merging with the last run when styles match.
pub fn push_styled_char(runs: &mut Vec<StyledRun>, ch: char, style: CellStyle) {
    if let Some(last) = runs.last_mut() {
        if last.style == style {
            last.text.push(ch);
            return;
        }
    }
    runs.push(StyledRun {
        text: ch.to_string(),
        style,
    });
}

/// Append zero-width / combining characters onto the last run (or start one).
pub fn push_zerowidth(runs: &mut Vec<StyledRun>, ch: char, style: CellStyle) {
    if let Some(last) = runs.last_mut() {
        if last.style == style {
            last.text.push(ch);
            return;
        }
    }
    runs.push(StyledRun {
        text: ch.to_string(),
        style,
    });
}

/// Drop trailing space runs that use the default chrome colors.
pub fn trim_default_trailing_spaces(runs: &mut Vec<StyledRun>) {
    loop {
        let Some(last) = runs.last() else {
            break;
        };
        if !last.style.is_default_chrome() {
            break;
        }
        if last.text.chars().all(|c| c == ' ') {
            runs.pop();
            continue;
        }
        let keep = last.text.trim_end_matches(' ').len();
        if keep == last.text.len() {
            break;
        }
        if keep == 0 {
            runs.pop();
            continue;
        }
        runs.last_mut().expect("last run").text.truncate(keep);
        break;
    }
}

/// Helper used by tests / headless: RGB of an SGR truecolor / indexed cell.
pub fn resolve_cell_colors(
    fg: alacritty_terminal::vte::ansi::Color,
    bg: alacritty_terminal::vte::ansi::Color,
    flags: alacritty_terminal::term::cell::Flags,
    term_colors: &alacritty_terminal::term::color::Colors,
) -> CellStyle {
    use alacritty_terminal::term::cell::Flags;

    let bold = flags.intersects(Flags::BOLD | Flags::BOLD_ITALIC);
    let italic = flags.intersects(Flags::ITALIC | Flags::BOLD_ITALIC);
    let underline = flags.intersects(Flags::ALL_UNDERLINES);
    let strikethrough = flags.contains(Flags::STRIKEOUT);
    let dim = flags.contains(Flags::DIM) || flags.contains(Flags::DIM_BOLD);

    let mut fg = palette::resolve_color(fg, term_colors, bold && !dim);
    let mut bg = palette::resolve_color(bg, term_colors, false);

    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if dim {
        fg = palette::dim_toward(fg, bg);
    }

    CellStyle {
        fg,
        bg,
        bold,
        italic,
        underline,
        strikethrough,
    }
}
