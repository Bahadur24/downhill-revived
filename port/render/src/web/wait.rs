//! The pending read (task B1): a read's or capture's begin leaves the map's promise in one slot
//! (its own `RefCell`, separate from the `Owned` object); `dhdr_web_pending()`, a wasm-bindgen
//! export, hands it to the reactor's Suspending `web_gpu_await()` import, so Rust is never on the
//! stack while the reactor is suspended.
//!
//! The browser's split read (plan 1.4, finding 085 S4):
//!
//! 1. C calls a begin (`dhdr_exact_read_begin`, `dhdr_exact_capture_begin`). It submits and maps
//!    through `map_read`, which opens the slot (a new promise) and starts `map_async`; the begin
//!    returns at once.
//! 2. C calls its own Suspending import `web_gpu_await()`; JS implements it as
//!    `new WebAssembly.Suspending(() => R.dhdr_web_pending())`. The promising root suspends; the
//!    event loop runs; the map's callback (the begin's own status update first) settles the
//!    promise with 1 (mapped) or 0 (failed). The JSPI resumes C with that number.
//! 3. C calls the finish, which reads the map's status the begin kept and copies.
//!
//! The promise always settles and is never rejected: a map that fails settles 0, device loss
//! settles 0 (`lost`), and a begin that opens a new wait while an older one is still unsettled
//! settles the older one 0 first. With nothing in flight (a begin that had nothing to map), the
//! promise is an already resolved 1. Only the map callback, the device-lost callback and the
//! exports touch the slot, each on the worker's one thread and never while another holds it.

#[cfg(feature = "web")]
use std::cell::RefCell;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "web")]
use js_sys::{Function, Promise};
#[cfg(feature = "web")]
use wasm_bindgen::prelude::*;

/// The one wait in flight.
#[cfg(feature = "web")]
#[derive(Default)]
struct Slot {
    /// Bumped by every `open`, so a late callback of an older wait settles nothing.
    generation: u32,
    /// The promise `dhdr_web_pending` hands out (taken by it), until the next `open`.
    promise: Option<Promise>,
    /// Its resolve function, until it is settled.
    resolve: Option<Function>,
}

#[cfg(feature = "web")]
thread_local! {
    static SLOT: RefCell<Slot> = RefCell::new(Slot::default());
}

/// Set by the device-lost callback (`web/init.rs`): reads refuse to begin after it.
static LOST: AtomicBool = AtomicBool::new(false);

/// A promise and its resolve function.
#[cfg(feature = "web")]
fn resolver() -> (Promise, Function) {
    let mut resolve = None;
    let promise = Promise::new(&mut |r, _reject| resolve = Some(r));
    // The executor runs inside Promise::new, so resolve is set.
    (
        promise,
        resolve.unwrap_or_else(|| {
            crate::guard::fatal("dhd_render: Promise::new did not run its executor")
        }),
    )
}

/// Settles `resolve` with 1 (`ok`) or 0. Called with the slot not borrowed.
#[cfg(feature = "web")]
fn call(resolve: Function, ok: bool) {
    let _ = resolve.call1(&JsValue::NULL, &JsValue::from(ok as i32));
}

/// Opens a new wait (settling an unsettled older one with 0) and returns its generation.
#[cfg(feature = "web")]
fn open() -> u32 {
    let (promise, resolve) = resolver();
    let (generation, older) = SLOT.with(|s| {
        let mut s = s.borrow_mut();
        s.generation = s.generation.wrapping_add(1);
        s.promise = Some(promise);
        (s.generation, s.resolve.replace(resolve))
    });
    if let Some(r) = older {
        call(r, false);
    }
    generation
}

/// Settles wait `generation` with `ok`, if it is still the one in flight and unsettled.
#[cfg(feature = "web")]
fn settle(generation: u32, ok: bool) {
    let resolve = SLOT.with(|s| {
        let mut s = s.borrow_mut();
        if s.generation == generation {
            s.resolve.take()
        } else {
            None
        }
    });
    if let Some(r) = resolve {
        call(r, ok);
    }
}

/// Maps `range` of `buffer` for reading as a browser begin does: opens the wait, then starts the
/// map; when it resolves, `on_mapped` runs with the result first (the begin's own status), then the
/// wait settles (1 mapped, 0 failed). Returns at once. Readbacks (`exact/readback.rs`) and the
/// display capture (`exact/display.rs`) map through this in the browser.
pub fn map_read<F>(buffer: &wgpu::Buffer, range: Range<u64>, on_mapped: F)
where
    F: FnOnce(Result<(), wgpu::BufferAsyncError>) + wgpu::WasmNotSend + 'static,
{
    #[cfg(feature = "web")]
    {
        let generation = open();
        buffer.map_async(wgpu::MapMode::Read, range, move |r| {
            let ok = r.is_ok();
            on_mapped(r);
            settle(generation, ok);
        });
    }
    #[cfg(not(feature = "web"))]
    buffer.map_async(wgpu::MapMode::Read, range, on_mapped);
}

/// The device is lost (the device-lost callback): every later begin refuses (0) and the wait in
/// flight, if any, settles 0 (its map fails too; the finish then reports it).
pub fn set_lost() {
    LOST.store(true, Ordering::Release);
    #[cfg(feature = "web")]
    settle(SLOT.with(|s| s.borrow().generation), false);
}

/// The device was lost: a begin returns 0 (logged by the caller), so no promise is left to hang.
pub fn lost() -> bool {
    LOST.load(Ordering::Acquire)
}

/// Copies `[from, from + dst.len())` of a mapped range into `dst`. In the browser the bytes come
/// straight from the mapped `ArrayBuffer` (wgpu's `Deref` of a view would first copy the whole
/// mapped range into a wasm `Vec`; for a 4 MiB read that is a second 4 MiB copy and allocation).
pub fn copy_mapped(view: &wgpu::BufferView, from: usize, dst: &mut [u8]) {
    #[cfg(feature = "web")]
    {
        let start = from as u32;
        view.as_uint8array()
            .subarray(start, start + dst.len() as u32)
            .copy_to(dst);
    }
    #[cfg(not(feature = "web"))]
    dst.copy_from_slice(&view[from..from + dst.len()]);
}

/// Waits until the queue has finished every command submitted so far (`web/init.rs`'s prewarm).
#[cfg(feature = "web")]
pub async fn work_done(queue: &wgpu::Queue) {
    thread_local! {
        /// The resolve functions of the work-done waits, by id (the callback must be Send, so it
        /// carries only the id).
        static DONE: RefCell<Vec<(u32, Function)>> = const { RefCell::new(Vec::new()) };
    }
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (promise, resolve) = resolver();
    DONE.with(|d| d.borrow_mut().push((id, resolve)));
    queue.on_submitted_work_done(move || {
        let r = DONE.with(|d| {
            let mut d = d.borrow_mut();
            d.iter()
                .position(|(i, _)| *i == id)
                .map(|at| d.swap_remove(at).1)
        });
        if let Some(r) = r {
            call(r, true);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// `dhdr_web_pending`: the promise of the read in flight. It settles to 1 (mapped) or 0 (failed),
/// never rejects; with nothing in flight it is an already resolved 1. JS calls it once per
/// `web_gpu_await()`.
#[cfg(feature = "web")]
#[wasm_bindgen]
pub fn dhdr_web_pending() -> js_sys::Promise {
    SLOT.with(|s| s.borrow_mut().promise.take())
        .unwrap_or_else(|| Promise::resolve(&JsValue::from(1)))
}
