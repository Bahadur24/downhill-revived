//! The game's interface (port/render/include/dhd_disc.h): port/host/host_sdl.c calls it on a
//! thread of its own. Strings are UTF-8 and NUL-terminated.

use std::ffi::{c_char, c_int, CStr};
use std::path::Path;

/// # Safety
/// `p` is NULL or a NUL-terminated string that lives for 'a.
unsafe fn text<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller's contract.
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

/// The online check of the disc image at `iso` (`crate::check`), with iso-check.txt at `cache`
/// (NULL: none). On an answer, `digest` gets the game files hash (32 bytes). `note` (`size` bytes,
/// cut to fit) gets what the log says: on an answer, what differs from the USA disc ("" for its
/// files); else why there is none. 1 hashed now, 2 from iso-check.txt, -1 the image can't be read,
/// -2 its game files can't be the USA disc's (no ISO 9660 file system, or a cut dump).
///
/// # Safety
/// `iso` and `cache` are NULL or NUL-terminated strings; `digest` has room for 32 bytes, `note` for
/// `size`.
#[no_mangle]
pub unsafe extern "C" fn dhd_disc_check(
    iso: *const c_char,
    cache: *const c_char,
    digest: *mut u8,
    note: *mut c_char,
    size: usize,
) -> c_int {
    // SAFETY: the caller's contract.
    let (iso, cache) = unsafe { (text(iso), text(cache)) };
    let (code, said) =
        match iso.map(|p| crate::check(cache.map(Path::new), Path::new(p), |_, _| true)) {
            None => (-1, "no disc image".to_string()),
            Some(Ok((files, from_cache))) => {
                // SAFETY: room for 32 bytes.
                unsafe { std::ptr::copy_nonoverlapping(files.sha256.as_ptr(), digest, 32) };
                (if from_cache { 2 } else { 1 }, files.note())
            }
            Some(Err(e @ crate::Error::Format(_))) => (-2, e.to_string()),
            Some(Err(e)) => (-1, e.to_string()),
        };
    if !note.is_null() && size > 0 {
        let n = said.floor_char_boundary(size - 1);
        // SAFETY: room for size bytes; n < size.
        unsafe {
            std::ptr::copy_nonoverlapping(said.as_ptr(), note as *mut u8, n);
            *note.add(n) = 0;
        }
    }
    code
}
