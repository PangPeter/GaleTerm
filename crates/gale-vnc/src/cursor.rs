//! Local cursor pseudo-encodings: Cursor (-239) and XCursor (-240).

use crate::error::{Error, Result};
use crate::pixel::{decode_pixels, PixelFormat};

/// Soft-cursor shape in BGRA8 with per-pixel alpha (mask applied).
#[derive(Clone, Debug)]
pub struct CursorShape {
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub width: u16,
    pub height: u16,
    /// `width * height * 4` BGRA bytes; alpha 0 = transparent.
    pub pixels: Vec<u8>,
}

impl CursorShape {
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Decode RFB Cursor / RichCursor pixel + bitmask into BGRA.
pub fn decode_rich_cursor(
    format: PixelFormat,
    hotspot_x: u16,
    hotspot_y: u16,
    width: u16,
    height: u16,
    pixels: &[u8],
    mask: &[u8],
) -> Result<CursorShape> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return Ok(CursorShape {
            hotspot_x,
            hotspot_y,
            width: 0,
            height: 0,
            pixels: Vec::new(),
        });
    }
    let bpp = format.bytes_per_pixel();
    let expect = w * h * bpp;
    if pixels.len() < expect {
        return Err(Error::Protocol(format!(
            "cursor pixels short: {} < {expect}",
            pixels.len()
        )));
    }
    let row_mask = w.div_ceil(8);
    let expect_mask = row_mask * h;
    if mask.len() < expect_mask {
        return Err(Error::Protocol(format!(
            "cursor mask short: {} < {expect_mask}",
            mask.len()
        )));
    }

    let mut bgra = vec![0u8; w * h * 4];
    for row in 0..h {
        let src = &pixels[row * w * bpp..(row + 1) * w * bpp];
        let dst = &mut bgra[row * w * 4..(row + 1) * w * 4];
        decode_pixels(format, src, dst);
        let mrow = &mask[row * row_mask..(row + 1) * row_mask];
        for col in 0..w {
            let bit = 7 - (col % 8);
            let opaque = (mrow[col / 8] >> bit) & 1 != 0;
            dst[col * 4 + 3] = if opaque { 255 } else { 0 };
        }
    }

    Ok(CursorShape {
        hotspot_x,
        hotspot_y,
        width,
        height,
        pixels: bgra,
    })
}

/// Decode RFB XCursor (two-color) into BGRA.
pub fn decode_xcursor(
    hotspot_x: u16,
    hotspot_y: u16,
    width: u16,
    height: u16,
    fore: [u8; 3],
    back: [u8; 3],
    bitmap: &[u8],
    mask: &[u8],
) -> Result<CursorShape> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return Ok(CursorShape {
            hotspot_x,
            hotspot_y,
            width: 0,
            height: 0,
            pixels: Vec::new(),
        });
    }
    let row_bytes = w.div_ceil(8);
    let expect = row_bytes * h;
    if bitmap.len() < expect || mask.len() < expect {
        return Err(Error::Protocol("xcursor bitmap/mask short".into()));
    }

    let mut bgra = vec![0u8; w * h * 4];
    for row in 0..h {
        let brow = &bitmap[row * row_bytes..(row + 1) * row_bytes];
        let mrow = &mask[row * row_bytes..(row + 1) * row_bytes];
        for col in 0..w {
            let bit = 7 - (col % 8);
            let on = (brow[col / 8] >> bit) & 1 != 0;
            let opaque = (mrow[col / 8] >> bit) & 1 != 0;
            let i = (row * w + col) * 4;
            let rgb = if on { fore } else { back };
            // BGRA
            bgra[i] = rgb[2];
            bgra[i + 1] = rgb[1];
            bgra[i + 2] = rgb[0];
            bgra[i + 3] = if opaque { 255 } else { 0 };
        }
    }

    Ok(CursorShape {
        hotspot_x,
        hotspot_y,
        width,
        height,
        pixels: bgra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pixel::PixelFormat;

    #[test]
    fn rich_cursor_applies_mask() {
        // 2×1 cursor: left opaque red, right transparent.
        let format = PixelFormat::RGB888_LE;
        // LE 0x00RRGGBB → bytes B,G,R,0
        let pixels = [
            0, 0, 255, 0, // red
            0, 255, 0, 0, // green (masked out)
        ];
        let mask = [0b1000_0000]; // only first pixel
        let shape = decode_rich_cursor(format, 0, 0, 2, 1, &pixels, &mask).unwrap();
        assert_eq!(&shape.pixels[0..4], &[0, 0, 255, 255]);
        assert_eq!(shape.pixels[7], 0);
    }

    #[test]
    fn xcursor_two_tone() {
        // 1×1 opaque foreground white
        let shape = decode_xcursor(0, 0, 1, 1, [255, 255, 255], [0, 0, 0], &[0b1000_0000], &[0b1000_0000])
            .unwrap();
        assert_eq!(&shape.pixels, &[255, 255, 255, 255]);
    }
}
