//! `dhdr_test_presented` (task HK): the last presented surface texture, read back through
//! COPY_SRC before it was presented, as the retired `gpu_wgpu.c`'s read_presented (last at
//! render/rust 3882d5576). `ffi.rs`'s
//! `dhdr_surface_present` calls `capture` on the frame just before presenting it (exact and native
//! frames alike), when the surface offers COPY_SRC.
//!
//! The copy: the whole texture (mip 0, all aspects) to a MAP_READ buffer with rows of
//! `width * 4` bytes rounded up to 256, submitted after the frame's own drawing, then a blocking map;
//! the rows are packed to `width * height` RGBA8 words (the surface format, unconverted, as the C
//! path keeps it). A failed map logs and leaves no picture (NULL), so the caller's check fails.
use crate::device::Gpu;
use std::sync::Mutex;

struct Presented {
    pixels: Vec<u32>,
    width: u32,
    height: u32,
}

static LAST: Mutex<Presented> = Mutex::new(Presented {
    pixels: Vec::new(),
    width: 0,
    height: 0,
});

/// Reads `frame` back (blocking) and keeps it as the last presented picture.
pub fn capture(gpu: &Gpu, frame: &wgpu::Texture) {
    let (width, height) = (frame.width(), frame.height());
    let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
    *last = Presented {
        pixels: Vec::new(),
        width: 0,
        height: 0,
    };
    if width == 0 || height == 0 {
        return;
    }
    let row = width as u64 * 4;
    let stride = (row + 255) & !255;
    let size = stride * height as u64;
    let read = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("present readback"),
        size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: frame,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &read,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride as u32),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([encoder.finish()]);
    let bytes = match super::depth_probe::map_read(gpu, &read, size) {
        Ok(b) => b,
        Err(e) => {
            crate::host::log(&format!("dhd_render: dhdr_test_presented: {e}"));
            return;
        }
    };
    let mut pixels = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height as usize {
        let line = &bytes[y * stride as usize..y * stride as usize + row as usize];
        pixels.extend(
            line.chunks_exact(4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        );
    }
    *last = Presented {
        pixels,
        width,
        height,
    };
}

/// The last presented picture: (pixels, width, height); NULL, 0, 0 when there is none. The
/// pixels stay valid until the next capture.
pub fn last() -> (*const u32, u32, u32) {
    let last = LAST.lock().unwrap_or_else(|p| p.into_inner());
    if last.pixels.is_empty() {
        (std::ptr::null(), 0, 0)
    } else {
        (last.pixels.as_ptr(), last.width, last.height)
    }
}
