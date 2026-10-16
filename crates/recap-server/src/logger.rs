//! The logger of the server: one line on stderr for each record, with the
//! time, the level and the module.
//!
//! ```text
//! 2026-09-28 21:14:12.753 INFO  http: GET /v1/history?project=/x 200 12.3ms 81.2 KB
//! ```
//!
//! launchd writes stderr to `~/Library/Logs/recap/server.log` (see `recap
//! service`). A test server in a terminal writes it on the terminal.
//!
//! `RECAP_LOG` sets the level of the recap modules: `error`, `warn`, `info`
//! (the default), `debug` or `trace`. Other crates log only warnings and
//! errors.
//!
//! The TUI does not call [`init`]. The TUI owns the screen, and a line on
//! stderr would stay on the screen (see the TUI renderer). Without a logger,
//! the `log` macros write nothing.

use chrono::Local;
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;

/// The largest log file that the logger keeps. When stderr is a regular
/// file of this size, the logger empties it before the next line.
const MAX_LOG_BYTES: u64 = 10 << 20;

/// The environment variable that sets the level.
const LEVEL_VAR: &str = "RECAP_LOG";

struct Logger {
    level: LevelFilter,
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        if metadata.target().starts_with("recap") {
            metadata.level() <= self.level
        } else {
            metadata.level() <= Level::Warn
        }
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let target = record.target();
        let target = target.strip_prefix("recap_server::").unwrap_or(target);
        let line = format!(
            "{} {:<5} {target}: {}\n",
            Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level(),
            record.args()
        );
        let stderr = std::io::stderr();
        let mut stderr = stderr.lock();
        limit_size(&stderr);
        // A failed write gets no report: the log is the place for reports.
        let _ = stderr.write_all(line.as_bytes());
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// Empty the log file when stderr is a regular file larger than
/// [`MAX_LOG_BYTES`]. launchd opens the file in append mode, so the next
/// line goes to the start of the empty file.
fn limit_size(stderr: &impl AsFd) {
    let Ok(fd) = stderr.as_fd().try_clone_to_owned() else {
        return;
    };
    let file = File::from(fd);
    if let Ok(meta) = file.metadata()
        && meta.is_file()
        && meta.len() > MAX_LOG_BYTES
    {
        let _ = file.set_len(0);
    }
}

/// The level that `RECAP_LOG` names, or `info`.
fn level() -> LevelFilter {
    std::env::var(LEVEL_VAR)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(LevelFilter::Info)
}

/// Install the logger. Call it once, at the start of `recap serve`.
pub fn init() {
    let level = level();
    if log::set_boxed_logger(Box::new(Logger { level })).is_ok() {
        log::set_max_level(level.max(LevelFilter::Warn));
    }
}

/// A duration for a log line: `850µs`, `12.3ms` or `4.21s`.
pub fn ms(d: std::time::Duration) -> String {
    let micros = d.as_micros();
    if micros < 1000 {
        format!("{micros}µs")
    } else if micros < 1_000_000 {
        format!("{:.1}ms", micros as f64 / 1000.0)
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}

/// A byte count for a log line: `512 B`, `81.2 KB`, `3.4 MB` or `2.39 GB`.
pub fn bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1 << 20 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else if n < 1 << 30 {
        format!("{:.1} MB", n as f64 / (1 << 20) as f64)
    } else {
        format!("{:.2} GB", n as f64 / (1 << 30) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn durations_and_sizes_use_a_short_unit() {
        assert_eq!(ms(Duration::from_micros(850)), "850µs");
        assert_eq!(ms(Duration::from_micros(12_345)), "12.3ms");
        assert_eq!(ms(Duration::from_millis(4_210)), "4.21s");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(83_149), "81.2 KB");
        assert_eq!(bytes(3_565_158), "3.4 MB");
        assert_eq!(bytes(2_567_593_165), "2.39 GB");
    }

    /// A log file past the limit becomes empty, and a small file stays.
    #[test]
    fn the_logger_empties_a_log_file_past_the_limit() {
        let path = std::env::temp_dir().join(format!("recap-log-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // launchd opens the log file in append mode too.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        file.set_len(1000).unwrap();
        limit_size(&file);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1000);

        file.set_len(MAX_LOG_BYTES + 1).unwrap();
        limit_size(&file);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        (&file).write_all(b"next\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "next\n");
        let _ = std::fs::remove_file(&path);
    }
}
