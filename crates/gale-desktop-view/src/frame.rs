//! Shared remote-desktop framebuffer and input events (VNC / RDP).

use std::sync::Arc;

/// Full desktop pixels in BGRA8 (gpui atlas layout).
#[derive(Clone)]
pub struct FrameBuffer {
    pub width: u16,
    pub height: u16,
    pub pixels: Arc<[u8]>,
}

impl FrameBuffer {
    pub fn new(width: u16, height: u16) -> Self {
        let len = width as usize * height as usize * 4;
        Self {
            width,
            height,
            pixels: vec![0u8; len].into(),
        }
    }

    pub fn from_rgba(width: u16, height: u16, rgba: &[u8]) -> Self {
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        for (src, dst) in rgba.chunks_exact(4).zip(pixels.chunks_exact_mut(4)) {
            dst[0] = src[2]; // B
            dst[1] = src[1]; // G
            dst[2] = src[0]; // R
            dst[3] = if src[3] == 0 { 255 } else { src[3] };
        }
        Self {
            width,
            height,
            pixels: pixels.into(),
        }
    }

    pub fn from_bgra(width: u16, height: u16, bgra: impl Into<Arc<[u8]>>) -> Self {
        Self {
            width,
            height,
            pixels: bgra.into(),
        }
    }
}

/// Pointer / keyboard / clipboard events from the desktop pane.
#[derive(Debug, Clone)]
pub enum DesktopInput {
    /// RFB-style button mask: bit0 L, bit1 M, bit2 R, bit3 wheel-up, bit4 wheel-down.
    Pointer { x: u16, y: u16, buttons: u8 },
    /// X11 keysym (VNC) — RDP maps these to scancodes / unicode.
    Key { down: bool, keysym: u32 },
    CutText(String),
    Refresh,
    Close,
}
