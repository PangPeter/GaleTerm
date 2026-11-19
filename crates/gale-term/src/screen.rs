//! Terminal emulation backed by `alacritty_terminal`.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;

use crate::style::{
    push_styled_char, push_zerowidth, resolve_cell_colors, trim_default_trailing_spaces, StyledLine,
};

pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// Side effects the emulator asks the host to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Bytes that must be written back to the PTY / channel (e.g. DSR replies).
    PtyWrite(String),
    Title(String),
    ResetTitle,
    Bell,
    ClipboardStore(String),
}

/// Input-encoding modes requested by the running program.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputModes {
    /// DECCKM: arrows / Home / End send `ESC O x` instead of `ESC [ x`.
    pub app_cursor: bool,
    /// Wrap pasted text in `ESC [200~` … `ESC [201~`.
    pub bracketed_paste: bool,
    /// Full-screen app on the alternate screen: the wheel should send arrow
    /// keys instead of scrolling history.
    pub alternate_scroll: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TermSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Clone, Default)]
pub struct EventCollector {
    events: Arc<Mutex<Vec<TerminalEvent>>>,
}

impl EventListener for EventCollector {
    fn send_event(&self, event: Event) {
        let mapped = match event {
            Event::PtyWrite(s) => TerminalEvent::PtyWrite(s),
            Event::Title(s) => TerminalEvent::Title(s),
            Event::ResetTitle => TerminalEvent::ResetTitle,
            Event::Bell => TerminalEvent::Bell,
            Event::ClipboardStore(_, s) => TerminalEvent::ClipboardStore(s),
            _ => return,
        };
        if let Ok(mut events) = self.events.lock() {
            events.push(mapped);
        }
    }
}

/// Feeds PTY / SSH output into a full xterm-compatible terminal grid.
pub struct TerminalEmulator {
    pub(crate) term: Term<EventCollector>,
    parser: Processor,
    events: EventCollector,
}

impl TerminalEmulator {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self::with_scrollback(cols, rows, DEFAULT_SCROLLBACK)
    }

    pub fn with_scrollback(cols: usize, rows: usize, scrollback: usize) -> Self {
        let size = TermSize {
            cols: cols.max(1),
            rows: rows.max(1),
        };
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        let events = EventCollector::default();
        Self {
            term: Term::new(config, &size, events.clone()),
            parser: Processor::new(),
            events,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.term.resize(TermSize {
            cols: cols.max(1),
            rows: rows.max(1),
        });
    }

    /// `(cols, rows)` of the visible screen.
    pub fn size(&self) -> (usize, usize) {
        (self.term.columns(), self.term.screen_lines())
    }

    /// `(col, row)` of the cursor within the visible screen.
    pub fn cursor(&self) -> (usize, usize) {
        let point = self.term.grid().cursor.point;
        (point.column.0, point.line.0.max(0) as usize)
    }

    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// Modes that change how the host must encode keyboard / paste input.
    pub fn input_modes(&self) -> InputModes {
        let mode = self.term.mode();
        InputModes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            alternate_scroll: mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL),
        }
    }

    /// Drains events emitted since the last call. Hosts must forward
    /// [`TerminalEvent::PtyWrite`] back to the remote side.
    pub fn take_events(&self) -> Vec<TerminalEvent> {
        self.events
            .events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }

    /// Plain-text snapshot of the visible screen (for tests / headless preview).
    pub fn visible_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .visible_styled_lines()
            .into_iter()
            .map(|line| line.plain_text().trim_end().to_string())
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines
    }

    /// Visible screen as styled runs (16 / 256 / truecolor, bold/italic/underline/inverse).
    ///
    /// Trailing empty rows are dropped. Trailing spaces with default chrome colors are
    /// trimmed, but spaces that carry a non-default background (powerline) are kept.
    pub fn visible_styled_lines(&self) -> Vec<StyledLine> {
        let grid = self.term.grid();
        let colors = self.term.colors();
        let mut lines: Vec<StyledLine> = (0..self.term.screen_lines())
            .map(|row| {
                let row = &grid[Line(row as i32)];
                let mut runs = Vec::new();
                for col in 0..self.term.columns() {
                    let cell = &row[Column(col)];
                    if cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        continue;
                    }
                    let style = resolve_cell_colors(cell.fg, cell.bg, cell.flags, colors);
                    let ch = if cell.flags.contains(Flags::HIDDEN) {
                        ' '
                    } else {
                        cell.c
                    };
                    push_styled_char(&mut runs, ch, style);
                    if let Some(zw) = cell.zerowidth() {
                        for zc in zw {
                            push_zerowidth(&mut runs, *zc, style);
                        }
                    }
                }
                trim_default_trailing_spaces(&mut runs);
                StyledLine { runs }
            })
            .collect();
        while lines.last().is_some_and(StyledLine::is_visually_empty) {
            lines.pop();
        }
        lines
    }

    /// Underlying terminal, for renderers that need cells, colors and damage.
    pub fn term(&self) -> &Term<EventCollector> {
        &self.term
    }

    pub fn term_mut(&mut self) -> &mut Term<EventCollector> {
        &mut self.term
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::index::Point;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    use super::*;

    #[test]
    fn prints_plain_text() {
        let mut term = TerminalEmulator::new(40, 10);
        term.process(b"hello\r\nworld");
        assert_eq!(term.visible_lines(), vec!["hello", "world"]);
        assert_eq!(term.cursor(), (5, 1));
    }

    #[test]
    fn clear_screen_csi() {
        let mut term = TerminalEmulator::new(20, 5);
        term.process(b"abc\x1b[2J");
        assert!(term.visible_lines().is_empty());
    }

    #[test]
    fn cursor_position_csi() {
        let mut term = TerminalEmulator::new(20, 5);
        term.process(b"\x1b[3;5Hx");
        assert_eq!(term.visible_lines(), vec!["", "", "    x"]);
    }

    #[test]
    fn wide_chars_occupy_two_columns() {
        let mut term = TerminalEmulator::new(20, 5);
        term.process("中文ab".as_bytes());
        assert_eq!(term.visible_lines(), vec!["中文ab"]);
        assert_eq!(term.cursor(), (6, 0));
    }

    #[test]
    fn sgr_sets_cell_color() {
        let mut term = TerminalEmulator::new(20, 5);
        term.process(b"\x1b[31mR\x1b[0mN");
        let grid = term.term().grid();
        assert_eq!(grid[Point::new(Line(0), Column(0))].fg, Color::Named(NamedColor::Red));
        assert_eq!(
            grid[Point::new(Line(0), Column(1))].fg,
            Color::Named(NamedColor::Foreground)
        );
    }

    #[test]
    fn visible_styled_lines_resolve_ansi_and_truecolor() {
        use crate::palette::{indexed_color, CellRgb, DEFAULT_FG};

        let mut term = TerminalEmulator::new(40, 5);
        // Red "R", reset "N", then truecolor cyan "T", then powerline-ish bg space.
        term.process(b"\x1b[31mR\x1b[0mN");
        term.process(b"\x1b[38;2;0;200;255mT");
        term.process(b"\x1b[48;5;33m \x1b[0m");

        let lines = term.visible_styled_lines();
        assert_eq!(lines.len(), 1);
        let runs = &lines[0].runs;
        assert!(runs.len() >= 3);
        assert_eq!(runs[0].text, "R");
        assert_eq!(runs[0].style.fg, indexed_color(1));
        assert_eq!(runs[1].text, "N");
        assert_eq!(runs[1].style.fg, DEFAULT_FG);
        assert_eq!(runs[2].text, "T");
        assert_eq!(runs[2].style.fg, CellRgb::new(0, 200, 255));
        let bg_run = runs.iter().find(|r| r.text == " ").expect("bg space");
        assert_eq!(bg_run.style.bg, indexed_color(33));
    }

    #[test]
    fn inverse_swaps_fg_bg() {
        use crate::palette::{indexed_color, DEFAULT_BG};

        let mut term = TerminalEmulator::new(20, 3);
        term.process(b"\x1b[7;32mX\x1b[0m");
        let runs = &term.visible_styled_lines()[0].runs;
        assert_eq!(runs[0].text, "X");
        assert_eq!(runs[0].style.fg, DEFAULT_BG);
        assert_eq!(runs[0].style.bg, indexed_color(2));
    }

    #[test]
    fn scrolled_lines_go_to_history() {
        let mut term = TerminalEmulator::new(10, 3);
        term.process(b"1\r\n2\r\n3\r\n4\r\n5");
        assert_eq!(term.visible_lines(), vec!["3", "4", "5"]);
        assert_eq!(term.history_size(), 2);
    }

    #[test]
    fn device_status_report_replies_via_event() {
        let mut term = TerminalEmulator::new(20, 5);
        term.process(b"ab\x1b[6n");
        assert_eq!(
            term.take_events(),
            vec![TerminalEvent::PtyWrite("\x1b[1;3R".into())]
        );
        assert!(term.take_events().is_empty());
    }

    #[test]
    fn title_change_emits_event() {
        let mut term = TerminalEmulator::new(20, 5);
        term.process(b"\x1b]0;gale\x07");
        assert!(term.take_events().contains(&TerminalEvent::Title("gale".into())));
    }

    #[test]
    fn resize_updates_size() {
        let mut term = TerminalEmulator::new(20, 5);
        term.resize(40, 10);
        assert_eq!(term.size(), (40, 10));
    }

    #[test]
    fn input_modes_track_decckm_and_bracketed_paste() {
        let mut term = TerminalEmulator::new(20, 5);
        assert_eq!(term.input_modes(), InputModes::default());
        term.process(b"\x1b[?1h\x1b[?2004h");
        assert_eq!(
            term.input_modes(),
            InputModes {
                app_cursor: true,
                bracketed_paste: true,
                alternate_scroll: false,
            }
        );
        term.process(b"\x1b[?1l\x1b[?2004l");
        assert_eq!(term.input_modes(), InputModes::default());
    }
}
