use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct EntriesJson {
    resource: String,
    entries: Vec<RawEntry>,
}

#[derive(Debug, Deserialize)]
struct RawEntry {
    id: String,
    timestamp: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditSource {
    Cursor,
    ClaudeCode,
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub timestamp: DateTime<Local>,
    /// The original file this entry belongs to (decoded path)
    pub file_path: PathBuf,
    /// The history snapshot file on disk
    pub snapshot_path: PathBuf,
    /// Containing projects, sorted outermost → innermost.
    /// Each entry contains a project marker (.git, Cargo.toml, etc).
    pub project_roots: Vec<PathBuf>,
    pub source: EditSource,
}

impl HistoryEntry {
    pub fn file_name(&self) -> &str {
        self.file_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
    }

    /// Innermost project root, if any.
    pub fn project_root(&self) -> Option<&Path> {
        self.project_roots.last().map(|p| p.as_path())
    }

    /// Render the path as "outer → inner → relative/path/in/inner".
    /// Falls back to last-N-components when no project context exists.
    pub fn display_path(&self) -> String {
        const ARROW: &str = " → ";

        let Some(deepest) = self.project_root() else {
            return short_path(&self.file_path, 3);
        };

        let names: Vec<String> = self
            .project_roots
            .iter()
            .map(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string()
            })
            .collect();

        let rel = self
            .file_path
            .strip_prefix(deepest)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| self.file_path.to_string_lossy().into_owned());

        format!("{}{}{}", names.join(ARROW), ARROW, rel)
    }
}

fn short_path(path: &Path, n: usize) -> String {
    let parts: Vec<_> = path.components().collect();
    let start = parts.len().saturating_sub(n);
    parts[start..]
        .iter()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn parse_resource_path(resource: &str) -> PathBuf {
    let without_scheme = resource.strip_prefix("file://").unwrap_or(resource);
    let decoded = percent_decode_str(without_scheme)
        .decode_utf8_lossy()
        .into_owned();
    PathBuf::from(decoded)
}

const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
];

fn is_project_root(dir: &Path) -> bool {
    PROJECT_MARKERS.iter().any(|m| dir.join(m).exists())
}

/// Walk up from `path` collecting every directory that looks like a project
/// root. Returns them sorted outermost → innermost.
pub fn find_project_chain(path: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let Some(mut dir) = path.parent() else {
        return chain;
    };
    loop {
        if is_project_root(dir) {
            chain.push(dir.to_path_buf());
        }
        match dir.parent() {
            Some(p) if p != dir => dir = p,
            _ => break,
        }
    }
    chain.reverse();
    chain
}

pub struct HistoryStore {
    pub entries: Vec<HistoryEntry>,
    /// Projects sorted by most recent change (desc)
    pub projects: Vec<PathBuf>,
    /// Most recent timestamp per project, for display
    pub project_latest: HashMap<PathBuf, DateTime<Local>>,
}

impl HistoryStore {
    pub fn from_entries(mut all_entries: Vec<HistoryEntry>) -> Self {
        all_entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        // Per-project latest timestamp, considering every project in the chain
        // (so an outer project also reflects edits inside nested projects).
        let mut project_latest: HashMap<PathBuf, DateTime<Local>> = HashMap::new();
        for e in &all_entries {
            for root in &e.project_roots {
                let slot = project_latest.entry(root.clone()).or_insert(e.timestamp);
                if e.timestamp > *slot {
                    *slot = e.timestamp;
                }
            }
        }

        let mut projects: Vec<PathBuf> = project_latest.keys().cloned().collect();
        projects.sort_by(|a, b| {
            let ta = project_latest
                .get(a)
                .copied()
                .unwrap_or(DateTime::UNIX_EPOCH.into());
            let tb = project_latest
                .get(b)
                .copied()
                .unwrap_or(DateTime::UNIX_EPOCH.into());
            tb.cmp(&ta)
        });

        Self {
            entries: all_entries,
            projects,
            project_latest,
        }
    }

    pub fn load_cursor(history_dir: &Path) -> Result<Vec<HistoryEntry>> {
        let read_dir = fs::read_dir(history_dir)
            .with_context(|| format!("reading history dir: {}", history_dir.display()))?;

        let mut all_entries: Vec<HistoryEntry> = Vec::new();
        let mut chain_cache: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

        for dir_entry in read_dir {
            let dir_entry = dir_entry?;
            let folder = dir_entry.path();
            if !folder.is_dir() {
                continue;
            }
            let entries_path = folder.join("entries.json");
            if !entries_path.exists() {
                continue;
            }

            let data = match fs::read_to_string(&entries_path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let parsed: EntriesJson = match serde_json::from_str(&data) {
                Ok(p) => p,
                Err(_) => continue,
            };

            let file_path = parse_resource_path(&parsed.resource);
            let project_roots = chain_cache
                .entry(file_path.clone())
                .or_insert_with(|| find_project_chain(&file_path))
                .clone();

            for raw in &parsed.entries {
                let snapshot_path = folder.join(&raw.id);
                let secs = raw.timestamp / 1000;
                let nanos = ((raw.timestamp % 1000) * 1_000_000) as u32;
                let timestamp = Local
                    .timestamp_opt(secs, nanos)
                    .single()
                    .unwrap_or_else(Local::now);

                all_entries.push(HistoryEntry {
                    timestamp,
                    file_path: file_path.clone(),
                    snapshot_path,
                    project_roots: project_roots.clone(),
                    source: EditSource::Cursor,
                });
            }
        }

        Ok(all_entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn entry(file: &str, roots: &[&str]) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local.timestamp_opt(0, 0).unwrap(),
            file_path: PathBuf::from(file),
            snapshot_path: PathBuf::new(),
            project_roots: roots.iter().map(PathBuf::from).collect(),
            source: EditSource::Cursor,
        }
    }

    #[test]
    fn display_path_shows_project_chain() {
        let e = entry(
            "/dev/gaze/crates/jsx/src/jsx.rs",
            &["/dev/gaze", "/dev/gaze/crates/jsx"],
        );
        assert_eq!(e.display_path(), "gaze → jsx → src/jsx.rs");
    }

    #[test]
    fn display_path_single_project() {
        let e = entry("/dev/foo/src/main.rs", &["/dev/foo"]);
        assert_eq!(e.display_path(), "foo → src/main.rs");
    }

    #[test]
    fn display_path_no_project_falls_back() {
        let e = entry("/tmp/loose/file.txt", &[]);
        assert_eq!(e.display_path(), "tmp/loose/file.txt");
    }
}

/// Compact relative time: "3h 20m", "45m", "2d", etc. — no trailing "ago"
pub fn relative_time_compact(dt: &DateTime<Local>) -> String {
    let now = Local::now();
    let diff = now.signed_duration_since(*dt);
    let secs = diff.num_seconds().max(0);
    if secs < 60 {
        return format!("{}s", secs);
    }
    let mins = diff.num_minutes();
    if mins < 60 {
        return format!("{}m", mins);
    }
    let hours = diff.num_hours();
    let rem_mins = mins - hours * 60;
    if hours < 24 {
        if rem_mins > 0 {
            return format!("{}h {}m", hours, rem_mins);
        }
        return format!("{}h", hours);
    }
    let days = diff.num_days();
    let rem_hours = hours - days * 24;
    if days < 30 {
        if rem_hours > 0 {
            return format!("{}d {}h", days, rem_hours);
        }
        return format!("{}d", days);
    }
    let months = days / 30;
    if months < 12 {
        return format!("{}mo", months);
    }
    format!("{}y", months / 12)
}
