//! Writes a [`LogRecord`] as one line of text, with or without ANSI colors.
//! The colors are the colors of `mlogs`:
//!
//! ```text
//! 2026-09-28 21:14:12.753 INFO  http:     GET /v1/history?project=/x 200 12.3ms 81.2 KB
//! ```
//!
//! The timestamp is light gray, the level has a color for each level, and the
//! target is cyan. The HTTP status has the color of its class, and a duration
//! has a color on a scale from green (5 ms) to purple (1 s).

use crate::logger::{LogRecord, bytes, ms};
use std::fmt::Write;
use std::time::Duration;

const RESET: &str = "\x1b[0m";
const LIGHT_GRAY: &str = "\x1b[38;5;252m";
const GRAY: &str = "\x1b[38;5;250m";
const CYAN: &str = "\x1b[36m";
const RED: &str = "\x1b[31m";
const ORANGE: &str = "\x1b[38;5;208m";
const GREEN: &str = "\x1b[32m";
const BLUE: &str = "\x1b[34m";
const PURPLE: &str = "\x1b[35m";

/// The width of the longest level name, `ERROR` and `DEBUG`.
const LEVEL_WIDTH: usize = 5;
/// The minimum width of the target with its `:`, for example `embedder:`.
const TARGET_MIN_WIDTH: usize = 9;

/// The fields that the renderer writes as a value without a key, in this
/// order. The unit or the position of the value tells what it is.
const BARE_FIELDS: [&str; 3] = ["status", "elapsed_ms", "bytes"];

/// Writes a line of the log file as text. A line that is not a [`LogRecord`],
/// for example a panic message, stays the same.
pub fn render_line(line: &str, color: bool) -> String {
    match serde_json::from_str::<LogRecord>(line) {
        Ok(record) => render(&record, color),
        Err(_) => line.to_string(),
    }
}

/// Writes the record as one line of text, without a newline.
pub fn render(record: &LogRecord, color: bool) -> String {
    let mut out = Painter { out: String::new(), color };
    out.paint(
        LIGHT_GRAY,
        &record.time.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
    );
    out.out.push(' ');
    out.paint(level_color(&record.level), &record.level);
    pad(&mut out.out, record.level.len(), LEVEL_WIDTH);
    out.out.push(' ');
    out.paint(CYAN, &record.target);
    out.paint(GRAY, ":");
    pad(
        &mut out.out,
        record.target.chars().count() + 1,
        TARGET_MIN_WIDTH,
    );
    out.out.push(' ');
    out.message(&record.message);

    for key in BARE_FIELDS {
        if let Some(value) = record.fields.get(key) {
            out.out.push(' ');
            out.bare_field(key, value);
        }
    }
    for (key, value) in &record.fields {
        if BARE_FIELDS.contains(&key.as_str()) {
            continue;
        }
        out.out.push(' ');
        out.paint(GRAY, key);
        out.paint(GRAY, "=");
        match value {
            serde_json::Value::String(text) => out.out.push_str(text),
            other => out.out.push_str(&other.to_string()),
        }
    }
    out.out
}

/// Text that gets ANSI colors only when `color` is true.
struct Painter {
    out: String,
    color: bool,
}

impl Painter {
    fn paint(&mut self, color: &str, text: &str) {
        if self.color && !color.is_empty() && !text.is_empty() {
            self.out.push_str(color);
            self.out.push_str(text);
            self.out.push_str(RESET);
        } else {
            self.out.push_str(text);
        }
    }

    /// Writes the message in the default color, and each duration in the
    /// message, such as `4.05s` in `load complete in 4.05s:`, in the color of
    /// the duration.
    fn message(&mut self, message: &str) {
        for (i, word) in message.split(' ').enumerate() {
            if i > 0 {
                self.out.push(' ');
            }
            let core = word.trim_start_matches('(');
            let core = core.trim_end_matches([',', ')', ':', ';']);
            match duration_millis(core) {
                Some(millis) => {
                    let start = word.len() - word.trim_start_matches('(').len();
                    let end = start + core.len();
                    self.out.push_str(&word[..start]);
                    self.paint(&duration_color(millis), core);
                    self.out.push_str(&word[end..]);
                }
                None => self.out.push_str(word),
            }
        }
    }

    fn bare_field(&mut self, key: &str, value: &serde_json::Value) {
        match (key, value.as_f64()) {
            ("status", Some(status)) => {
                let text = value.to_string();
                self.paint(status_color(status as u64), &text);
            }
            ("elapsed_ms", Some(millis)) => {
                let text = ms(Duration::from_secs_f64(millis.max(0.0) / 1000.0));
                self.paint(&duration_color(millis), &text);
            }
            ("bytes", Some(n)) => self.out.push_str(&bytes(n as u64)),
            _ => {
                self.paint(GRAY, key);
                self.paint(GRAY, "=");
                self.out.push_str(&value.to_string());
            }
        }
    }
}

/// Appends spaces to `out` until the text of `width` characters fills
/// `min_width` characters.
fn pad(out: &mut String, width: usize, min_width: usize) {
    for _ in width..min_width {
        out.push(' ');
    }
}

fn level_color(level: &str) -> &'static str {
    match level {
        "ERROR" => RED,
        "WARN" => ORANGE,
        "INFO" => GREEN,
        "DEBUG" => BLUE,
        "TRACE" => PURPLE,
        _ => "",
    }
}

/// The color of an HTTP status code, by its class.
fn status_color(status: u64) -> &'static str {
    match status / 100 {
        1 => GRAY,
        2 => GREEN,
        3 => CYAN,
        4 => ORANGE,
        5 => RED,
        _ => "",
    }
}

/// Parses a duration such as `9.97ms`, `350µs` or `1.2s` into milliseconds.
/// A number without a unit is not a duration.
fn duration_millis(value: &str) -> Option<f64> {
    let unit_start = value.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let number: f64 = value[..unit_start].parse().ok()?;
    let factor = match &value[unit_start..] {
        "ms" => 1.0,
        "s" => 1000.0,
        "µs" | "us" => 0.001,
        "ns" => 0.000_001,
        _ => return None,
    };
    Some(number * factor)
}

/// The colors of the duration scale: green at 5 ms, orange, red, and purple
/// at 1 s and more.
const DURATION_COLORS: [(f64, f64, f64); 4] = [
    (80.0, 200.0, 80.0),
    (255.0, 150.0, 0.0),
    (230.0, 50.0, 50.0),
    (190.0, 90.0, 240.0),
];
const FAST_MILLIS: f64 = 5.0;
const SLOW_MILLIS: f64 = 1000.0;

/// A 24-bit color for a duration. The color changes on a logarithmic scale,
/// so that each step of the scale multiplies the duration by the same factor.
fn duration_color(millis: f64) -> String {
    let position = ((millis.max(FAST_MILLIS) / FAST_MILLIS).ln()
        / (SLOW_MILLIS / FAST_MILLIS).ln())
    .min(1.0);
    let scaled = position * (DURATION_COLORS.len() - 1) as f64;
    let index = (scaled as usize).min(DURATION_COLORS.len() - 2);
    let fraction = scaled - index as f64;
    let (from, to) = (DURATION_COLORS[index], DURATION_COLORS[index + 1]);
    let mix = |a: f64, b: f64| (a + (b - a) * fraction).round() as u8;
    let mut color = String::with_capacity(20);
    write!(
        color,
        "\x1b[38;2;{};{};{}m",
        mix(from.0, to.0),
        mix(from.1, to.1),
        mix(from.2, to.2)
    )
    .unwrap();
    color
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn record(level: &str, target: &str, message: &str, fields: serde_json::Value) -> LogRecord {
        LogRecord {
            time: Local.with_ymd_and_hms(2026, 10, 16, 23, 52, 53).unwrap()
                + chrono::Duration::milliseconds(474),
            level: level.to_string(),
            target: target.to_string(),
            message: message.to_string(),
            fields: fields.as_object().cloned().unwrap_or_default(),
        }
    }

    #[test]
    fn writes_a_request_like_the_text_format() {
        let r = record(
            "INFO",
            "http",
            "GET /v1/agents",
            serde_json::json!({"bytes": 1782579, "elapsed_ms": 34.0, "status": 200}),
        );
        assert_eq!(
            render(&r, false),
            "2026-10-16 23:52:53.474 INFO  http:     GET /v1/agents 200 34.0ms 1.7 MB"
        );
        let colored = render(&r, true);
        assert!(colored.contains(&format!("{GREEN}200{RESET}")), "{colored:?}");
        assert!(colored.contains(&format!("{CYAN}http{RESET}")), "{colored:?}");
    }

    #[test]
    fn writes_the_other_fields_with_their_keys() {
        let r = record("WARN", "embedder", "slow", serde_json::json!({"n": 3, "name": "a b"}));
        assert_eq!(
            render(&r, false),
            "2026-10-16 23:52:53.474 WARN  embedder: slow n=3 name=a b"
        );
    }

    #[test]
    fn colors_the_durations_in_the_message() {
        let r = record("INFO", "server", "load complete in 1.5s: (Claude 2ms, x)", serde_json::json!({}));
        let colored = render(&r, true);
        assert!(
            colored.ends_with(&format!(
                "load complete in {}1.5s{RESET}: (Claude {}2ms{RESET}, x)",
                duration_color(1500.0),
                duration_color(2.0)
            )),
            "{colored:?}"
        );
    }

    #[test]
    fn a_line_that_is_not_a_record_stays_the_same() {
        let line = "thread 'main' panicked at src/main.rs:1:1:";
        assert_eq!(render_line(line, true), line);
        let r = record("INFO", "http", "GET /", serde_json::json!({}));
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(render_line(&json, false), render(&r, false));
    }

    #[test]
    fn the_duration_color_goes_from_green_to_purple() {
        assert_eq!(duration_color(1.0), "\x1b[38;2;80;200;80m");
        assert_eq!(duration_color(1000.0), "\x1b[38;2;190;90;240m");
        assert_eq!(duration_color(60_000.0), "\x1b[38;2;190;90;240m");
        assert_eq!(duration_millis("9.97ms"), Some(9.97));
        assert_eq!(duration_millis("1.5s"), Some(1500.0));
        assert_eq!(duration_millis("12"), None);
        assert_eq!(duration_millis("fast"), None);
    }
}
