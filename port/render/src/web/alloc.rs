//! The browser module's allocator (task B1): a bounded `#[global_allocator]` over
//! `[__heap_base, 0x69000000)` in the core's imported memory, trapping on exhaustion without
//! touching anything outside that window (finding 085, S4; the G1 memory window).
//!
//! The renderer shares the core's one memory (`--import-memory`). Its linker window is data from
//! 0x60000000 (`--global-base`), a 1 MiB stack above it (`--no-stack-first`), then the heap up to
//! the module's own `--initial-memory` (0x69000000). Everything outside belongs to the core and the
//! C reactor. std's default wasm allocator grows the memory and would take pages above the core's
//! initial size, so this one never calls `memory.grow`: it hands out only the window, and when the
//! window is full it logs one line and returns null, so std's alloc-error path aborts (an
//! `unreachable` trap) before any byte outside is touched.
//!
//! Design: small requests (up to 256 bytes, alignment up to 16) come from 16 size classes of 16
//! bytes each, kept on per-class free lists (LIFO, never merged back: they are reused by the same
//! sizes, which is what wgpu's and wasm-bindgen's small objects do). Everything else is first fit
//! over one address-ordered free list of 16-byte-aligned spans, split on allocation and coalesced
//! with both neighbours on free. A large block keeps a 16-byte header just below the pointer it
//! returns: the span's start and length (alignment above 16 may leave a gap in front, which the
//! header lets free give back). `dealloc` gets the layout from std, so small blocks need no header.
//!
//! wasm32 without the atomics feature is single-threaded (the browser module's worker); the
//! state is in `Cell`s. A build with atomics is refused at compile time.
#[cfg(target_feature = "atomics")]
compile_error!("web/alloc.rs is single-threaded: the browser module is built without atomics");

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::ptr::null_mut;

/// The top of the Rust window: the renderer's `--initial-memory` (G1's memory window).
pub const HEAP_TOP: usize = 0x6900_0000;
const ALIGN: usize = 16;
/// A large block's header (span start, span length), padded to `ALIGN`.
const HEADER: usize = 16;
/// Small classes: 16, 32, ... 256 bytes.
const CLASS_STEP: usize = 16;
const CLASSES: usize = 16;
const SMALL_MAX: usize = CLASS_STEP * CLASSES;
// Every span start and length is a multiple of ALIGN, so a split leaves either nothing or a
// remainder that can hold a span; and a large block's header fits below its pointer.
const _: () =
    assert!(std::mem::size_of::<Span>() <= ALIGN && std::mem::size_of::<[usize; 2]>() <= HEADER);

extern "C" {
    /// wasm-ld's end of the stack and static data: the heap starts here.
    static __heap_base: u8;
}

/// A free span of the large list (at its own start).
#[repr(C)]
struct Span {
    len: usize,
    next: *mut Span,
}

/// A free small block (at its own start).
#[repr(C)]
struct Small {
    next: *mut Small,
}

pub struct Bounded {
    /// First fit, address ordered.
    spans: Cell<*mut Span>,
    classes: [Cell<*mut Small>; CLASSES],
    ready: Cell<bool>,
    used: Cell<usize>,
    peak: Cell<usize>,
    failures: Cell<usize>,
}

// SAFETY: the browser module has one thread (no atomics, checked above); nothing is shared.
unsafe impl Sync for Bounded {}

/// What the allocator has done so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// The heap's first byte (`__heap_base`, aligned) and one past its last (`HEAP_TOP`).
    pub base: usize,
    pub top: usize,
    /// Bytes handed out now (headers, class rounding and gaps included), and the most ever.
    pub used: usize,
    pub peak: usize,
    /// Requests refused (the window was full, or a size overflowed).
    pub failures: usize,
}

const fn round_up(v: usize, to: usize) -> usize {
    (v + to - 1) & !(to - 1)
}

/// The class of a small request, or None for the large list.
fn class_of(layout: Layout) -> Option<usize> {
    if layout.align() <= ALIGN && layout.size() <= SMALL_MAX {
        Some(layout.size().max(1).div_ceil(CLASS_STEP) - 1)
    } else {
        None
    }
}

impl Bounded {
    pub const fn new() -> Bounded {
        const EMPTY: Cell<*mut Small> = Cell::new(null_mut());
        Bounded {
            spans: Cell::new(null_mut()),
            classes: [EMPTY; CLASSES],
            ready: Cell::new(false),
            used: Cell::new(0),
            peak: Cell::new(0),
            failures: Cell::new(0),
        }
    }

    /// The heap's first byte: `__heap_base`, rounded up to `ALIGN`.
    pub fn base() -> usize {
        // Only the symbol's address is taken (safe); nothing is read through it.
        round_up(std::ptr::addr_of!(__heap_base) as usize, ALIGN)
    }

    pub fn stats(&self) -> Stats {
        Stats {
            base: Self::base(),
            top: HEAP_TOP,
            used: self.used.get(),
            peak: self.peak.get(),
            failures: self.failures.get(),
        }
    }

    /// The whole window as one free span (first use).
    fn init(&self) {
        let base = Self::base();
        if base + ALIGN <= HEAP_TOP {
            let s = base as *mut Span;
            // SAFETY: [base, HEAP_TOP) is the heap window, owned by this allocator alone.
            unsafe {
                s.write(Span {
                    len: HEAP_TOP - base,
                    next: null_mut(),
                })
            };
            self.spans.set(s);
        }
        self.ready.set(true);
    }

    fn took(&self, bytes: usize) {
        self.used.set(self.used.get() + bytes);
        if self.used.get() > self.peak.get() {
            self.peak.set(self.used.get());
        }
    }

    fn refuse(&self, layout: Layout) -> *mut u8 {
        self.failures.set(self.failures.get() + 1);
        // host::log on wasm hands the str's pointer and length to the console import: it
        // allocates nothing, so it is safe here.
        let _ = layout;
        crate::host::log("dhd_render: the browser module's heap is full (out of memory in [__heap_base, 0x69000000)); aborting");
        null_mut()
    }

    /// Takes `need` bytes (a multiple of `ALIGN`) at an `align`-aligned address, first fit, with
    /// `HEADER` bytes below it when `header`. Returns (span start, span length, pointer): the
    /// block is [start, start + length), the caller's bytes [pointer, pointer + need).
    ///
    /// # Safety
    /// The large list is well formed (only this allocator writes it).
    unsafe fn take(
        &self,
        need: usize,
        align: usize,
        header: bool,
    ) -> Option<(usize, usize, usize)> {
        let mut prev: *mut Span = null_mut();
        let mut cur = self.spans.get();
        while !cur.is_null() {
            let start = cur as usize;
            // SAFETY: cur is a free span of the list.
            let (len, next) = unsafe { ((*cur).len, (*cur).next) };
            let ptr = round_up(start + if header { HEADER } else { 0 }, align);
            let end = start + len;
            if let Some(stop) = ptr.checked_add(need) {
                if stop <= end {
                    // The block is [start, stop); the rest (a multiple of ALIGN) stays free.
                    let (taken, rest) = if end > stop {
                        let r = stop as *mut Span;
                        // SAFETY: [stop, end) is inside the free span being taken apart.
                        unsafe {
                            r.write(Span {
                                len: end - stop,
                                next,
                            })
                        };
                        (stop - start, r)
                    } else {
                        (len, next)
                    };
                    if prev.is_null() {
                        self.spans.set(rest);
                    } else {
                        // SAFETY: prev is a free span of the list.
                        unsafe { (*prev).next = rest };
                    }
                    return Some((start, taken, ptr));
                }
            }
            prev = cur;
            cur = next;
        }
        None
    }

    /// Gives [start, start + len) back to the large list, merged with its neighbours.
    ///
    /// # Safety
    /// The span was taken from the list and is no longer used.
    unsafe fn give(&self, start: usize, len: usize) {
        let block = start as *mut Span;
        let mut prev: *mut Span = null_mut();
        let mut cur = self.spans.get();
        while !cur.is_null() && (cur as usize) < start {
            prev = cur;
            // SAFETY: cur is a free span of the list.
            cur = unsafe { (*cur).next };
        }
        // SAFETY: block is the freed span; prev and cur are free spans of the list.
        unsafe {
            block.write(Span { len, next: cur });
            if !cur.is_null() && start + len == cur as usize {
                (*block).len += (*cur).len;
                (*block).next = (*cur).next;
            }
            if prev.is_null() {
                self.spans.set(block);
            } else if prev as usize + (*prev).len == start {
                (*prev).len += (*block).len;
                (*prev).next = (*block).next;
            } else {
                (*prev).next = block;
            }
        }
    }
}

// SAFETY: every block handed out lies inside [__heap_base, HEAP_TOP), is aligned as asked, and is
// handed out once until it is freed (the free lists hold only free blocks).
unsafe impl GlobalAlloc for Bounded {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.ready.get() {
            self.init();
        }
        if let Some(c) = class_of(layout) {
            let size = (c + 1) * CLASS_STEP;
            let head = self.classes[c].get();
            if !head.is_null() {
                // SAFETY: head is a free block of this class.
                self.classes[c].set(unsafe { (*head).next });
                self.took(size);
                return head as *mut u8;
            }
            // A new block of the class: exactly `size` bytes cut from the large list, no header.
            // SAFETY: the large list is well formed.
            return match unsafe { self.take(size, ALIGN, false) } {
                Some((_, taken, ptr)) => {
                    debug_assert_eq!(taken, size);
                    self.took(size);
                    ptr as *mut u8
                }
                None => self.refuse(layout),
            };
        }
        let align = layout.align().max(ALIGN);
        let Some(need) = layout
            .size()
            .checked_add(ALIGN - 1)
            .map(|n| n & !(ALIGN - 1))
        else {
            return self.refuse(layout);
        };
        // SAFETY: the large list is well formed.
        match unsafe { self.take(need.max(ALIGN), align, true) } {
            Some((start, taken, ptr)) => {
                // SAFETY: [ptr - HEADER, ptr) is inside the taken span, above its start.
                unsafe { ((ptr - HEADER) as *mut [usize; 2]).write([start, taken]) };
                self.took(taken);
                ptr as *mut u8
            }
            None => self.refuse(layout),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(c) = class_of(layout) {
            let b = ptr as *mut Small;
            // SAFETY: ptr is a block of class c that std gives back.
            unsafe {
                b.write(Small {
                    next: self.classes[c].get(),
                })
            };
            self.classes[c].set(b);
            self.used.set(self.used.get() - (c + 1) * CLASS_STEP);
            return;
        }
        // SAFETY: a large block's header sits just below its pointer (alloc).
        let [start, taken] = unsafe { ((ptr as usize - HEADER) as *const [usize; 2]).read() };
        self.used.set(self.used.get() - taken);
        // SAFETY: the span was taken by alloc and std no longer uses it.
        unsafe { self.give(start, taken) };
    }
}

#[global_allocator]
static HEAP: Bounded = Bounded::new();

/// The allocator's numbers (diagnostics).
pub fn stats() -> Stats {
    HEAP.stats()
}
