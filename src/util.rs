use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Null-terminated UTF-16 for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 buffer (optionally null-terminated) to String.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// %APPDATA%\Windock
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("Windock")
}

pub fn log_path() -> PathBuf {
    data_dir().join("windock.log")
}

pub fn log(msg: &str) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let _ = std::fs::create_dir_all(data_dir());
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "[{secs}] {msg}");
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::util::log(&format!($($t)*)) };
}

pub fn init_log() {
    let _ = std::fs::create_dir_all(data_dir());
    // Keep the log from growing forever: start fresh when it gets large.
    if std::fs::metadata(log_path()).map(|m| m.len() > 512 * 1024).unwrap_or(false) {
        let _ = std::fs::remove_file(log_path());
    }
    std::panic::set_hook(Box::new(|info| {
        log(&format!("panic: {info}"));
    }));
}
