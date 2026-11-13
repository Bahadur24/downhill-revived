//! The browser's interface (port/web/www/settings.js): the crate built on its own as a small wasm
//! module (`rustgs.py build --target wasm32-unknown-unknown --crate dhd_settings`, then
//! build/web/dhd_settings.wasm). It defines and exports its own memory and imports nothing: the page
//! (which shapes the canvas from the settings before the game starts) and the game worker each
//! instantiate it, apart from the game's shared memory, where dhd_render's web build lives.
//!
//! No wasm-bindgen: a plain C interface. The caller copies its arguments into buffers from
//! `dhd_settings_alloc` (UTF-8, or dhd.ini's bytes as stored), and reads each call's result from
//! `dhd_settings_out` / `dhd_settings_out_len` until the next call.

use crate::{edited, environment, Ini};
use std::cell::RefCell;
use std::fmt::Write;
use std::path::Path;

thread_local! {
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn put(bytes: Vec<u8>) {
    OUT.with(|o| *o.borrow_mut() = bytes);
}

/// # Safety
/// `ptr` / `len` are a buffer from `dhd_settings_alloc` (or len 0).
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 {
        return &[];
    }
    // SAFETY: the caller's contract.
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

/// # Safety
/// As `bytes`.
unsafe fn text<'a>(ptr: *const u8, len: usize) -> &'a str {
    // SAFETY: the caller's contract.
    std::str::from_utf8(unsafe { bytes(ptr, len) }).unwrap_or("")
}

fn json(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A buffer of `len` bytes in this module's memory, for an argument.
#[no_mangle]
pub extern "C" fn dhd_settings_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// # Safety
/// `ptr` / `len` came from `dhd_settings_alloc(len)`.
#[no_mangle]
pub unsafe extern "C" fn dhd_settings_free(ptr: *mut u8, len: usize) {
    // SAFETY: the caller's contract: the allocation of that capacity.
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len.max(1)) });
}

/// The last call's result.
#[no_mangle]
pub extern "C" fn dhd_settings_out() -> *const u8 {
    OUT.with(|o| o.borrow().as_ptr())
}

#[no_mangle]
pub extern "C" fn dhd_settings_out_len() -> usize {
    OUT.with(|o| o.borrow().len())
}

/// 1 when DHD_SETTINGS's `name` means no settings at all (none, 0).
///
/// # Safety
/// A buffer as `bytes`.
#[no_mangle]
pub unsafe extern "C" fn dhd_settings_disabled(name: *const u8, name_len: usize) -> i32 {
    // SAFETY: the caller's contract.
    crate::disabled(Path::new(unsafe { text(name, name_len) })) as i32
}

/// The game's start (`environment`): dhd.ini's text (`stored` 0: there is none) as DHD_* variables,
/// none of `set` (the variables already set, one a line). Result: JSON
/// `{"vars": [[name, value], ...], "unknown": [key, ...]}`.
///
/// # Safety
/// Buffers as `bytes`.
#[no_mangle]
pub unsafe extern "C" fn dhd_settings_environment(
    ini: *const u8,
    ini_len: usize,
    stored: i32,
    set: *const u8,
    set_len: usize,
) -> i32 {
    // SAFETY: the caller's contract.
    let (ini, set) = unsafe { (Ini::parse(bytes(ini, ini_len)), text(set, set_len)) };
    let is_set = |name: &str| set.lines().any(|s| s == name);
    let env = environment((stored != 0).then_some(&ini), is_set);
    let mut out = String::from("{\"vars\":[");
    for (i, (name, value)) in env.vars.iter().enumerate() {
        out.push_str(if i > 0 { ",[" } else { "[" });
        json(&mut out, name);
        out.push(',');
        json(&mut out, value);
        out.push(']');
    }
    out.push_str("],\"unknown\":[");
    for (i, key) in env.unknown.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json(&mut out, key);
    }
    out.push_str("]}");
    put(out.into_bytes());
    1
}

/// The text after saving `key = value` into dhd.ini's text (`edited`; `stored` 0: none yet, which
/// starts as the crate's header). Result: the new text's bytes, or the reason it can't be saved
/// (then 0).
///
/// # Safety
/// Buffers as `bytes`.
#[no_mangle]
pub unsafe extern "C" fn dhd_settings_save(
    ini: *const u8,
    ini_len: usize,
    stored: i32,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
) -> i32 {
    // SAFETY: the caller's contract.
    let (ini, key, value) = unsafe {
        (
            bytes(ini, ini_len),
            text(key, key_len),
            text(value, value_len),
        )
    };
    match edited((stored != 0).then_some(ini), key, value) {
        Ok(ini) => {
            put(ini.bytes().to_vec());
            1
        }
        Err(e) => {
            put(e.into_bytes());
            0
        }
    }
}

/// The value dhd.ini's text gives `key`, as written (`Ini::written`). 1 when it has one (the
/// result), else 0.
///
/// # Safety
/// Buffers as `bytes`.
#[no_mangle]
pub unsafe extern "C" fn dhd_settings_written(
    ini: *const u8,
    ini_len: usize,
    key: *const u8,
    key_len: usize,
) -> i32 {
    // SAFETY: the caller's contract.
    let (ini, key) = unsafe { (Ini::parse(bytes(ini, ini_len)), text(key, key_len)) };
    match ini.written(key) {
        Some(v) => {
            put(v.into_bytes());
            1
        }
        None => {
            put(Vec::new());
            0
        }
    }
}
