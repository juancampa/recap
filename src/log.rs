//! The log file that holds the messages which the program must not write on
//! the terminal.
//!
//! The TUI owns the screen. A write to stdout or to stderr paints a cell that
//! the renderer believes is empty. The renderer compares each new frame
//! against its own copy of the screen, so it sends nothing for that cell and
//! the text stays on the screen. The text goes away only when the panel draws
//! other text over the same cell.
//!
//! Every code path that can run while the TUI is on the screen must call
//! [`crate::log_warn!`] in place of `eprintln!`. The loader threads are the
//! main example: they run for minutes after the first frame.

use chrono::Local;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// The largest log file that the program keeps. The next write after this
/// size replaces the file.
const MAX_LOG_BYTES: u64 = 1 << 20;

/// The path of the log file: `~/.cache/recap/recap.log`.
pub fn log_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".cache/recap/recap.log"))
}

/// Append one line to the log file. A failure gets no report, because the
/// program has no other place to write the message.
pub fn write(message: &str) {
    let Some(path) = log_path() else {
        return;
    };
    write_to(&path, message);
}

/// Append `message` to the file at `path`. The caller gets no result: the
/// program has no other place to write the message.
fn write_to(path: &std::path::Path, message: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let too_big = std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES);
    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(!too_big)
        .write(too_big)
        .truncate(too_big)
        .open(path)
    else {
        return;
    };
    let stamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    let _ = writeln!(file, "{stamp} {message}");
}

/// Write one formatted line to the log file. This macro takes the arguments
/// that `eprintln!` takes.
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::write(&format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log file keeps every line until it passes `MAX_LOG_BYTES`. The
    /// next write then replaces the file, so the log never grows without a
    /// limit.
    #[test]
    fn the_log_file_grows_until_the_size_limit_replaces_it() {
        let dir = std::env::temp_dir().join(format!("recap-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("recap.log");

        write_to(&path, "first");
        write_to(&path, "second");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("first") && text.contains("second"), "{text}");

        let filler = "x".repeat(MAX_LOG_BYTES as usize + 1);
        write_to(&path, &filler);
        write_to(&path, "after the limit");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("first"), "the write past the limit replaces the file");
        assert!(text.contains("after the limit"), "{}", &text[..40.min(text.len())]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
