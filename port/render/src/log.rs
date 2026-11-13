//! The `log` crate's logger: wgpu's and naga's diagnostics, and the renderer's own, to the
//! host (`host.rs`), up to the level `DhdrDeviceDesc.log_level` asks for (`DHDR_LOG_*`; errors
//! only until the device sets it).
use crate::abi::{DHDR_LOG_ERROR, DHDR_LOG_INFO, DHDR_LOG_WARN};
use std::sync::atomic::{AtomicU32, Ordering};

static LEVEL: AtomicU32 = AtomicU32::new(DHDR_LOG_ERROR);

struct HostLogger;

fn filter(level: u32) -> ::log::LevelFilter {
    match level {
        0 => ::log::LevelFilter::Off,
        DHDR_LOG_ERROR => ::log::LevelFilter::Error,
        DHDR_LOG_WARN => ::log::LevelFilter::Warn,
        DHDR_LOG_INFO => ::log::LevelFilter::Info,
        _ => ::log::LevelFilter::Info,
    }
}

impl ::log::Log for HostLogger {
    fn enabled(&self, m: &::log::Metadata) -> bool {
        // discord-rich-presence's own lines (one per try while Discord isn't running) stay out:
        // dhd_presence logs what the player needs, once
        m.level() <= filter(LEVEL.load(Ordering::Relaxed))
            && !m.target().starts_with("discord_rich_presence")
    }
    fn log(&self, r: &::log::Record) {
        if self.enabled(r.metadata()) {
            crate::host::log(&format!(
                "dhd_render: {} {}: {}",
                r.level(),
                r.target(),
                r.args()
            ));
        }
    }
    fn flush(&self) {}
}

static LOGGER: HostLogger = HostLogger;

/// Installs the logger once (`guard::install_hooks` calls it). A logger set by someone else first
/// (a test harness) stays.
pub fn install() {
    if ::log::set_logger(&LOGGER).is_ok() {
        ::log::set_max_level(filter(LEVEL.load(Ordering::Relaxed)));
    }
}

/// The most verbose level forwarded (`DHDR_LOG_*`; 0 = none). The device calls it at init.
pub fn set_level(level: u32) {
    LEVEL.store(level, Ordering::Relaxed);
    ::log::set_max_level(filter(level));
}

/// The error of a module function whose task has not landed yet. Skeleton stubs (F0b) return it,
/// so every unimplemented path fails cleanly: module functions return `Err(message)` and the
/// export that called them logs it (`ffi.rs`); they do not log their own errors.
pub fn unimplemented(what: &str) -> String {
    format!("{what}: not implemented yet")
}

/// A skeleton stub with no error path: logs, then panics (in the library the abort profile ends
/// the process after the panic hook has logged; in a test the test fails). Only reached once the
/// object it belongs to exists, which the skeleton's creates never allow.
pub fn todo(what: &str) -> ! {
    crate::host::log(&format!("dhd_render: {what}: not implemented yet"));
    panic!("{what}: not implemented yet")
}
