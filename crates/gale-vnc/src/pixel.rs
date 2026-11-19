//! RFB pixel formats and conversion into BGRA8 for gpui.

/// Wire pixel format (16 bytes after the type/pad in SetPixelFormat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// 32-bpp little-endian RGB888 — easy to turn into BGRA for the GPU.
    pub const RGB888_LE: Self = Self {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_colour: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 16,
        green_shift: 8,
        blue_shift: 0,
    };

    pub fn bytes_per_pixel(self) -> usize {
        (self.bits_per_pixel as usize).div_ceil(8)
    }

    pub fn write(self, out: &mut Vec<u8>) {
        out.push(self.bits_per_pixel);
        out.push(self.depth);
        out.push(u8::from(self.big_endian));
        out.push(u8::from(self.true_colour));
        out.extend_from_slice(&self.red_max.to_be_bytes());
        out.extend_from_slice(&self.green_max.to_be_bytes());
        out.extend_from_slice(&self.blue_max.to_be_bytes());
        out.push(self.red_shift);
        out.push(self.green_shift);
        out.push(self.blue_shift);
        out.extend_from_slice(&[0; 3]);
    }

    pub fn read(buf: &[u8]) -> Option<Self> {
        if buf.len() < 16 {
            return None;
        }
        Some(Self {
            bits_per_pixel: buf[0],
            depth: buf[1],
            big_endian: buf[2] != 0,
            true_colour: buf[3] != 0,
            red_max: u16::from_be_bytes([buf[4], buf[5]]),
            green_max: u16::from_be_bytes([buf[6], buf[7]]),
            blue_max: u16::from_be_bytes([buf[8], buf[9]]),
            red_shift: buf[10],
            green_shift: buf[11],
            blue_shift: buf[12],
        })
    }
}

/// Convert a row of pixels in `format` into BGRA8 (gpui atlas layout).
pub fn decode_pixels(format: PixelFormat, src: &[u8], dst: &mut [u8]) {
    let bpp = format.bytes_per_pixel();
    debug_assert_eq!(dst.len() % 4, 0);
    debug_assert_eq!(src.len(), dst.len() / 4 * bpp);

    if format == PixelFormat::RGB888_LE && bpp == 4 {
        for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            // LE RGB888 stored as B,G,R,A or R,G,B,pad — with our requested
            // shifts (R=16,G=8,B=0) a u32 LE reads as 0x00RRGGBB → bytes B,G,R,0.
            d[0] = s[0]; // B
            d[1] = s[1]; // G
            d[2] = s[2]; // R
            d[3] = 255;
        }
        return;
    }

    for (i, d) in dst.chunks_exact_mut(4).enumerate() {
        let pixel = read_pixel(format, &src[i * bpp..i * bpp + bpp]);
        let r = scale_channel(pixel >> format.red_shift, format.red_max);
        let g = scale_channel(pixel >> format.green_shift, format.green_max);
        let b = scale_channel(pixel >> format.blue_shift, format.blue_max);
        d[0] = b;
        d[1] = g;
        d[2] = r;
        d[3] = 255;
    }
}

fn read_pixel(format: PixelFormat, bytes: &[u8]) -> u32 {
    match (format.bytes_per_pixel(), format.big_endian) {
        (1, _) => u32::from(bytes[0]),
        (2, true) => u32::from(u16::from_be_bytes([bytes[0], bytes[1]])),
        (2, false) => u32::from(u16::from_le_bytes([bytes[0], bytes[1]])),
        (4, true) => u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        (4, false) => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        _ => 0,
    }
}

fn scale_channel(value: u32, max: u16) -> u8 {
    if max == 0 {
        return 0;
    }
    let masked = value & u32::from(max);
    ((masked * 255) / u32::from(max)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb888_le_roundtrips_to_bgra() {
        // One red pixel: R=255 at shift 16 → bytes [0, 0, 255, 0] in LE.
        let src = [0u8, 0, 255, 0];
        let mut dst = [0u8; 4];
        decode_pixels(PixelFormat::RGB888_LE, &src, &mut dst);
        assert_eq!(dst, [0, 0, 255, 255]); // BGRA
    }
}
