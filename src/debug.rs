use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static DEBUG_FILE: OnceLock<Mutex<File>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

pub fn init(path: &str) {
    START.get_or_init(Instant::now);
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => {
            let _ = DEBUG_FILE.set(Mutex::new(file));
        }
        Err(error) => eprintln!("raffi: cannot open debug file {path}: {error}"),
    }
}

pub fn is_enabled() -> bool {
    DEBUG_FILE.get().is_some()
}

/// Milliseconds since the debug log was opened, for startup timings.
pub fn elapsed_ms() -> u128 {
    START.get().map_or(0, |start| start.elapsed().as_millis())
}

pub fn write_debug(msg: &str) {
    if let Some(mutex) = DEBUG_FILE.get() {
        if let Ok(mut f) = mutex.lock() {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = writeln!(f, "[{ts}] {msg}");
        }
    }
}

#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if $crate::debug::is_enabled() {
            $crate::debug::write_debug(&format!($($arg)*))
        }
    };
}
