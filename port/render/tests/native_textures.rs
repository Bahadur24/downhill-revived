//! Native textures (task N3's referee).
//!
//!     python port/tools/rustgs.py test --target x86_64-pc-windows-msvc -- --test native_textures
//!
//! On a DX12 hardware device (FXC): a decoded texture of an odd size and a replacement with a full
//! mip chain are read back level by level and must equal what went in; the `DhdrTexture` handle
//! counts references through `texture_retain` / `texture_release`; textures are made and dropped on
//! several threads at once.
#![cfg(all(feature = "native", windows))]

mod common;
use common::*;
use dhd_render::native::{
    mip_bytes, texture_handle, texture_ref, texture_release, texture_retain, textures::Tex,
};
use std::sync::Arc;

/// Distinct pixels: every byte depends on the index and the seed.
fn pattern(count: usize, seed: u32) -> Vec<u32> {
    (0..count as u32)
        .map(|i| (i.wrapping_mul(2654435761) ^ seed.wrapping_mul(40503)).rotate_left(seed % 32))
        .collect()
}

fn bytes(p: &[u32]) -> Vec<u8> {
    p.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[test]
fn decoded_round_trip() {
    let g = gpu(Adapter::Hardware, Compiler::Fxc);
    let (w, h) = (37, 19);
    let pixels = pattern(w * h, 1);
    let t = no_errors(g, "decoded texture", || {
        Tex::create(g, &pixels, w as u32, h as u32)
    })
    .unwrap_or_else(|e| panic!("Tex::create: {e}"));
    assert_eq!(t.size(), (37, 19, 1));
    assert_eq!(t.groups(), 1, "a decoded texture has one bind group");
    let back = t
        .read_level(g, 0)
        .unwrap_or_else(|e| panic!("read_level: {e}"));
    assert_eq!(back, bytes(&pixels), "the decoded texture read back");
}

#[test]
fn replacement_mips() {
    let g = gpu(Adapter::Hardware, Compiler::Fxc);
    let (w, h, levels) = (64u32, 16u32, 7u32);
    let total = mip_bytes(w, h, levels).expect("64 x 16 has 7 levels");
    assert_eq!(total, 4 * (64 * 16 + 32 * 8 + 16 * 4 + 8 * 2 + 4 + 2 + 1));
    assert_eq!(mip_bytes(w, h, 8), None, "no eighth level");
    let mips = bytes(&pattern(total / 4, 2));
    let t = no_errors(g, "replacement", || Tex::replace(g, &mips, w, h, levels))
        .unwrap_or_else(|e| panic!("Tex::replace: {e}"));
    assert_eq!(t.size(), (64, 16, 7));
    assert_eq!(
        t.groups(),
        8,
        "a replacement has a bind group per replacement sampler"
    );
    let mut at = 0;
    for level in 0..levels {
        let (lw, lh) = ((w >> level).max(1), (h >> level).max(1));
        let n = (lw * lh * 4) as usize;
        let back = t
            .read_level(g, level)
            .unwrap_or_else(|e| panic!("read_level {level}: {e}"));
        assert_eq!(back, mips[at..at + n], "level {level} ({lw} x {lh})");
        at += n;
    }
    assert_eq!(at, total);
}

#[test]
fn handle_references() {
    let g = gpu(Adapter::Hardware, Compiler::Fxc);
    let t = Arc::new(
        Tex::create(g, &pattern(4 * 4, 3), 4, 4).unwrap_or_else(|e| panic!("Tex::create: {e}")),
    );
    let weak = Arc::downgrade(&t);
    let h = texture_handle(t);
    assert!(!h.is_null());
    // SAFETY: h is a live handle throughout, until its last release.
    unsafe {
        assert_eq!(texture_ref(h).expect("a live handle").size(), (4, 4, 1));
        texture_retain(h);
        assert_eq!(weak.strong_count(), 2);
        texture_release(h);
        assert_eq!(weak.strong_count(), 1);
        assert!(weak.upgrade().is_some());
        texture_release(h);
        assert!(texture_ref(std::ptr::null_mut()).is_none());
        texture_retain(std::ptr::null_mut());
        texture_release(std::ptr::null_mut());
    }
    assert!(
        weak.upgrade().is_none(),
        "the last release drops the texture"
    );
}

#[test]
fn threads() {
    let g = gpu(Adapter::Hardware, Compiler::Fxc);
    let made: Vec<Arc<Tex>> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..4u32)
            .map(|i| {
                s.spawn(move || {
                    let mut kept = Vec::new();
                    for j in 0..8u32 {
                        let (w, h) = (8 + i + j, 5 + j);
                        let t = Tex::create(g, &pattern((w * h) as usize, i * 8 + j), w, h)
                            .unwrap_or_else(|e| panic!("thread {i} texture {j}: {e}"));
                        assert_eq!(t.size(), (w, h, 1));
                        if j % 2 == 0 {
                            kept.push(Arc::new(t));
                        }
                    }
                    kept
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("a worker panicked"))
            .collect()
    });
    assert_eq!(made.len(), 16);
    // Read on this thread what the others made, and drop them here.
    for (n, t) in made.iter().enumerate() {
        let (i, j) = (n as u32 / 4, (n as u32 % 4) * 2);
        let (w, h, _) = t.size();
        assert_eq!((w, h), (8 + i + j, 5 + j));
        let back = t
            .read_level(g, 0)
            .unwrap_or_else(|e| panic!("read_level: {e}"));
        assert_eq!(
            back,
            bytes(&pattern((w * h) as usize, i * 8 + j)),
            "texture {n}"
        );
    }
}
