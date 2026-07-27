use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// `build_groups` from coalescing distinct prompts.
    Prompt { text: String },
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub timestamp: DateTime<Local>,
    /// The original file this entry belongs to (decoded path), or for
    /// prompts a synthetic unique key.
    pub file_path: PathBuf,
    /// The history snapshot file on disk. Empty for prompt entries.
    pub snapshot_path: PathBuf,
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
            HistoryEntryKind::Prompt { text } => Some(text),
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
        if let HistoryEntryKind::Prompt { text } = &self.kind {
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
fn single_line(s: &str) -> String {
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

/// Extra indent applied to soft-wrapped continuation lines so they read as
/// visually distinct from lines produced by a real linebreak in the text.
pub const WRAP_INDENT: usize = 2;

/// One physical line produced by [`wrap_text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedLine {
    pub text: String,
    /// True when this line is a soft-wrap continuation of the previous one
    /// (the renderer indents it by [`WRAP_INDENT`]). False for the very first
    /// line and for the first line following a real linebreak in `text`.
    pub continuation: bool,
}

/// Word-wrap `text` into at most `max_lines` lines of `width` columns. Real
/// linebreaks (`\n`) in `text` are preserved: each starts a fresh, un-indented
/// line. Lines produced by *soft* wrapping (a line too long for `width`) are
/// flagged `continuation: true` and wrapped `WRAP_INDENT` columns narrower so
/// the renderer can indent them without overflowing. Whitespace within a
/// logical line is collapsed to single spaces; blank lines are dropped. Words
/// longer than a line are hard-split. When the text doesn't fit in `max_lines`,
/// the last line is truncated with a trailing `…`. Always returns between 1 and
/// `max_lines` lines.
pub fn wrap_text(text: &str, width: usize, max_lines: usize) -> Vec<WrappedLine> {
    let width = width.max(1);
    let max_lines = max_lines.max(1);
    let mut lines: Vec<WrappedLine> = Vec::new();

    // Stop generating once we have one more line than we'll keep — that's
    // enough to know the text overflowed, and it bounds work on huge prompts.
    'outer: for para in text.split('\n') {
        let para = single_line(para);
        if para.is_empty() {
            continue;
        }
        let mut cur = String::new();
        let mut cur_len = 0usize;
        // Whether the line currently being built is a soft-wrap continuation.
        let mut cont = false;
        // Available columns for the current line (continuations are narrower).
        let line_width = |cont: bool| {
            if cont {
                width.saturating_sub(WRAP_INDENT).max(1)
            } else {
                width
            }
        };

        for word in para.split(' ') {
            let mut rest = word;
            loop {
                let lw = line_width(cont);
                let wlen = rest.chars().count();
                if cur_len == 0 {
                    if wlen <= lw {
                        cur.push_str(rest);
                        cur_len = wlen;
                        break;
                    }
                    // Hard-split a word longer than the whole line.
                    let take_bytes = rest
                        .char_indices()
                        .nth(lw)
                        .map(|(i, _)| i)
                        .unwrap_or(rest.len());
                    lines.push(WrappedLine {
                        text: rest[..take_bytes].to_string(),
                        continuation: cont,
                    });
                    if lines.len() > max_lines {
                        break 'outer;
                    }
                    rest = &rest[take_bytes..];
                    cont = true;
                    continue;
                } else if cur_len + 1 + wlen <= lw {
                    cur.push(' ');
                    cur.push_str(rest);
                    cur_len += 1 + wlen;
                    break;
                } else {
                    lines.push(WrappedLine {
                        text: std::mem::take(&mut cur),
                        continuation: cont,
                    });
                    cur_len = 0;
                    cont = true;
                    if lines.len() > max_lines {
                        break 'outer;
                    }
                    // Reprocess this word on the fresh line.
                    continue;
                }
            }
        }
        // Flush the last line of this paragraph (para is non-empty, so cur has
        // content unless every word was hard-split into already-pushed lines).
        if cur_len > 0 {
            lines.push(WrappedLine {
                text: cur,
                continuation: cont,
            });
            if lines.len() > max_lines {
                break 'outer;
            }
        }
    }

    if lines.is_empty() {
        lines.push(WrappedLine {
            text: String::new(),
            continuation: false,
        });
    }

    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            let lw = if last.continuation {
                width.saturating_sub(WRAP_INDENT).max(1)
            } else {
                width
            };
            let mut chars: Vec<char> = last.text.chars().collect();
            while chars.len() + 1 > lw {
                chars.pop();
            }
            chars.push('…');
            last.text = chars.into_iter().collect();
        }
    }

    lines
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
    /// Outer→inner chain of project roots ending at the keyed project.
    pub project_chains: HashMap<PathBuf, Vec<PathBuf>>,
    /// Display name per project root (worktree-aware: `mainrepo (worktree)`).
    pub project_names: HashMap<PathBuf, String>,
    /// Number of history entries that touch each project (an entry whose chain
    /// is `[outer, inner]` increments the count for both).
    pub project_counts: HashMap<PathBuf, usize>,
    /// Cursor composerId → display name, for labelling attributed edits (see
    /// [`crate::cursor_sessions`]). Populated by the Cursor loader thread.
    pub cursor_session_names: HashMap<String, String>,
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
            cursor_session_names: HashMap::new(),
        };
        store.recompute_projects();
        store
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
    }

    /// Scan the Cursor history dir, emitting one batch per folder via
    /// `on_batch`. Folders are processed newest-first (sorted by
    /// `entries.json` mtime descending) so the most recent edits stream to
    /// the UI before older history. `mtimes` is updated per folder so
    /// subsequent calls skip unchanged folders; `seen_keys` deduplicates
    /// across calls. `on_batch` returning `false` aborts the scan (used to
    /// shut down when the receiver has been dropped).
    pub fn load_cursor(
        history_dir: &Path,
        mtimes: &mut HashMap<PathBuf, SystemTime>,
        seen_keys: &mut HashSet<PathBuf>,
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
            project_roots: roots.iter().map(PathBuf::from).collect(),
            source: EditSource::ClaudeCode,
            kind: HistoryEntryKind::Prompt {
                text: text.to_string(),
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

    fn texts(lines: &[WrappedLine]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn wrap_text_fits_single_line() {
        let out = wrap_text("hello world", 40, 5);
        assert_eq!(texts(&out), vec!["hello world"]);
        assert!(!out[0].continuation);
    }

    #[test]
    fn wrap_text_wraps_on_words() {
        // width 11 → "hello" (5) + " world" won't fit (12) → second line.
        let out = wrap_text("hello world foo", 11, 5);
        assert_eq!(texts(&out), vec!["hello world", "foo"]);
        // The wrapped second line is a soft-wrap continuation.
        assert!(!out[0].continuation);
        assert!(out[1].continuation);
    }

    #[test]
    fn wrap_text_caps_at_max_lines_with_ellipsis() {
        let out = wrap_text("aa bb cc dd", 2, 2);
        assert_eq!(out.len(), 2);
        assert!(out.last().unwrap().text.ends_with('…'));
    }

    #[test]
    fn wrap_text_hard_splits_long_word() {
        let out = wrap_text("abcdefgh", 3, 8);
        // First line uses full width (3); continuation lines are narrowed by
        // WRAP_INDENT, leaving width 1 for the remaining "defgh".
        assert_eq!(texts(&out), vec!["abc", "d", "e", "f", "g", "h"]);
        assert!(!out[0].continuation);
        assert!(out[1..].iter().all(|l| l.continuation));
    }

    #[test]
    fn wrap_text_empty_yields_one_line() {
        let out = wrap_text("", 10, 5);
        assert_eq!(texts(&out), vec![""]);
        assert!(!out[0].continuation);
    }

    #[test]
    fn wrap_text_preserves_real_linebreaks_uindented() {
        // Each real linebreak starts a fresh, non-continuation line.
        let out = wrap_text("first line\nsecond line", 40, 5);
        assert_eq!(texts(&out), vec!["first line", "second line"]);
        assert!(!out[0].continuation);
        assert!(!out[1].continuation, "line after \\n is not a soft-wrap");
    }

    #[test]
    fn wrap_text_drops_blank_lines() {
        let out = wrap_text("a\n\n\nb", 40, 5);
        assert_eq!(texts(&out), vec!["a", "b"]);
    }

    #[test]
    fn wrap_text_mixes_linebreaks_and_wrapping() {
        // First para wraps (width 11), second para is a real linebreak.
        let out = wrap_text("hello world foo\nbye", 11, 5);
        assert_eq!(texts(&out), vec!["hello world", "foo", "bye"]);
        assert!(!out[0].continuation);
        assert!(out[1].continuation, "soft-wrap of first para");
        assert!(!out[2].continuation, "real linebreak");
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
