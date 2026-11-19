//! Local PTY and terminal emulation (via `alacritty_terminal`).

mod error;
mod palette;
mod pty;
mod screen;
mod snapshot;
mod style;

pub use alacritty_terminal;
pub use error::{Error, Result};
pub use palette::{indexed_color, resolve_color, CellRgb, DEFAULT_BG, DEFAULT_FG};
pub use pty::{LocalPty, PtySize};
pub use screen::{
    EventCollector, InputModes, TerminalEmulator, TerminalEvent, DEFAULT_SCROLLBACK,
};
pub use snapshot::{
    CursorShape, CursorSnapshot, GridLine, GridRun, ScreenSnapshot, SelectionKind,
};
pub use style::{CellStyle, StyledLine, StyledRun};
