//! Per-frame render snapshot (column-positioned runs, cursor, selection) plus
//! mouse selection and scrollback control.

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::CursorShape as AnsiCursorShape;

use crate::palette::DEFAULT_BG;
use crate::screen::TerminalEmulator;
use crate::style::{resolve_cell_colors, CellStyle};

/// Cells sharing one style, starting at `col` and spanning `cols` cells.
///
/// Wide (CJK / emoji) characters always get their own run with `cols == 2`
/// so renderers can place them on the grid regardless of glyph advance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridRun {
    pub col: usize,
    pub cols: usize,
    pub text: String,
    pub style: CellStyle,
    pub selected: bool,
    pub wide: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GridLine {
    pub runs: Vec<GridRun>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    HollowBlock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub col: usize,
    pub row: usize,
    /// 2 when the cursor sits on a wide character.
    pub cols: usize,
    pub shape: CursorShape,
    /// Character under the cursor and its style (for block-cursor redraw).
    pub text: String,
    pub style: CellStyle,
}

/// Everything a renderer needs for one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSnapshot {
    pub cols: usize,
    pub rows: usize,
    pub lines: Vec<GridLine>,
    /// `None` when hidden, or scrolled away into history.
    pub cursor: Option<CursorSnapshot>,
    /// Lines scrolled back into history (0 = following live output).
    pub display_offset: usize,
}

/// How a mouse selection grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    /// Character-wise (single click + drag).
    Simple,
    /// Whole words (double click).
    Word,
    /// Whole lines (triple click).
    Line,
}

impl TerminalEmulator {
    pub fn snapshot(&self) -> ScreenSnapshot {
        let term = &self.term;
        let grid = term.grid();
        let colors = term.colors();
        let cols = term.columns();
        let rows = term.screen_lines();
        let offset = grid.display_offset();
        let selection = term.selection.as_ref().and_then(|s| s.to_range(term));

        let lines = (0..rows)
            .map(|row| {
                let line = Line(row as i32 - offset as i32);
                let cells = &grid[line];
                let mut runs: Vec<GridRun> = Vec::new();
                for col in 0..cols {
                    let cell = &cells[Column(col)];
                    if cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        continue;
                    }
                    let style = resolve_cell_colors(cell.fg, cell.bg, cell.flags, colors);
                    let selected = selection
                        .as_ref()
                        .is_some_and(|r| r.contains(Point::new(line, Column(col))));
                    let wide = cell.flags.contains(Flags::WIDE_CHAR);
                    let ch = if cell.flags.contains(Flags::HIDDEN) {
                        ' '
                    } else {
                        cell.c
                    };

                    let merge = runs.last_mut().filter(|last| {
                        !wide
                            && !last.wide
                            && last.style == style
                            && last.selected == selected
                            && last.col + last.cols == col
                    });
                    match merge {
                        Some(last) => {
                            last.text.push(ch);
                            last.cols += 1;
                        }
                        None => runs.push(GridRun {
                            col,
                            cols: if wide { 2 } else { 1 },
                            text: ch.to_string(),
                            style,
                            selected,
                            wide,
                        }),
                    }
                    if let Some(zw) = cell.zerowidth() {
                        runs.last_mut()
                            .expect("run just pushed")
                            .text
                            .extend(zw.iter());
                    }
                }
                for run in &mut runs {
                    trim_blank_tail(run);
                }
                runs.retain(|run| !run.text.is_empty());
                GridLine { runs }
            })
            .collect();

        ScreenSnapshot {
            cols,
            rows,
            lines,
            cursor: self.cursor_snapshot(offset, rows),
            display_offset: offset,
        }
    }

    fn cursor_snapshot(&self, offset: usize, rows: usize) -> Option<CursorSnapshot> {
        let term = &self.term;
        if !term.mode().contains(TermMode::SHOW_CURSOR) {
            return None;
        }
        let shape = match term.cursor_style().shape {
            AnsiCursorShape::Block => CursorShape::Block,
            AnsiCursorShape::Underline => CursorShape::Underline,
            AnsiCursorShape::Beam => CursorShape::Beam,
            AnsiCursorShape::HollowBlock => CursorShape::HollowBlock,
            AnsiCursorShape::Hidden => return None,
        };
        let point = term.grid().cursor.point;
        let row = usize::try_from(point.line.0 + offset as i32).ok()?;
        if row >= rows {
            return None;
        }
        let cell = &term.grid()[point];
        let ch = if cell.c == '\0' { ' ' } else { cell.c };
        Some(CursorSnapshot {
            col: point.column.0,
            row,
            cols: if cell.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 },
            shape,
            text: ch.to_string(),
            style: resolve_cell_colors(cell.fg, cell.bg, cell.flags, term.colors()),
        })
    }

    /// Grid point for a visible `(col, row)`, accounting for scrollback.
    fn visible_point(&self, col: usize, row: usize) -> Point {
        let rows = self.term.screen_lines();
        let cols = self.term.columns();
        let offset = self.term.grid().display_offset() as i32;
        Point::new(
            Line(row.min(rows.saturating_sub(1)) as i32 - offset),
            Column(col.min(cols.saturating_sub(1))),
        )
    }

    /// Begin a mouse selection at a visible cell. `right_half` is whether the
    /// pointer is in the right half of that cell.
    pub fn start_selection(&mut self, col: usize, row: usize, right_half: bool, kind: SelectionKind) {
        let ty = match kind {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Word => SelectionType::Semantic,
            SelectionKind::Line => SelectionType::Lines,
        };
        let point = self.visible_point(col, row);
        self.term.selection = Some(Selection::new(ty, point, side(right_half)));
    }

    pub fn update_selection(&mut self, col: usize, row: usize, right_half: bool) {
        let point = self.visible_point(col, row);
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side(right_half));
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    pub fn has_selection(&self) -> bool {
        self.term.selection.as_ref().is_some_and(|s| !s.is_empty())
    }

    pub fn selection_text(&self) -> Option<String> {
        if !self.has_selection() {
            return None;
        }
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// Scroll the viewport; positive `lines` moves up into history.
    pub fn scroll_lines(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }
}

fn side(right_half: bool) -> Side {
    if right_half {
        Side::Right
    } else {
        Side::Left
    }
}

/// Blank cells on the default background paint nothing, so drop them from
/// the end of a run.
fn trim_blank_tail(run: &mut GridRun) {
    let paints_blanks = run.selected
        || run.style.bg != DEFAULT_BG
        || run.style.underline
        || run.style.strikethrough;
    if paints_blanks || run.wide {
        return;
    }
    let kept = run.text.trim_end_matches([' ', '\0']).len();
    let removed = run.text[kept..].chars().count();
    run.text.truncate(kept);
    run.cols -= removed;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_chars_get_their_own_two_cell_run() {
        let mut term = TerminalEmulator::new(20, 3);
        term.process("ab中c".as_bytes());
        let line = &term.snapshot().lines[0];
        let layout: Vec<_> = line
            .runs
            .iter()
            .map(|r| (r.col, r.cols, r.text.as_str(), r.wide))
            .collect();
        assert_eq!(
            layout,
            vec![(0, 2, "ab", false), (2, 2, "中", true), (4, 1, "c", false)]
        );
    }

    #[test]
    fn cursor_follows_output_and_hides_with_dectcem() {
        let mut term = TerminalEmulator::new(20, 3);
        term.process(b"abc");
        let cursor = term.snapshot().cursor.unwrap();
        assert_eq!((cursor.col, cursor.row), (3, 0));
        term.process(b"\x1b[?25l");
        assert!(term.snapshot().cursor.is_none());
    }

    #[test]
    fn simple_selection_copies_text_and_marks_runs() {
        let mut term = TerminalEmulator::new(20, 3);
        term.process(b"hello world");
        term.start_selection(0, 0, false, SelectionKind::Simple);
        term.update_selection(4, 0, true);
        assert_eq!(term.selection_text().as_deref(), Some("hello"));
        let runs = &term.snapshot().lines[0].runs;
        assert!(runs.iter().any(|r| r.selected && r.text == "hello"));

        term.clear_selection();
        assert!(term.selection_text().is_none());
    }

    #[test]
    fn word_selection_picks_whole_word() {
        let mut term = TerminalEmulator::new(20, 3);
        term.process(b"foo bar baz");
        term.start_selection(5, 0, false, SelectionKind::Word);
        assert_eq!(term.selection_text().as_deref(), Some("bar"));
    }

    #[test]
    fn scrollback_moves_viewport_and_hides_cursor() {
        let mut term = TerminalEmulator::new(10, 3);
        for i in 0..10 {
            term.process(format!("line{i}\r\n").as_bytes());
        }
        term.scroll_lines(2);
        let snap = term.snapshot();
        assert_eq!(snap.display_offset, 2);
        assert!(snap.cursor.is_none());
        assert_eq!(snap.lines[0].runs[0].text, "line6");
        term.scroll_to_bottom();
        assert_eq!(term.display_offset(), 0);
    }
}
