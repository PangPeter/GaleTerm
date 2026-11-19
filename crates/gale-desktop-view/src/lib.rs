//! Remote-desktop view: paints a BGRA framebuffer and forwards pointer / key
//! events for VNC and RDP sessions.

mod frame;
mod keysym;
mod view;

pub use frame::{DesktopInput, FrameBuffer};
pub use keysym::keysym_from_keystroke;
pub use view::{DesktopPane, DesktopPaneEvent};
