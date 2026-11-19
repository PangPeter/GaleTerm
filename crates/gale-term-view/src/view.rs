use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gale_term::{
    CellRgb, CellStyle, CursorShape, CursorSnapshot, GridRun, InputModes, ScreenSnapshot,
    SelectionKind, TerminalEmulator, TerminalEvent, DEFAULT_BG, DEFAULT_FG,
};
use gpui_kit::component::ActiveTheme;
use gpui_kit::*;

/// Default monospace family after the desktop registers bundled Meslo fonts.
/// Prefer `MesloLGS NF`; fall back to theme mono / Menlo if unavailable.
pub const DEFAULT_TERM_FONT: &str = "MesloLGS NF";

/// Line height relative to the font size; also the row pitch used for sizing.
const LINE_HEIGHT_RATIO: f32 = 1.3;

/// Events the pane asks the host to handle (write to PTY, title, etc.).
///
/// Returned from [`TerminalPane::feed_bytes`]; `Output` is also emitted for
/// input the pane generates itself (wheel → arrow keys on the alternate screen).
#[derive(Clone, Debug)]
pub enum TerminalPaneEvent {
    Output(Vec<u8>),
    Title(String),
    Bell,
}

/// Emitted after the grid changes size; hosts must resize the PTY / channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalResized {
    pub cols: usize,
    pub rows: usize,
}

impl EventEmitter<TerminalResized> for TerminalPane {}
impl EventEmitter<TerminalPaneEvent> for TerminalPane {}

/// Where the grid was last painted, for mouse hit-testing.
#[derive(Clone, Copy)]
struct GridLayout {
    origin: Point<Pixels>,
    cell_w: Pixels,
    cell_h: Pixels,
}

/// Retained terminal pane: owns the emulator and paints it cell-accurately.
pub struct TerminalPane {
    emulator: Arc<Mutex<TerminalEmulator>>,
    cols: usize,
    rows: usize,
    font_family: SharedString,
    /// Host focus handle; the cursor turns hollow while it is not focused.
    focus: Option<FocusHandle>,
    layout: Rc<Cell<Option<GridLayout>>>,
    selecting: bool,
    /// Fractional wheel lines carried between events (trackpads).
    scroll_remainder: f32,
}

impl TerminalPane {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self::with_font(cols, rows, DEFAULT_TERM_FONT)
    }

    pub fn with_font(cols: usize, rows: usize, font_family: impl Into<SharedString>) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            emulator: Arc::new(Mutex::new(TerminalEmulator::new(cols, rows))),
            cols,
            rows,
            font_family: font_family.into(),
            focus: None,
            layout: Rc::new(Cell::new(None)),
            selecting: false,
            scroll_remainder: 0.,
        }
    }

    pub fn set_font_family(&mut self, family: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.font_family = family.into();
        cx.notify();
    }

    pub fn set_focus_handle(&mut self, focus: FocusHandle) {
        self.focus = Some(focus);
    }

    pub fn emulator(&self) -> Arc<Mutex<TerminalEmulator>> {
        self.emulator.clone()
    }

    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    pub fn input_modes(&self) -> InputModes {
        self.with_term(|term| term.input_modes()).unwrap_or_default()
    }

    pub fn selection_text(&self) -> Option<String> {
        self.with_term(|term| term.selection_text()).flatten()
    }

    /// Call before forwarding typed input: drops the selection and jumps back
    /// to live output.
    pub fn on_user_input(&mut self, cx: &mut Context<Self>) {
        let changed = self
            .with_term_mut(|term| {
                let changed = term.has_selection() || term.display_offset() > 0;
                term.clear_selection();
                term.scroll_to_bottom();
                changed
            })
            .unwrap_or(false);
        if changed {
            cx.notify();
        }
    }

    /// Feed PTY / SSH bytes into the emulator; the frame is rebuilt on render.
    pub fn feed_bytes(&mut self, bytes: &[u8], cx: &mut Context<Self>) -> Vec<TerminalPaneEvent> {
        let events = self
            .with_term_mut(|term| {
                term.process(bytes);
                term.take_events()
            })
            .unwrap_or_default();
        cx.notify();
        events
            .into_iter()
            .filter_map(|ev| match ev {
                TerminalEvent::PtyWrite(s) => Some(TerminalPaneEvent::Output(s.into_bytes())),
                TerminalEvent::Title(s) => Some(TerminalPaneEvent::Title(s)),
                TerminalEvent::ResetTitle => Some(TerminalPaneEvent::Title(String::new())),
                TerminalEvent::Bell => Some(TerminalPaneEvent::Bell),
                TerminalEvent::ClipboardStore(_) => None,
            })
            .collect()
    }

    pub fn resize(&mut self, cols: usize, rows: usize, cx: &mut Context<Self>) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.with_term_mut(|term| term.resize(cols, rows));
        cx.emit(TerminalResized { cols, rows });
        cx.notify();
    }

    pub fn refresh_snapshot(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    fn with_term<R>(&self, f: impl FnOnce(&TerminalEmulator) -> R) -> Option<R> {
        self.emulator.lock().ok().map(|term| f(&term))
    }

    fn with_term_mut<R>(&self, f: impl FnOnce(&mut TerminalEmulator) -> R) -> Option<R> {
        self.emulator.lock().ok().map(|mut term| f(&mut term))
    }

    /// `(cell width, line height)` for the current font.
    fn cell_size(&self, window: &Window, font_size: Pixels) -> (Pixels, Pixels) {
        let text = window.text_system();
        let font_id = text.resolve_font(&font(self.font_family.clone()));
        let width = text
            .em_advance(font_id, font_size)
            .unwrap_or(font_size * 0.6);
        (width, (font_size * LINE_HEIGHT_RATIO).round())
    }

    /// Visible cell under `position`, plus whether it is the right half.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(usize, usize, bool)> {
        let layout = self.layout.get()?;
        let x = ((position.x - layout.origin.x) / layout.cell_w).max(0.);
        let y = ((position.y - layout.origin.y) / layout.cell_h).max(0.);
        let col = (x.floor() as usize).min(self.cols.saturating_sub(1));
        let row = (y.floor() as usize).min(self.rows.saturating_sub(1));
        Some((col, row, x.fract() >= 0.5))
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some((col, row, right)) = self.cell_at(ev.position) else {
            return;
        };
        let kind = match ev.click_count {
            0 | 1 => SelectionKind::Simple,
            2 => SelectionKind::Word,
            _ => SelectionKind::Line,
        };
        self.with_term_mut(|term| {
            if ev.modifiers.shift && term.has_selection() {
                term.update_selection(col, row, right);
            } else {
                term.start_selection(col, row, right, kind);
            }
        });
        self.selecting = true;
        cx.notify();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.selecting || ev.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if let Some((col, row, right)) = self.cell_at(ev.position) {
            self.with_term_mut(|term| term.update_selection(col, row, right));
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, cx: &mut Context<Self>) {
        self.selecting = false;
        self.with_term_mut(|term| {
            if !term.has_selection() {
                term.clear_selection();
            }
        });
        cx.notify();
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.get() else {
            return;
        };
        self.scroll_remainder += ev.delta.pixel_delta(layout.cell_h).y / layout.cell_h;
        let lines = self.scroll_remainder.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_remainder -= lines as f32;

        let modes = self.input_modes();
        if modes.alternate_scroll {
            let key: &[u8] = match (lines > 0, modes.app_cursor) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            cx.emit(TerminalPaneEvent::Output(key.repeat(lines.unsigned_abs() as usize)));
        } else {
            self.with_term_mut(|term| term.scroll_lines(lines));
            cx.notify();
        }
    }
}

fn cell_rgb_to_hsla(c: CellRgb) -> Hsla {
    rgb(u32::from(c.r) << 16 | u32::from(c.g) << 8 | u32::from(c.b)).into()
}

fn text_run(len: usize, style: CellStyle, color: Hsla, base_font: &Font) -> TextRun {
    let mut run_font = base_font.clone();
    if style.bold {
        run_font = run_font.bold();
    }
    if style.italic {
        run_font = run_font.italic();
    }
    TextRun {
        len,
        font: run_font,
        color,
        background_color: None,
        underline: style.underline.then_some(UnderlineStyle {
            thickness: px(1.),
            color: Some(color),
            wavy: false,
        }),
        strikethrough: style.strikethrough.then_some(StrikethroughStyle {
            thickness: px(1.),
            color: Some(color),
        }),
    }
}

/// Everything the paint closure needs, captured at render time.
struct Frame {
    snapshot: ScreenSnapshot,
    font: Font,
    font_size: Pixels,
    cell_w: Pixels,
    cell_h: Pixels,
    selection_bg: Hsla,
    cursor_color: Hsla,
    focused: bool,
}

impl Frame {
    fn cell_bounds(&self, origin: Point<Pixels>, col: usize, row: usize, cols: usize) -> Bounds<Pixels> {
        Bounds::new(
            point(origin.x + self.cell_w * col as f32, origin.y + self.cell_h * row as f32),
            size(self.cell_w * cols as f32, self.cell_h),
        )
    }

    /// Glyph advance pinned to the grid so fallback fonts cannot drift a row.
    fn pitch(&self, cols: usize, wide: bool) -> Pixels {
        if wide {
            self.cell_w * cols as f32
        } else {
            self.cell_w
        }
    }

    /// Paint `text` in `style.fg` with every glyph advancing exactly `pitch`.
    fn paint_text(
        &self,
        text: &str,
        style: CellStyle,
        pitch: Pixels,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if text.chars().all(|c| c == ' ') && !style.underline && !style.strikethrough {
            return;
        }
        let run = text_run(text.len(), style, cell_rgb_to_hsla(style.fg), &self.font);
        let line = window.text_system().shape_line(
            SharedString::from(text.to_owned()),
            self.font_size,
            &[run],
            Some(pitch),
        );
        let _ = line.paint(at, self.cell_h, TextAlign::Left, None, window, cx);
    }

    fn paint_run(&self, origin: Point<Pixels>, row: usize, run: &GridRun, window: &mut Window, cx: &mut App) {
        let bounds = self.cell_bounds(origin, run.col, row, run.cols);
        if run.selected {
            window.paint_quad(fill(bounds, self.selection_bg));
        } else if run.style.bg != DEFAULT_BG {
            window.paint_quad(fill(bounds, cell_rgb_to_hsla(run.style.bg)));
        }
        let pitch = self.pitch(run.cols, run.wide);
        self.paint_text(&run.text, run.style, pitch, bounds.origin, window, cx);
    }

    fn paint_cursor(&self, origin: Point<Pixels>, cursor: &CursorSnapshot, window: &mut Window, cx: &mut App) {
        let bounds = self.cell_bounds(origin, cursor.col, cursor.row, cursor.cols);
        let bar = px(2.);
        match (cursor.shape, self.focused) {
            (CursorShape::Block, true) => {
                window.paint_quad(fill(bounds, self.cursor_color));
                let style = CellStyle {
                    fg: DEFAULT_BG,
                    underline: false,
                    strikethrough: false,
                    ..cursor.style
                };
                let pitch = self.pitch(cursor.cols, cursor.cols > 1);
                self.paint_text(&cursor.text, style, pitch, bounds.origin, window, cx);
            }
            (CursorShape::Beam, true) => {
                window.paint_quad(fill(
                    Bounds::new(bounds.origin, size(bar, bounds.size.height)),
                    self.cursor_color,
                ));
            }
            (CursorShape::Underline, true) => {
                window.paint_quad(fill(
                    Bounds::new(
                        point(bounds.origin.x, bounds.origin.y + bounds.size.height - bar),
                        size(bounds.size.width, bar),
                    ),
                    self.cursor_color,
                ));
            }
            _ => window.paint_quad(outline(bounds, self.cursor_color, BorderStyle::Solid)),
        }
    }

    fn paint(&self, origin: Point<Pixels>, window: &mut Window, cx: &mut App) {
        for (row, line) in self.snapshot.lines.iter().enumerate() {
            for run in &line.runs {
                self.paint_run(origin, row, run, window, cx);
            }
        }
        if let Some(cursor) = &self.snapshot.cursor {
            self.paint_cursor(origin, cursor, window, cx);
        }
    }
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let font_size = theme.mono_font_size;
        let (cell_w, cell_h) = self.cell_size(window, font_size);
        let Some(snapshot) = self.with_term(TerminalEmulator::snapshot) else {
            return div().into_any_element();
        };
        let frame = Frame {
            snapshot,
            font: font(self.font_family.clone()),
            font_size,
            cell_w,
            cell_h,
            selection_bg: theme.selection,
            cursor_color: cell_rgb_to_hsla(DEFAULT_FG),
            focused: self.focus.as_ref().is_none_or(|f| f.is_focused(window)),
        };

        let (cols, rows) = (self.cols, self.rows);
        let pane = cx.entity().downgrade();
        let layout = self.layout.clone();
        let grid = canvas(
            move |bounds, _, cx| {
                layout.set(Some(GridLayout {
                    origin: bounds.origin,
                    cell_w,
                    cell_h,
                }));
                // Fit the grid to the space we were given; resize after this frame.
                let fit_cols = ((bounds.size.width / cell_w).floor() as usize).max(2);
                let fit_rows = ((bounds.size.height / cell_h).floor() as usize).max(1);
                if (fit_cols, fit_rows) != (cols, rows) {
                    cx.defer(move |cx| {
                        let _ = pane.update(cx, |pane, cx| pane.resize(fit_cols, fit_rows, cx));
                    });
                }
            },
            move |bounds, (), window, cx| frame.paint(bounds.origin, window, cx),
        )
        .size_full();

        div()
            .id("gale-term-pane")
            .size_full()
            .bg(cell_rgb_to_hsla(DEFAULT_BG))
            .px_4()
            .py_3()
            .overflow_hidden()
            .cursor(CursorStyle::IBeam)
            .child(
                div()
                    .id("gale-term-grid")
                    .size_full()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, _, cx| this.on_mouse_down(ev, cx)),
                    )
                    .on_mouse_move(
                        cx.listener(|this, ev: &MouseMoveEvent, _, cx| this.on_mouse_move(ev, cx)),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, cx| this.on_mouse_up(cx)),
                    )
                    .on_scroll(
                        cx.listener(|this, ev: &ScrollEvent, _, cx| this.on_scroll(ev, cx)),
                    )
                    .child(grid),
            )
            .into_any_element()
    }
}
