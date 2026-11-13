//! The texture packs' PNG files (`port/gs/texpack.c`, finding 057), decoded with the png crate:
//! one decoder on every native platform. Any colour type, bit depth and interlacing becomes 8-bit
//! RGBA rows: palettes and grey expanded, tRNS made alpha, 16-bit samples cut to their high byte
//! (libpng's `png_set_strip_16`). A PNG with no alpha (no alpha channel, no tRNS) gets `opaque`
//! as its alpha: the GS's 128 for a pack, as PCSX2's loader does for RGB files.
//!
//! C reads the file (the host's file layer, UTF-8 paths) and owns the buffers: `size` reads the
//! header, C makes room (the mip levels follow the picture there), `decode` fills the first
//! w x h x 4 bytes. No file, environment or allocation crosses the boundary.
use std::io::Cursor;

use png::{BitDepth, ColorType, Decoder, Limits, Transformations};

/// The picture's width and height, from its header.
pub fn size(data: &[u8]) -> Result<(u32, u32), String> {
    let reader = Decoder::new(Cursor::new(data))
        .read_info()
        .map_err(|e| e.to_string())?;
    let info = reader.info();
    Ok((info.width, info.height))
}

/// Decodes into `out` (at least w * h * 4 bytes; w x h is what `size` gave): RGBA8 rows.
pub fn decode(data: &[u8], out: &mut [u8], w: u32, h: u32, opaque: u8) -> Result<(), String> {
    // The caller bounds the size (texpack.c: 4096 a side); the crate's own 64 MiB default would
    // refuse a 4096 x 4096 RGBA picture with 16-bit samples.
    let mut decoder = Decoder::new_with_limits(Cursor::new(data), Limits { bytes: usize::MAX });
    decoder.set_transformations(Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (iw, ih) = (reader.info().width, reader.info().height);
    if (iw, ih) != (w, h) {
        return Err(format!("{iw} x {ih}, not the {w} x {h} asked for"));
    }
    let pixels = w as usize * h as usize;
    let (color, depth) = reader.output_color_type();
    if depth != BitDepth::Eight {
        return Err(format!("{depth:?} samples after normalising"));
    }
    let channels = color.samples();
    let rows = reader.output_buffer_size().ok_or("too large")?;
    if out.len() < pixels * 4 || rows != pixels * channels {
        return Err(format!("{} bytes for {w} x {h} {color:?}", out.len()));
    }
    reader
        .next_frame(&mut out[..rows])
        .map_err(|e| e.to_string())?;
    // Widen in place, from the end, so no sample is overwritten before it is read.
    match color {
        ColorType::Rgba => {}
        ColorType::Rgb => {
            for i in (0..pixels).rev() {
                let (r, g, b) = (out[i * 3], out[i * 3 + 1], out[i * 3 + 2]);
                out[i * 4..i * 4 + 4].copy_from_slice(&[r, g, b, opaque]);
            }
        }
        ColorType::GrayscaleAlpha => {
            for i in (0..pixels).rev() {
                let (v, a) = (out[i * 2], out[i * 2 + 1]);
                out[i * 4..i * 4 + 4].copy_from_slice(&[v, v, v, a]);
            }
        }
        ColorType::Grayscale => {
            for i in (0..pixels).rev() {
                let v = out[i];
                out[i * 4..i * 4 + 4].copy_from_slice(&[v, v, v, opaque]);
            }
        }
        ColorType::Indexed => return Err("a palette left after expanding".into()),
    }
    Ok(())
}
