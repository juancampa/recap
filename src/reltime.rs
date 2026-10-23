//! The relative time column of the TUI: "3h 12m" and similar, aligned so
//! that the units of all visible rows line up.

use chrono::{DateTime, Local};

#[derive(Debug, Clone, Copy)]
pub struct DurationParts {
    pub days: u64,
    pub hours: u64,
    pub minutes: u64,
}

pub fn duration_parts(dt: &DateTime<Local>) -> DurationParts {
    let total_mins = Local::now().signed_duration_since(*dt).num_minutes().max(0) as u64;
    DurationParts {
        days: total_mins / (60 * 24),
        hours: (total_mins / 60) % 24,
        minutes: total_mins % 60,
    }
}

fn digit_count(n: u64) -> usize {
    n.checked_ilog10().map(|x| x as usize + 1).unwrap_or(1)
}

/// Format a batch of relative durations with vertically-aligned d/h/m
/// columns. The minute column is always shown; the hour column appears when
/// any entry has hours (or days); the day column appears when any entry has
/// days. Returned strings all share the same width so callers can render
/// them flush left and have units line up across rows.
pub fn align_relative_times(timestamps: &[DateTime<Local>]) -> Vec<String> {
    let parts: Vec<DurationParts> = timestamps.iter().map(duration_parts).collect();
    align_durations(&parts)
}

fn align_durations(parts: &[DurationParts]) -> Vec<String> {
    let any_days = parts.iter().any(|p| p.days > 0);
    // Days imply hours so the column stays present even when an entry's
    // hours field is 0 (e.g. `3d  0h 12m`).
    let any_hours = any_days || parts.iter().any(|p| p.hours > 0);

    let d_width = parts
        .iter()
        .filter(|p| p.days > 0)
        .map(|p| digit_count(p.days) + 1)
        .max()
        .unwrap_or(0);
    let h_width = parts
        .iter()
        .filter(|p| p.days > 0 || p.hours > 0)
        .map(|p| digit_count(p.hours) + 1)
        .max()
        .unwrap_or(0);
    let m_width = parts
        .iter()
        .map(|p| digit_count(p.minutes) + 1)
        .max()
        .unwrap_or(2);

    parts
        .iter()
        .map(|p| {
            let mut out = String::new();
            if any_days {
                if p.days > 0 {
                    out.push_str(&format!("{:>w$}", format!("{}d", p.days), w = d_width));
                } else {
                    out.push_str(&" ".repeat(d_width));
                }
                out.push(' ');
            }
            if any_hours {
                if p.days > 0 || p.hours > 0 {
                    out.push_str(&format!("{:>w$}", format!("{}h", p.hours), w = h_width));
                } else {
                    out.push_str(&" ".repeat(h_width));
                }
                out.push(' ');
            }
            out.push_str(&format!("{:>w$}", format!("{}m", p.minutes), w = m_width));
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_durations_pads_unused_columns() {
        let parts = vec![
            DurationParts { days: 0, hours: 0, minutes: 5 },
            DurationParts { days: 0, hours: 3, minutes: 12 },
            DurationParts { days: 25, hours: 0, minutes: 7 },
        ];
        let out = align_durations(&parts);
        // All same width; days column appears for everyone since one entry has days.
        let widths: Vec<usize> = out.iter().map(|s| s.chars().count()).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]));
        assert_eq!(out[0], "        5m");
        assert_eq!(out[1], "    3h 12m");
        assert_eq!(out[2], "25d 0h  7m");
    }

    #[test]
    fn align_durations_skips_day_column_when_unused() {
        let parts = vec![
            DurationParts { days: 0, hours: 0, minutes: 5 },
            DurationParts { days: 0, hours: 3, minutes: 12 },
        ];
        let out = align_durations(&parts);
        assert_eq!(out[0], "    5m");
        assert_eq!(out[1], "3h 12m");
    }

    #[test]
    fn align_durations_minutes_only_when_no_hours() {
        let parts = vec![
            DurationParts { days: 0, hours: 0, minutes: 5 },
            DurationParts { days: 0, hours: 0, minutes: 47 },
        ];
        let out = align_durations(&parts);
        assert_eq!(out[0], " 5m");
        assert_eq!(out[1], "47m");
    }
}
