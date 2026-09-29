use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditSource {
    Cursor,
    ClaudeCode,
    Neovim,
}

#[derive(Debug, Clone)]
pub enum HistoryEntryKind {
    /// A file edit. `snapshot_path` points at the captured file content;
    /// `file_path` is the real on-disk path.
    FileChange,
    /// A user prompt sent to the LLM. `snapshot_path` is unused; `file_path`
    /// is a synthetic unique key (e.g. `claude:<session>:<uuid>`) that keeps
    /// `build_groups` from coalescing distinct prompts. `response` is the
    /// final text the agent wrote for this prompt, after its last tool use.
    /// `None` when the turn has no final text: an interrupted turn, a queued
    /// prompt, or a session that still runs.
    Prompt {
        text: String,
        response: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub timestamp: DateTime<Local>,
    /// The original file this entry belongs to (decoded path), or for
    /// prompts a synthetic unique key.
    pub file_path: PathBuf,
    /// The history snapshot file on disk. Empty for prompt entries.
    pub snapshot_path: PathBuf,
    /// The content this entry's diff must start from, when the source records
    /// it. Cursor agent edits carry the exact before-content of the tool call
    /// (see [`crate::cursor_edits`]), which is more accurate than the
    /// previous snapshot of the same file. `None` for every other source, and
    /// then [`HistoryStore::prev_snapshot`] falls back to that previous
    /// snapshot.
    pub base_path: Option<PathBuf>,
    /// Containing projects, sorted outermost → innermost.
    /// Each entry contains a project marker (.git, Cargo.toml, etc).
    pub project_roots: Vec<PathBuf>,
    pub source: EditSource,
    pub kind: HistoryEntryKind,
    /// Originating agent session id, when the source has one. Claude Code
    /// entries carry their `<session>.jsonl` id here so the UI can check
    /// whether that session is still running (see [`crate::session`]) and
    /// jump to / resume it. `None` for sources without a session concept
    /// (Cursor, Neovim).
    pub session_id: Option<String>,
    /// The working directory the originating agent session ran in, when
    /// known. Used as the `claude --resume` launch directory so the resumed
    /// session starts in the same project — more reliable than guessing from
    /// [`Self::project_root`], which is empty for prompt entries (their
    /// `file_path` is synthetic). `None` for sources that don't record a cwd.
    pub session_cwd: Option<PathBuf>,
}

impl HistoryEntry {
    pub fn file_name(&self) -> &str {
        self.file_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
    }

    pub fn is_prompt(&self) -> bool {
        matches!(self.kind, HistoryEntryKind::Prompt { .. })
    }

    pub fn prompt_text(&self) -> Option<&str> {
        match &self.kind {
            HistoryEntryKind::Prompt { text, .. } => Some(text),
            HistoryEntryKind::FileChange => None,
        }
    }

    /// The agent's final response to a prompt entry. `None` for a file
    /// change and for a prompt whose turn has no final text.
    pub fn response_text(&self) -> Option<&str> {
        match &self.kind {
            HistoryEntryKind::Prompt { response, .. } => response.as_deref(),
            HistoryEntryKind::FileChange => None,
        }
    }

    /// Innermost project root, if any.
    pub fn project_root(&self) -> Option<&Path> {
        self.project_roots.last().map(|p| p.as_path())
    }

    /// Render the path as "outer → inner → relative/path/in/inner".
    /// Falls back to last-N-components when no project context exists.
    /// `names` provides a lookup for worktree-aware display names.
    pub fn display_path(&self, names: &HashMap<PathBuf, String>) -> String {
        const ARROW: &str = " → ";
        let (chain, rel) = self.display_path_segments(names);
        // The tail may be multi-line (prompt text); flatten for one-line display.
        let rel = single_line(&rel);
        if chain.is_empty() {
            rel
        } else {
            format!("{}{}{}", chain.join(ARROW), ARROW, rel)
        }
    }

    /// Split `display_path` into (project chain names, tail). For file-change
    /// entries the tail is the relative path within the innermost project.
    /// For prompt entries the tail is the prompt text flattened to a single
    /// line. When there is no project context, the names list is empty and
    /// the tail is a short fallback (file path tail or full prompt).
    pub fn display_path_segments(
        &self,
        names: &HashMap<PathBuf, String>,
    ) -> (Vec<String>, String) {
        if let HistoryEntryKind::Prompt { text, .. } = &self.kind {
            let chain = self.chain_names(names);
            return (chain, single_line(text));
        }

        let Some(deepest) = self.project_root() else {
            return (Vec::new(), short_path(&self.file_path, 3));
        };

        let chain = self.chain_names(names);

        let rel = self
            .file_path
            .strip_prefix(deepest)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| self.file_path.to_string_lossy().into_owned());

        (chain, rel)
    }

    fn chain_names(&self, names: &HashMap<PathBuf, String>) -> Vec<String> {
        self.project_roots
            .iter()
            .map(|p| match names.get(p) {
                Some(s) => s.clone(),
                None => p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string(),
            })
            .collect()
    }
}

/// Collapse newlines and runs of whitespace into a single space, so a
/// multi-line prompt fits in one row of the History panel.
pub fn single_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_space && !out.is_empty() {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
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

/// What the loader knows about one directory. `Missing` means the directory
/// does not exist, so no directory below it exists either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirState {
    Missing,
    Plain,
    Root,
}

/// The directory states that [`find_project_chain`] found so far, for every
/// loader thread. A `stat` under an automount such as `/home` on macOS costs
/// 10 to 20 ms, and the loaders probe the same directories many thousand
/// times, so each directory gets one probe per load.
static DIR_STATES: Mutex<Option<HashMap<PathBuf, DirState>>> = Mutex::new(None);

/// Forget every directory state. Call this before a reload, so a project
/// marker that appeared after the last load gets a new probe.
pub fn clear_dir_states() {
    *DIR_STATES.lock().unwrap() = None;
}

fn dir_state(dir: &Path) -> DirState {
    let mut guard = DIR_STATES.lock().unwrap();
    let states = guard.get_or_insert_with(HashMap::new);
    if let Some(state) = states.get(dir) {
        return *state;
    }
    let state = if !dir.is_dir() {
        DirState::Missing
    } else if PROJECT_MARKERS.iter().any(|m| dir.join(m).exists()) {
        DirState::Root
    } else {
        DirState::Plain
    };
    states.insert(dir.to_path_buf(), state);
    state
}

/// Display name for a project root. Regular projects use the directory
/// basename. Git worktrees (`.git` file pointing at `.git/worktrees/<name>`)
/// render as `mainrepo (worktree)`. Submodules (`.git` file pointing at
/// `.git/modules/<...>`) render as `[basename]`. The synthetic
/// `~/.cursor/plans` fallback project surfaces as `cursor plans`.
pub fn project_display_name(path: &Path) -> String {
    if let Ok(home) = std::env::var("HOME")
        && path == Path::new(&home).join(".cursor/plans")
    {
        return "cursor plans".to_string();
    }
    let basename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    match git_dir_kind(path) {
        Some(GitDirKind::Worktree { main_name }) => format!("{} ({})", main_name, basename),
        Some(GitDirKind::Submodule) => format!("[{}]", basename),
        None => basename,
    }
}

enum GitDirKind {
    Worktree { main_name: String },
    Submodule,
}

/// Inspect `<project>/.git`. If it's a file with a `gitdir:` line, classify
/// the path as a worktree (containing `/.git/worktrees/`) or submodule
/// (containing `/.git/modules/`). Returns None for regular `.git` directories.
fn git_dir_kind(project: &Path) -> Option<GitDirKind> {
    let dot_git = project.join(".git");
    // read_to_string on a directory returns Err, so this safely no-ops for
    // regular repos where `.git` is a directory.
    let content = std::fs::read_to_string(&dot_git).ok()?;
    let line = content.lines().next()?.trim();
    let gitdir = line.strip_prefix("gitdir:")?.trim();

    if gitdir.contains("/.git/modules/") {
        return Some(GitDirKind::Submodule);
    }
    if let Some(idx) = gitdir.rfind("/.git/worktrees/") {
        let main_repo = Path::new(&gitdir[..idx]);
        if let Some(name) = main_repo.file_name().and_then(|s| s.to_str()) {
            return Some(GitDirKind::Worktree {
                main_name: name.to_string(),
            });
        }
    }
    None
}

/// Walk up from `path` collecting every directory that looks like a project
/// root. Returns them sorted outermost → innermost.
pub fn find_project_chain(path: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let Some(parent) = path.parent() else {
        return chain;
    };
    // Walk from the filesystem root down to the parent. The walk stops at the
    // first directory that does not exist, because a path below a missing
    // directory (a remote path, for example) cannot hold a project marker.
    let ancestors: Vec<&Path> = parent.ancestors().collect();
    for dir in ancestors.into_iter().rev() {
        match dir_state(dir) {
            DirState::Missing => break,
            DirState::Root => chain.push(dir.to_path_buf()),
            DirState::Plain => {}
        }
    }
    chain
}

pub struct HistoryStore {
    pub entries: Vec<HistoryEntry>,
    /// Projects sorted by most recent change (desc)
    pub projects: Vec<PathBuf>,
    /// Most recent timestamp per project, for display
    pub project_latest: HashMap<PathBuf, DateTime<Local>>,
    /// Outer→inner chain of project roots ending at the keyed project.
    pub project_chains: HashMap<PathBuf, Vec<PathBuf>>,
    /// Display name per project root (worktree-aware: `mainrepo (worktree)`).
    pub project_names: HashMap<PathBuf, String>,
    /// Number of history entries that touch each project (an entry whose chain
    /// is `[outer, inner]` increments the count for both).
    pub project_counts: HashMap<PathBuf, usize>,
    /// Agent session id → session title. A Cursor entry uses the composer
    /// name (see [`crate::cursor_sessions`]); a Claude Code entry uses the
    /// `aiTitle` of its session file (see [`crate::claude`]). The loader
    /// threads of both sources fill this one map.
    pub session_titles: HashMap<String, String>,
    /// For each entry, the index of the nearest *older* entry touching the same
    /// `file_path` — i.e. the snapshot a diff for that entry is taken against.
    /// Derived from `entries`; rebuilt by `recompute_projects` (see
    /// [`Self::prev_snapshot`]). Same length as `entries`.
    prev_by_index: Vec<Option<usize>>,
}

impl HistoryStore {
    pub fn from_entries(mut all_entries: Vec<HistoryEntry>) -> Self {
        all_entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        let mut store = Self {
            entries: all_entries,
            projects: Vec::new(),
            project_latest: HashMap::new(),
            project_chains: HashMap::new(),
            project_names: HashMap::new(),
            project_counts: HashMap::new(),
            session_titles: HashMap::new(),
            prev_by_index: Vec::new(),
        };
        store.recompute_projects();
        store
    }

    /// Snapshot the diff for `entry_idx` is taken against: the nearest older
    /// entry with the same `file_path`, or `None` when this is the file's first
    /// snapshot. Reads the precomputed `prev_by_index` rather than re-scanning
    /// `entries`, which turned every caller into an O(n) walk (and content
    /// search, which asks for every entry, into O(n²)).
    pub fn prev_snapshot(&self, entry_idx: usize) -> Option<&Path> {
        // A source that records its own before-content wins: it describes the
        // one edit, and the previous snapshot can include other edits.
        if let Some(base) = self.entries.get(entry_idx).and_then(|e| e.base_path.as_deref()) {
            return Some(base);
        }
        let prev = (*self.prev_by_index.get(entry_idx)?)?;
        Some(self.entries[prev].snapshot_path.as_path())
    }

    /// Recompute the project-level metadata from `self.entries`. Called by
    /// `from_entries` and after a reload appends new entries.
    pub fn recompute_projects(&mut self) {
        // Per-project latest timestamp, considering every project in the chain
        // (so an outer project also reflects edits inside nested projects).
        let mut project_latest: HashMap<PathBuf, DateTime<Local>> = HashMap::new();
        let mut project_chains: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        let mut project_counts: HashMap<PathBuf, usize> = HashMap::new();
        for e in &self.entries {
            for (i, root) in e.project_roots.iter().enumerate() {
                let slot = project_latest.entry(root.clone()).or_insert(e.timestamp);
                if e.timestamp > *slot {
                    *slot = e.timestamp;
                }
                project_chains
                    .entry(root.clone())
                    .or_insert_with(|| e.project_roots[..=i].to_vec());
                *project_counts.entry(root.clone()).or_insert(0) += 1;
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

        let project_names: HashMap<PathBuf, String> = project_latest
            .keys()
            .map(|p| (p.clone(), project_display_name(p)))
            .collect();

        self.projects = projects;
        self.project_latest = project_latest;
        self.project_chains = project_chains;
        self.project_names = project_names;
        self.project_counts = project_counts;

        // Both this and `prev_by_index` derive from `entries`, and every
        // mutation of `entries` (load batch merge, reload) runs through here —
        // so they're refreshed together and can't drift apart.
        self.recompute_predecessors();
    }

    /// Link each entry to the nearest older entry for the same file. `entries`
    /// is newest-first, so walking it backwards and remembering the last index
    /// seen per path leaves, at each step, exactly the smallest index greater
    /// than the current one — the nearest older snapshot.
    fn recompute_predecessors(&mut self) {
        let mut last_seen: HashMap<&Path, usize> = HashMap::new();
        let mut prev = vec![None; self.entries.len()];
        for (idx, entry) in self.entries.iter().enumerate().rev() {
            let path = entry.file_path.as_path();
            prev[idx] = last_seen.get(path).copied();
            last_seen.insert(path, idx);
        }
        self.prev_by_index = prev;
    }

    /// Scan the Cursor history dir, emitting one batch per folder via
    /// `on_batch`. Folders are processed newest-first (sorted by
    /// `entries.json` mtime descending) so the most recent edits stream to
    /// the UI before older history. `mtimes` is updated per folder so
    /// subsequent calls skip unchanged folders; `seen_keys` deduplicates
    /// across calls. `on_batch` returning `false` aborts the scan (used to
    /// shut down when the receiver has been dropped).
    /// Read the Cursor file history. `since` limits the scan: the loader
    /// skips a folder whose `entries.json` is older, because such a folder
    /// holds no newer entry. `None` reads every folder.
    pub fn load_cursor(
        history_dir: &Path,
        mtimes: &mut HashMap<PathBuf, SystemTime>,
        seen_keys: &mut HashSet<PathBuf>,
        since: Option<SystemTime>,
        on_batch: &mut dyn FnMut(Vec<HistoryEntry>) -> bool,
    ) -> Result<()> {
        let read_dir = fs::read_dir(history_dir)
            .with_context(|| format!("reading history dir: {}", history_dir.display()))?;

        // Collect (folder, entries.json mtime) for folders worth scanning,
        // then sort newest-first.
        let mut folders: Vec<(PathBuf, SystemTime)> = Vec::new();
        for dir_entry in read_dir.flatten() {
            let folder = dir_entry.path();
            if !folder.is_dir() {
                continue;
            }
            let entries_path = folder.join("entries.json");
            let Ok(meta) = fs::metadata(&entries_path) else {
                continue;
            };
            let Some(mtime) = meta.modified().ok() else {
                continue;
            };
            if let Some(prev) = mtimes.get(&folder).copied()
                && mtime <= prev
            {
                continue;
            }
            if since.is_some_and(|cut| mtime < cut) {
                continue;
            }
            folders.push((folder, mtime));
        }
        folders.sort_by(|a, b| b.1.cmp(&a.1));

        let mut chain_cache: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

        for (folder, entries_mtime) in folders {
            let entries_path = folder.join("entries.json");
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

            // Cursor writes a snapshot whenever a file is touched, even if the
            // content is unchanged (e.g. just opened). Walk oldest → newest and
            // drop snapshots whose content matches the previous one — they'd
            // render as empty diffs.
            let mut sorted: Vec<&RawEntry> = parsed.entries.iter().collect();
            sorted.sort_by_key(|r| r.timestamp);
            let mut batch: Vec<HistoryEntry> = Vec::new();
            let mut prev_hash: Option<[u8; 32]> = None;
            for raw in sorted {
                let snapshot_path = folder.join(&raw.id);
                let Ok(content) = fs::read(&snapshot_path) else {
                    continue;
                };
                let hash: [u8; 32] = Sha256::digest(&content).into();
                if Some(hash) == prev_hash {
                    continue;
                }
                prev_hash = Some(hash);

                // Walk every entry to keep prev_hash continuity, but only
                // emit ones not already returned in a prior load.
                if seen_keys.contains(&snapshot_path) {
                    continue;
                }

                let secs = raw.timestamp / 1000;
                let nanos = ((raw.timestamp % 1000) * 1_000_000) as u32;
                let timestamp = Local
                    .timestamp_opt(secs, nanos)
                    .single()
                    .unwrap_or_else(Local::now);

                seen_keys.insert(snapshot_path.clone());
                batch.push(HistoryEntry {
                    timestamp,
                    file_path: file_path.clone(),
                    snapshot_path,
                    base_path: None,
                    project_roots: project_roots.clone(),
                    source: EditSource::Cursor,
                    kind: HistoryEntryKind::FileChange,
                    // Attribution is applied as a post-pass once the (expensive)
                    // composer map is built off-thread; see `crate::app`.
                    session_id: None,
                    session_cwd: None,
                });
            }

            mtimes.insert(folder, entries_mtime);

            if !batch.is_empty() && !on_batch(batch) {
                return Ok(());
            }
        }

        Ok(())
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
            base_path: None,
            project_roots: roots.iter().map(PathBuf::from).collect(),
            source: EditSource::Cursor,
            kind: HistoryEntryKind::FileChange,
            session_id: None,
            session_cwd: None,
        }
    }

    fn prompt_entry(text: &str, roots: &[&str]) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local.timestamp_opt(0, 0).unwrap(),
            file_path: PathBuf::from(format!("prompt:{}", text)),
            snapshot_path: PathBuf::new(),
            base_path: None,
            project_roots: roots.iter().map(PathBuf::from).collect(),
            source: EditSource::ClaudeCode,
            kind: HistoryEntryKind::Prompt {
                text: text.to_string(),
                response: None,
            },
            session_id: None,
            session_cwd: None,
        }
    }

    #[test]
    fn display_path_for_prompt_uses_text() {
        let e = prompt_entry("Add a new feature\nplease", &["/dev/foo"]);
        assert_eq!(
            e.display_path(&HashMap::new()),
            "foo → Add a new feature please"
        );
    }

    #[test]
    fn display_path_for_prompt_without_project() {
        let e = prompt_entry("hello", &[]);
        assert_eq!(e.display_path(&HashMap::new()), "hello");
    }

    #[test]
    fn display_path_shows_project_chain() {
        let e = entry(
            "/dev/gaze/crates/jsx/src/jsx.rs",
            &["/dev/gaze", "/dev/gaze/crates/jsx"],
        );
        assert_eq!(
            e.display_path(&HashMap::new()),
            "gaze → jsx → src/jsx.rs"
        );
    }

    #[test]
    fn display_path_single_project() {
        let e = entry("/dev/foo/src/main.rs", &["/dev/foo"]);
        assert_eq!(e.display_path(&HashMap::new()), "foo → src/main.rs");
    }

    #[test]
    fn display_path_no_project_falls_back() {
        let e = entry("/tmp/loose/file.txt", &[]);
        assert_eq!(e.display_path(&HashMap::new()), "tmp/loose/file.txt");
    }

    #[test]
    fn align_durations_pads_unused_columns() {
        let parts = vec![
            DurationParts { days: 0, hours: 0, minutes: 5 },
            DurationParts { days: 0, hours: 3, minutes: 12 },
            DurationParts { days: 25, hours: 0, minutes: 7 },
        ];
        let out = super::align_durations(&parts);
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
        let out = super::align_durations(&parts);
        assert_eq!(out[0], "    5m");
        assert_eq!(out[1], "3h 12m");
    }

    #[test]
    fn align_durations_minutes_only_when_no_hours() {
        let parts = vec![
            DurationParts { days: 0, hours: 0, minutes: 5 },
            DurationParts { days: 0, hours: 0, minutes: 47 },
        ];
        let out = super::align_durations(&parts);
        assert_eq!(out[0], " 5m");
        assert_eq!(out[1], "47m");
    }

    #[test]
    fn display_path_uses_name_lookup() {
        let e = entry(
            "/cursor/worktrees/ws/hfgxb/src/main.rs",
            &["/cursor/worktrees/ws/hfgxb"],
        );
        let mut names = HashMap::new();
        names.insert(
            PathBuf::from("/cursor/worktrees/ws/hfgxb"),
            "mnode (hfgxb)".to_string(),
        );
        assert_eq!(e.display_path(&names), "mnode (hfgxb) → src/main.rs");
    }

    /// `prev_snapshot` replaced a linear `entries[idx+1..].find(same file)` at
    /// every caller. Assert it agrees with that scan for every entry, over a
    /// history that interleaves several files and a prompt (whose synthetic
    /// `file_path` is unique, so it must never link to anything).
    #[test]
    fn prev_snapshot_matches_linear_scan() {
        let files = ["/p/a.rs", "/p/b.rs", "/p/a.rs", "/p/a.rs", "/p/c.rs", "/p/b.rs"];
        let mut entries: Vec<HistoryEntry> = files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let mut e = entry(f, &["/p"]);
                // from_entries sorts newest-first; make the order deterministic
                // and give each entry a distinguishable snapshot.
                e.timestamp = Local.timestamp_opt(1_000 + i as i64, 0).unwrap();
                e.snapshot_path = PathBuf::from(format!("/snap/{i}"));
                e
            })
            .collect();
        let mut p = prompt_entry("a prompt", &["/p"]);
        p.timestamp = Local.timestamp_opt(1_003, 0).unwrap();
        p.snapshot_path = PathBuf::from("/snap/prompt");
        entries.push(p);

        let store = HistoryStore::from_entries(entries);
        for idx in 0..store.entries.len() {
            let expected = store.entries[idx + 1..]
                .iter()
                .find(|e| e.file_path == store.entries[idx].file_path)
                .map(|e| e.snapshot_path.as_path());
            assert_eq!(
                store.prev_snapshot(idx),
                expected,
                "entry {idx} ({:?})",
                store.entries[idx].file_path
            );
        }

        // Sanity: the fixture actually exercises both outcomes.
        assert!((0..store.entries.len()).any(|i| store.prev_snapshot(i).is_some()));
        assert!((0..store.entries.len()).any(|i| store.prev_snapshot(i).is_none()));
        let prompt_idx = store
            .entries
            .iter()
            .position(|e| matches!(e.kind, HistoryEntryKind::Prompt { .. }))
            .expect("prompt present");
        assert_eq!(store.prev_snapshot(prompt_idx), None);
    }
}

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
