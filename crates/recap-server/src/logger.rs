//! The logger of the server: one record on stderr for each `log` call.
//!
//! When stderr is a file, the logger writes each record as one line of JSON
//! (see [`LogRecord`]):
//!
//! ```text
//! {"time":"2026-09-28T21:14:12.753+02:00","level":"INFO","target":"http","message":"GET /v1/history","fields":{"status":200,"elapsed_ms":12.3,"bytes":83149}}
//! ```
//!
//! When stderr is a terminal, the logger writes the record as colored text
//! (see [`crate::colorize`]). `recap logs` writes the same text.
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

use chrono::{DateTime, Local};
use log::{Level, LevelFilter, Log, Metadata, Record};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{IsTerminal, Write};
use std::os::fd::AsFd;

/// The largest log file that the logger keeps. When stderr is a regular
/// file of this size, the logger empties it before the next line.
const MAX_LOG_BYTES: u64 = 10 << 20;

/// The environment variable that sets the level.
const LEVEL_VAR: &str = "RECAP_LOG";

struct Logger {
    level: LevelFilter,
    /// Stderr is a terminal: the logger writes colored text, not JSON.
    terminal: bool,
}

/// One record of the log. The logger writes it as one line of JSON.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    pub time: DateTime<Local>,
    pub level: String,
    /// The module that wrote the record, without the `recap_server::` prefix.
    pub target: String,
    pub message: String,
    /// The key-value fields of the record, for example the `status` of an
    /// HTTP request.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl LogRecord {
    fn new(time: DateTime<Local>, record: &Record) -> Self {
        let target = record.target();
        let mut fields = Fields(serde_json::Map::new());
        // The visitor does not fail.
        let _ = record.key_values().visit(&mut fields);
        LogRecord {
            time,
            level: record.level().to_string(),
            target: target.strip_prefix("recap_server::").unwrap_or(target).to_string(),
            message: record.args().to_string(),
            fields: fields.0,
        }
    }
}

/// Collects the key-value fields of a record as JSON values.
struct Fields(serde_json::Map<String, serde_json::Value>);

impl<'kvs> log::kv::VisitSource<'kvs> for Fields {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'kvs>,
        value: log::kv::Value<'kvs>,
    ) -> Result<(), log::kv::Error> {
        let json = if let Some(v) = value.to_u64() {
            v.into()
        } else if let Some(v) = value.to_i64() {
            v.into()
        } else if let Some(v) = value.to_f64().and_then(serde_json::Number::from_f64) {
            v.into()
        } else if let Some(v) = value.to_bool() {
            v.into()
        } else {
            value.to_string().into()
        };
        self.0.insert(key.to_string(), json);
        Ok(())
    }
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
        let record = LogRecord::new(Local::now(), record);
        let mut line = if self.terminal {
            crate::colorize::render(&record, true)
        } else {
            serde_json::to_string(&record).unwrap_or_default()
        };
        line.push('\n');
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
    let terminal = std::io::stderr().is_terminal();
    if log::set_boxed_logger(Box::new(Logger { level, terminal })).is_ok() {
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

    /// The record keeps the key-value fields of a `log` call with their types.
    #[test]
    fn a_record_keeps_the_fields() {
        let time = Local::now();
        let fields: &[(&str, log::kv::Value)] = &[
            ("status", 500u16.into()),
            ("elapsed_ms", 12.3f64.into()),
            ("name", "x y".into()),
        ];
        let record = LogRecord::new(
            time,
            &Record::builder()
                .level(Level::Warn)
                .target("recap_server::http")
                .args(format_args!("GET /v1/x"))
                .key_values(&fields)
                .build(),
        );
        let json = serde_json::to_string(&record).unwrap();
        assert!(
            json.ends_with(r#""level":"WARN","target":"http","message":"GET /v1/x","fields":{"elapsed_ms":12.3,"name":"x y","status":500}}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<LogRecord>(&json).unwrap(), record);
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
