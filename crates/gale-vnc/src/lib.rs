//! VNC / RFB client for GaleTerm (Phase 3.5).
//!
//! Speaks RFB 3.8 with VNC password authentication, Raw and CopyRect encodings,
//! and a 32-bpp RGB framebuffer converted to BGRA for gpui.

mod auth;
mod client;
mod error;
mod pixel;

pub use client::{
    FrameBuffer, VncClientConfig, VncInput, VncOutput, VncPrompt, VncSession,
};
pub use error::{Error, Result};
pub use pixel::PixelFormat;
