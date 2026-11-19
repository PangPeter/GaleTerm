//! Remote-desktop view: paints a BGRA framebuffer and forwards pointer / key
//! events as VNC (RFB) input.

mod keysym;
mod view;

pub use keysym::keysym_from_keystroke;
pub use view::{DesktopPane, DesktopPaneEvent};
