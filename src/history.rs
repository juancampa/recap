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

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub timestamp: DateTime<Local>,
    /// The original file this entry belongs to (decoded path)
    pub file_path: PathBuf,
    /// The history snapshot file on disk
    pub snapshot_path: PathBuf,
    /// Detected project root (contains .git or Cargo.toml)
    pub project_root: Option<PathBuf>,
}

impl HistoryEntry {
    pub fn short_path(&self, n: usize) -> String {
        let parts: Vec<_> = self.file_path.components().collect();
        let start = parts.len().saturating_sub(n);
        parts[start..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }

    pub fn file_name(&self) -> &str {
        self.file_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
    }
}

fn parse_resource_path(resource: &str) -> PathBuf {
    let without_scheme = resource.strip_prefix("file://").unwrap_or(resource);
    let decoded = percent_decode_str(without_scheme)
        .decode_utf8_lossy()
        .into_owned();
    PathBuf::from(decoded)
}

fn find_project_root(path: &Path) -> Option<PathBuf> {
    let mut dir = path.parent()?;
    loop {
        if dir.join(".git").exists() || dir.join("Cargo.toml").exists() {
            return Some(dir.to_path_buf());
        }
        match dir.parent() {
            Some(p) if p != dir => dir = p,
            _ => return None,
        }
    }
}

pub struct HistoryStore {
    pub entries: Vec<HistoryEntry>,
    /// Projects sorted by most recent change (desc)
    pub projects: Vec<PathBuf>,
    /// Most recent timestamp per project, for display
    pub project_latest: HashMap<PathBuf, DateTime<Local>>,
}

impl HistoryStore {
    pub fn load(history_dir: &Path) -> Result<Self> {
        let read_dir = fs::read_dir(history_dir)
            .with_context(|| format!("reading history dir: {}", history_dir.display()))?;

        let mut all_entries: Vec<HistoryEntry> = Vec::new();
        let mut root_cache: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();

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
            let project_root = root_cache
                .entry(file_path.clone())
                .or_insert_with(|| find_project_root(&file_path))
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
                    project_root: project_root.clone(),
                });
            }
        }

        all_entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        // Collect per-project latest timestamp
        let mut project_latest: HashMap<PathBuf, DateTime<Local>> = HashMap::new();
        for e in &all_entries {
            if let Some(root) = &e.project_root {
                let slot = project_latest.entry(root.clone()).or_insert(e.timestamp);
                if e.timestamp > *slot {
                    *slot = e.timestamp;
                }
            }
        }

        let mut projects: Vec<PathBuf> = project_latest.keys().cloned().collect();
        // Sort by most recent first
        projects.sort_by(|a, b| {
            let ta = project_latest.get(a).copied().unwrap_or(DateTime::UNIX_EPOCH.into());
            let tb = project_latest.get(b).copied().unwrap_or(DateTime::UNIX_EPOCH.into());
            tb.cmp(&ta)
        });

        Ok(Self {
            entries: all_entries,
            projects,
            project_latest,
        })
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
