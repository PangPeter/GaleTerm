//! Terminal view for gpui — styled cell runs via [`StyledText`] (16/256/truecolor).
//!
//! Damage-based incremental GPU redraw comes next; hosts own a
//! [`gale_term::TerminalEmulator`] and feed PTY bytes into it.

mod view;

pub use view::{TerminalPane, TerminalPaneEvent, TerminalResized, DEFAULT_TERM_FONT};
