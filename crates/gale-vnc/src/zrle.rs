//! ZRLE (Zlib Run-Length Encoding) — RFB encoding type 16 (RFC 6143 §7.6.9).
//!
//! Assumes the client has negotiated 32-bpp / depth-24 true-colour
//! ([`PixelFormat::RGB888_LE`](crate::pixel::PixelFormat::RGB888_LE)), so each
//! CPIXEL is 3 bytes (B,G,R — the unused pad byte is omitted on the wire).
//!
//! Important: a **single** zlib stream is shared for the whole RFB connection.
//! Each rectangle is a Sync-flushed chunk, not a standalone zlib file.

use flate2::{Decompress, FlushDecompress, Status};

use crate::error::{Error, Result};

const TILE: usize = 64;
const CPIXEL: usize = 3;

/// Persistent inflater for one VNC connection's ZRLE rectangles.
#[derive(Debug)]
pub struct ZrleDecoder {
    inflate: Decompress,
}

impl Default for ZrleDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ZrleDecoder {
    pub fn new() -> Self {
        Self {
            // `zlib_header = true`: RFB ZRLE wraps deflate in a zlib stream.
            inflate: Decompress::new(true),
        }
    }

    /// Inflate one ZRLE rectangle and blit BGRA pixels into `fb`.
    pub fn decode_rect(
        &mut self,
        compressed: &[u8],
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        fb_w: u16,
        fb: &mut [u8],
    ) -> Result<()> {
        let inflated = inflate_chunk(&mut self.inflate, compressed)?;
        let (rw, rh) = (w as usize, h as usize);
        let fw = fb_w as usize;
        let mut cursor = Cursor::new(&inflated);

        let mut ty = 0usize;
        while ty < rh {
            let th = (rh - ty).min(TILE);
            let mut tx = 0usize;
            while tx < rw {
                let tw = (rw - tx).min(TILE);
                decode_tile(&mut cursor, tw, th, |row, col, bgr| {
                    let dst_x = x as usize + tx + col;
                    let dst_y = y as usize + ty + row;
                    let i = (dst_y * fw + dst_x) * 4;
                    fb[i] = bgr[0];
                    fb[i + 1] = bgr[1];
                    fb[i + 2] = bgr[2];
                    fb[i + 3] = 255;
                })?;
                tx += tw;
            }
            ty += th;
        }

        let _ = cursor.remaining();
        Ok(())
    }
}

fn inflate_chunk(inflate: &mut Decompress, compressed: &[u8]) -> Result<Vec<u8>> {
    if compressed.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = compressed;
    let mut out = Vec::with_capacity(compressed.len().saturating_mul(16).max(4096));
    while !input.is_empty() {
        let in_before = inflate.total_in();
        let out_before = out.len();
        match inflate.decompress_vec(input, &mut out, FlushDecompress::Sync) {
            Ok(Status::Ok) | Ok(Status::StreamEnd) => {}
            Ok(Status::BufError) => {
                out.reserve(out.capacity().max(4096));
                continue;
            }
            Err(e) => return Err(Error::Protocol(format!("zrle zlib: {e}"))),
        }
        let consumed = (inflate.total_in() - in_before) as usize;
        if consumed > input.len() {
            return Err(Error::Protocol("zrle zlib: consumed past input".into()));
        }
        input = &input[consumed..];
        if consumed == 0 && out.len() == out_before {
            if out.capacity() < out.len() + 65536 {
                out.reserve(65536);
                continue;
            }
            return Err(Error::Protocol("zrle zlib: stalled".into()));
        }
    }
    Ok(out)
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn u8(&mut self) -> Result<u8> {
        if self.pos >= self.data.len() {
            return Err(Error::Protocol("zrle: truncated".into()));
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn cpixel(&mut self) -> Result<[u8; 3]> {
        if self.pos + CPIXEL > self.data.len() {
            return Err(Error::Protocol("zrle: truncated cpixel".into()));
        }
        let p = [
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
        ];
        self.pos += CPIXEL;
        Ok(p)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            return Err(Error::Protocol("zrle: truncated data".into()));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
}

fn decode_tile(
    cur: &mut Cursor<'_>,
    tw: usize,
    th: usize,
    mut put: impl FnMut(usize, usize, [u8; 3]),
) -> Result<()> {
    let sub = cur.u8()?;
    let rle = sub & 0x80 != 0;
    let palette_size = usize::from(sub & 0x7f);

    match (rle, palette_size) {
        (false, 0) => {
            for row in 0..th {
                for col in 0..tw {
                    put(row, col, cur.cpixel()?);
                }
            }
        }
        (false, 1) => {
            let c = cur.cpixel()?;
            for row in 0..th {
                for col in 0..tw {
                    put(row, col, c);
                }
            }
        }
        (false, 2..=16) => {
            let mut palette = [[0u8; 3]; 16];
            for i in 0..palette_size {
                palette[i] = cur.cpixel()?;
            }
            let bits = if palette_size <= 2 {
                1
            } else if palette_size <= 4 {
                2
            } else {
                4
            };
            packed_indices(cur, tw, th, bits, |row, col, idx| {
                if idx >= palette_size {
                    return Err(Error::Protocol(format!(
                        "zrle: palette index {idx} >= {palette_size}"
                    )));
                }
                put(row, col, palette[idx]);
                Ok(())
            })?;
        }
        (true, 0) => {
            let mut n = 0usize;
            let total = tw * th;
            while n < total {
                let c = cur.cpixel()?;
                let run = read_run_length(cur)?;
                for _ in 0..run {
                    if n >= total {
                        return Err(Error::Protocol("zrle: plain RLE overflow".into()));
                    }
                    put(n / tw, n % tw, c);
                    n += 1;
                }
            }
        }
        (true, 1) => {
            return Err(Error::Protocol("zrle: invalid subencoding 129".into()));
        }
        (true, 2..=127) => {
            let mut palette = [[0u8; 3]; 127];
            for i in 0..palette_size {
                palette[i] = cur.cpixel()?;
            }
            let mut n = 0usize;
            let total = tw * th;
            while n < total {
                let b = cur.u8()?;
                let idx = usize::from(b & 0x7f);
                if idx >= palette_size {
                    return Err(Error::Protocol(format!(
                        "zrle: palette-RLE index {idx} >= {palette_size}"
                    )));
                }
                let run = if b & 0x80 == 0 {
                    1
                } else {
                    read_run_length(cur)?
                };
                let c = palette[idx];
                for _ in 0..run {
                    if n >= total {
                        return Err(Error::Protocol("zrle: palette RLE overflow".into()));
                    }
                    put(n / tw, n % tw, c);
                    n += 1;
                }
            }
        }
        _ => {
            return Err(Error::Protocol(format!(
                "zrle: unsupported subencoding {sub}"
            )));
        }
    }
    Ok(())
}

fn read_run_length(cur: &mut Cursor<'_>) -> Result<usize> {
    let mut sum = 0usize;
    loop {
        let b = cur.u8()?;
        sum += usize::from(b);
        if b != 255 {
            break;
        }
    }
    Ok(sum + 1)
}

fn packed_indices(
    cur: &mut Cursor<'_>,
    tw: usize,
    th: usize,
    bits: usize,
    mut put: impl FnMut(usize, usize, usize) -> Result<()>,
) -> Result<()> {
    let pixels_per_byte = 8 / bits;
    let row_bytes = tw.div_ceil(pixels_per_byte);
    let mask = (1usize << bits) - 1;
    for row in 0..th {
        let data = cur.take(row_bytes)?;
        let mut bit_pos = 0usize;
        for col in 0..tw {
            let byte = data[bit_pos / 8];
            let shift = 8 - bits - (bit_pos % 8);
            let idx = usize::from(byte >> shift) & mask;
            bit_pos += bits;
            put(row, col, idx)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write as _;

    #[test]
    fn solid_tile_fills_rect() {
        let raw = [1u8, 10, 20, 30];
        let inflated = raw.to_vec();
        let mut fb = vec![0u8; 2 * 2 * 4];
        let mut cursor = Cursor::new(&inflated);
        decode_tile(&mut cursor, 2, 2, |row, col, bgr| {
            let i = (row * 2 + col) * 4;
            fb[i] = bgr[0];
            fb[i + 1] = bgr[1];
            fb[i + 2] = bgr[2];
            fb[i + 3] = 255;
        })
        .unwrap();
        for px in fb.chunks_exact(4) {
            assert_eq!(px, &[10, 20, 30, 255]);
        }
    }

    #[test]
    fn raw_tile_pixels() {
        let mut raw = vec![0u8];
        raw.extend_from_slice(&[1, 2, 3]);
        raw.extend_from_slice(&[4, 5, 6]);
        let mut fb = vec![0u8; 2 * 1 * 4];
        let mut cursor = Cursor::new(&raw);
        decode_tile(&mut cursor, 2, 1, |row, col, bgr| {
            let i = (row * 2 + col) * 4;
            fb[i] = bgr[0];
            fb[i + 1] = bgr[1];
            fb[i + 2] = bgr[2];
            fb[i + 3] = 255;
        })
        .unwrap();
        assert_eq!(&fb[0..4], &[1, 2, 3, 255]);
        assert_eq!(&fb[4..8], &[4, 5, 6, 255]);
    }

    #[test]
    fn oneshot_zlib_also_works_on_fresh_decoder() {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(&[1u8, 1, 2, 3]).unwrap();
        let compressed = e.finish().unwrap();
        let mut dec = ZrleDecoder::new();
        let mut fb = vec![0u8; 4];
        // Complete zlib members end the stream; a fresh decoder can still read one.
        dec.decode_rect(&compressed, 0, 0, 1, 1, 1, &mut fb).unwrap();
        assert_eq!(&fb, &[1, 2, 3, 255]);
    }
}
