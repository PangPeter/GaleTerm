//! Remote-desktop view: paints a BGRA framebuffer and forwards pointer / key
//! events for VNC and RDP sessions.

mod frame;
mod keysym;
mod scale;
mod view;

pub use frame::{DesktopInput, FrameBuffer, SoftCursor};
pub use keysym::keysym_from_keystroke;
pub use scale::{layout_image, DesktopScaleMode};
pub use view::{DesktopPane, DesktopPaneEvent};
