//! Cursor **agent session** ("composer" — one chat/agent conversation)
//! integration. Two jobs, both from `globalStorage/state.vscdb`:
//!
//! - **Prompts.** Each composer's user messages (type-1 bubbles) become
//!   [`HistoryEntryKind::Prompt`] entries keyed to the `composerId`, exactly
//!   like Claude Code prompts — so a Cursor session shows its prompts and
//!   edits together under one colored session bar.
//! - **Edit attribution.** File edits themselves come from VS Code Local
//!   History (full content + real save times, for every edit tool); this
//!   module tags each with the composer that produced it (see below), so
//!   agent edits share their session's bar and manual saves stay unattributed.
//!
//! Original attribution notes:
//!
//! The edits we surface come from VS Code Local History, which logs every
//! file save with no author. Cursor records its agent conversations
//! separately in `globalStorage/state.vscdb`, keyed by a `composerId` UUID.
//! This module joins the two so a history snapshot `(path, timestamp)` can be
//! tagged with the composer that most likely wrote it, using three layers
//! from most to least precise:
//!
//! 1. **`patch-graph`** — Cursor's newest store records each applied edit as
//!    `(fileUri.path, timestamp, patchSource, composerId)`. When a patch for
//!    the same path lands within [`PATCH_TOL_MS`] of the snapshot and was made
//!    by the agent, that composer is an exact match. Only recent edits have
//!    these records (no historical backfill).
//! 2. **composer window + file set** — every composer has a
//!    `[createdAt, lastUpdatedAt]` window and a set of files it edited (from
//!    its message "bubbles"). A snapshot whose path is in the set and whose
//!    time falls inside the window (± [`WINDOW_GRACE_MS`]) is attributed to
//!    that composer. This is the fuzzy fallback that covers old history; it's
//!    ambiguous only when two composers touched the same file at overlapping
//!    times.
//!
//! There is no stored resulting-file content, so an exact content-hash match
//! isn't possible — hence the timestamp/window join.

use crate::history::{find_project_chain, EditSource, HistoryEntry, HistoryEntryKind};
use chrono::{DateTime, Local};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A `patch-graph` timestamp and the snapshot time may differ by the save
/// latency; accept a patch within this many milliseconds of the snapshot.
const PATCH_TOL_MS: i64 = 10_000;

/// Grace added around a composer's `[createdAt, lastUpdatedAt]` window when
/// testing whether a snapshot falls inside it (a save can lag the last edit).
const WINDOW_GRACE_MS: i64 = 120_000;

#[derive(Debug, Clone)]
struct Patch {
    ts_ms: i64,
    composer: String,
}

#[derive(Debug, Clone)]
struct Composer {
    id: String,
    created: i64,
    updated: i64,
    files: HashSet<PathBuf>,
}

/// Per-composer metadata gathered before the file sets are attached.
#[derive(Debug, Clone, Default)]
struct ComposerMeta {
    created: Option<i64>,
    updated: Option<i64>,
    name: Option<String>,
    /// Git-repo roots the composer worked in; base for resolving the
    /// `relativeWorkspacePath` of agentic diffs.
    repo_paths: Vec<PathBuf>,
}

/// Built map from Cursor's global store. Cheap to query; built once per load.
#[derive(Debug, Default)]
pub struct CursorSessions {
    /// Exact per-edit records, indexed by absolute file path.
    patches: HashMap<PathBuf, Vec<Patch>>,
    /// Composer windows + file sets for the fuzzy fallback.
    composers: Vec<Composer>,
    /// composerId → human-readable display name.
    names: HashMap<String, String>,
    /// Cursor agent prompt entries (one per user message), keyed to their
    /// composer via `session_id`. Emitted into the history like Claude prompts.
    prompts: Vec<HistoryEntry>,
}

impl CursorSessions {
    /// Build from Cursor's `globalStorage/state.vscdb`. Best-effort: any read
    /// error yields an empty map (attribution simply degrades to `None`), so
    /// this never blocks the loader.
    ///
    /// NOTE: this scans a multi-gigabyte SQLite store (~10s), so it must run
    /// off the UI thread and be cached — see [`Self::load_cached`].
    pub fn load() -> Self {
        match global_state_db() {
            Some(db) => Self::from_db(&db).unwrap_or_default(),
            None => Self::default(),
        }
    }

    /// The modification time of the source DB, used to decide whether a cached
    /// map is still current. `None` when Cursor isn't installed.
    pub fn source_mtime() -> Option<std::time::SystemTime> {
        std::fs::metadata(global_state_db()?).and_then(|m| m.modified()).ok()
    }

    /// Return the cached map when `prev_mtime` matches the DB's current mtime;
    /// otherwise rebuild. The returned mtime should be stored alongside the map
    /// so the next call can skip the rebuild. Rebuilding is the expensive path.
    pub fn load_cached(
        prev: Option<std::sync::Arc<Self>>,
        prev_mtime: Option<std::time::SystemTime>,
    ) -> (std::sync::Arc<Self>, Option<std::time::SystemTime>) {
        let cur = Self::source_mtime();
        if let (Some(p), true) = (&prev, cur.is_some() && cur == prev_mtime) {
            return (p.clone(), cur);
        }
        (std::sync::Arc::new(Self::load()), cur)
    }

    fn from_db(db: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

        let mut metas: HashMap<String, ComposerMeta> = HashMap::new();
        read_composer_headers(&conn, &mut metas);
        read_composer_data(&conn, &mut metas);

        let (files, mut prompts) = read_bubbles(&conn, &metas);

        // A composer's `trackedGitRepos` are often empty, leaving its prompts
        // with no project chain. The files the agent actually edited are a
        // better signal of where the work happened, so backfill any empty
        // chain from the composer's edited file set (deterministic pick).
        let mut chain_cache: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for p in &mut prompts {
            if !p.project_roots.is_empty() {
                continue;
            }
            let Some(composer) = p.session_id.as_deref() else {
                continue;
            };
            if let Some(file) = files.get(composer).and_then(|set| set.iter().min()) {
                p.project_roots = chain_cache
                    .entry(file.clone())
                    .or_insert_with(|| find_project_chain(file))
                    .clone();
            }
        }

        let patches = read_patch_graph(&conn);

        let mut composers = Vec::new();
        let mut names = HashMap::new();
        for (id, meta) in metas {
            if let Some(name) = meta.name {
                names.insert(id.clone(), name);
            }
            if let (Some(created), Some(updated)) = (meta.created, meta.updated) {
                composers.push(Composer {
                    files: files.get(&id).cloned().unwrap_or_default(),
                    id,
                    created,
                    updated,
                });
            }
        }

        Ok(Self {
            patches,
            composers,
            names,
            prompts,
        })
    }

    /// The Cursor agent prompt entries (user messages), ready to merge into the
    /// history. Each carries `session_id = composerId`.
    pub fn prompts(&self) -> &[HistoryEntry] {
        &self.prompts
    }

    /// composerId for the edit at `path` made around `ts_ms` (unix millis),
    /// or `None` if no composer can be attributed. Prefers an exact
    /// `patch-graph` hit, then the composer whose window+file-set brackets the
    /// edit (the narrowest window wins ties).
    pub fn attribute(&self, path: &Path, ts_ms: i64) -> Option<String> {
        if let Some(list) = self.patches.get(path) {
            let best = list
                .iter()
                .filter(|p| (p.ts_ms - ts_ms).abs() <= PATCH_TOL_MS)
                .min_by_key(|p| (p.ts_ms - ts_ms).abs());
            if let Some(p) = best {
                return Some(p.composer.clone());
            }
        }

        self.composers
            .iter()
            .filter(|c| {
                c.files.contains(path)
                    && ts_ms >= c.created - WINDOW_GRACE_MS
                    && ts_ms <= c.updated + WINDOW_GRACE_MS
            })
            // Narrowest window is the most specific attribution.
            .min_by_key(|c| c.updated - c.created)
            .map(|c| c.id.clone())
    }

    /// The composerId → display-name map, for handing to the store so the UI
    /// can label attributed edits.
    pub fn names_map(&self) -> &HashMap<String, String> {
        &self.names
    }
}

/// composerId → name/window from the `composerHeaders` table (newer schema).
fn read_composer_headers(conn: &Connection, metas: &mut HashMap<String, ComposerMeta>) {
    let Ok(mut stmt) =
        conn.prepare("SELECT composerId, createdAt, lastUpdatedAt, value FROM composerHeaders")
    else {
        return;
    };
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    });
    let Ok(rows) = rows else { return };
    for (id, created, updated, value) in rows.flatten() {
        let meta = metas.entry(id).or_default();
        meta.created = created;
        meta.updated = updated;
        if let Some(v) = value.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()) {
            if let Some(name) = nonempty_str(&v, "name") {
                meta.name = Some(name);
            } else if meta.name.is_none() {
                if let Some(sub) = nonempty_str(&v, "subtitle") {
                    meta.name = Some(sub);
                }
            }
            if let Some(repos) = v.get("trackedGitRepos").and_then(|r| r.as_array()) {
                for repo in repos {
                    if let Some(p) = repo.get("repoPath").and_then(|p| p.as_str()) {
                        meta.repo_paths.push(PathBuf::from(p));
                    }
                }
            }
        }
    }
}

/// composerId → name/window from `cursorDiskKV` `composerData:<id>` blobs
/// (older composers not present in `composerHeaders`). Only fills fields left
/// unset by the header pass, so newer metadata wins.
fn read_composer_data(conn: &Connection, metas: &mut HashMap<String, ComposerMeta>) {
    let Ok(mut stmt) =
        conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key LIKE 'composerData:%'")
    else {
        return;
    };
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    });
    let Ok(rows) = rows else { return };
    for (key, value) in rows.flatten() {
        let Some(id) = key.strip_prefix("composerData:") else {
            continue;
        };
        let Some(v) = value.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()) else {
            continue;
        };
        let meta = metas.entry(id.to_string()).or_default();
        if meta.created.is_none() {
            meta.created = v.get("createdAt").and_then(|x| x.as_i64());
        }
        if meta.updated.is_none() {
            meta.updated = v.get("lastUpdatedAt").and_then(|x| x.as_i64());
        }
        if meta.name.is_none() {
            meta.name = nonempty_str(&v, "name");
        }
    }
}

/// Scan message bubbles once, extracting two things per composer:
///
/// - the **set of files edited** (for the fuzzy window attribution), and
/// - the **user prompts** (type-1 bubbles), as [`HistoryEntryKind::Prompt`]
///   entries keyed to the composer — the Cursor analogue of Claude prompts.
///
/// A Cursor agent edit is recorded three different ways depending on the client
/// version, and the `LIKE` filter must match all of them (plus type-1 prompt
/// bubbles) without triggering a full-table JSON scan:
/// - modern agent edits: a `toolFormerData` **tool call** (`edit_file`,
///   `edit_file_v2`, `search_replace`) whose `params.relativeWorkspacePath`
///   (usually absolute) names the file;
/// - agentic bubbles: `diffsSinceLastApply[].relativeWorkspacePath` (relative
///   to a repo root);
/// - legacy/ask bubbles: absolute `codeBlocks[].uri.path`;
/// - user prompts: `"type":1` bubbles with `text` and a top-level `createdAt`.
fn read_bubbles(
    conn: &Connection,
    metas: &HashMap<String, ComposerMeta>,
) -> (HashMap<String, HashSet<PathBuf>>, Vec<HistoryEntry>) {
    let mut files: HashMap<String, HashSet<PathBuf>> = HashMap::new();
    let mut prompts: Vec<HistoryEntry> = Vec::new();
    // Ancestor-chain cache keyed by the probe path, shared across prompts.
    let mut chain_cache: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT key, value FROM cursorDiskKV WHERE key LIKE 'bubbleId:%' \
         AND (value LIKE '%\"codeBlocks\":[{%' OR value LIKE '%\"diffsSinceLastApply\":[{%' \
              OR value LIKE '%search_replace%' OR value LIKE '%edit_file%' \
              OR value LIKE '%\"type\":1,%')",
    ) else {
        return (files, prompts);
    };
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    });
    let Ok(rows) = rows else { return (files, prompts) };
    for (key, value) in rows.flatten() {
        // key = "bubbleId:<composerId>:<bubbleId>"
        let mut parts = key.splitn(3, ':');
        let (_, Some(composer), Some(bubble)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Some(v) = value.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()) else {
            continue;
        };

        // User prompt (type-1) bubble → a Prompt history entry.
        if v.get("type").and_then(|t| t.as_i64()) == Some(1) {
            if let Some(entry) =
                prompt_entry(&v, composer, bubble, metas.get(composer), &mut chain_cache)
            {
                prompts.push(entry);
            }
        }

        let repos = metas.get(composer).map(|m| m.repo_paths.as_slice()).unwrap_or(&[]);
        let set = files.entry(composer.to_string()).or_default();

        // Modern agent edits: an `edit_file*` / `search_replace` tool call.
        if let Some(tfd) = v.get("toolFormerData") {
            let name = tfd
                .get("name")
                .or_else(|| tfd.get("tool"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            if is_edit_tool(name) {
                if let Some(p) = tool_params(tfd)
                    .as_ref()
                    .and_then(|p| p.get("relativeWorkspacePath"))
                    .and_then(|p| p.as_str())
                {
                    insert_path(set, p, repos);
                }
            }
        }

        // Legacy/ask bubbles: absolute `codeBlocks[].uri.path`.
        if let Some(blocks) = v.get("codeBlocks").and_then(|b| b.as_array()) {
            for block in blocks {
                if let Some(p) = block
                    .get("uri")
                    .and_then(|u| u.get("path"))
                    .and_then(|p| p.as_str())
                {
                    set.insert(PathBuf::from(p));
                }
            }
        }

        // Agentic bubbles: `diffsSinceLastApply[].relativeWorkspacePath`.
        if let Some(diffs) = v.get("diffsSinceLastApply").and_then(|d| d.as_array()) {
            for diff in diffs {
                if let Some(rel) = diff.get("relativeWorkspacePath").and_then(|p| p.as_str()) {
                    insert_path(set, rel, repos);
                }
            }
        }
    }
    (files, prompts)
}

/// Build a [`HistoryEntryKind::Prompt`] entry from a type-1 (user) bubble, or
/// `None` when the text is empty or there is no usable timestamp. The bubble
/// carries no `session_cwd`, but its `createdAt` (RFC3339) and the composer's
/// repo roots give the timestamp and project chain. The synthetic `file_path`
/// (`cursor:<composerId>:<bubbleId>`) mirrors Claude's, keeping distinct
/// prompts from coalescing in `build_groups`.
fn prompt_entry(
    v: &Value,
    composer: &str,
    bubble: &str,
    meta: Option<&ComposerMeta>,
    chain_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
) -> Option<HistoryEntry> {
    let text = nonempty_str(v, "text")?;
    let ts_str = v.get("createdAt").and_then(|t| t.as_str())?;
    let timestamp = DateTime::parse_from_rfc3339(ts_str).ok()?.with_timezone(&Local);

    // Project chain from the composer's first git repo root. `find_project_chain`
    // walks a *file*'s ancestors, so probe a pseudo-child of the root (as the
    // Claude prompt path does) so the root itself can appear in the chain.
    let repo = meta.and_then(|m| m.repo_paths.first());
    let project_roots = match repo {
        Some(root) => {
            let probe = root.join(".");
            chain_cache
                .entry(probe.clone())
                .or_insert_with(|| find_project_chain(&probe))
                .clone()
        }
        None => Vec::new(),
    };

    Some(HistoryEntry {
        timestamp,
        file_path: PathBuf::from(format!("cursor:{composer}:{bubble}")),
        snapshot_path: PathBuf::new(),
        project_roots,
        source: EditSource::Cursor,
        kind: HistoryEntryKind::Prompt { text },
        session_id: Some(composer.to_string()),
        session_cwd: repo.cloned(),
    })
}

/// Whether a `toolFormerData` tool name is one that edits a file.
fn is_edit_tool(name: &str) -> bool {
    name == "search_replace" || name.starts_with("edit_file") || name.starts_with("write_file")
}

/// A tool call's `params`, which Cursor stores either as a JSON object or as a
/// JSON-encoded string.
fn tool_params(tfd: &Value) -> Option<Value> {
    match tfd.get("params") {
        Some(Value::Object(o)) => Some(Value::Object(o.clone())),
        Some(Value::String(s)) => serde_json::from_str(s).ok(),
        _ => None,
    }
}

/// Add a file path to `set`. Absolute paths go in directly; relative ones are
/// resolved against each of the composer's git-repo roots (Cursor's
/// `relativeWorkspacePath` is usually absolute but occasionally relative).
fn insert_path(set: &mut HashSet<PathBuf>, path: &str, repos: &[PathBuf]) {
    if path.starts_with('/') {
        set.insert(PathBuf::from(path));
    } else {
        for root in repos {
            set.insert(root.join(path));
        }
    }
}

/// Exact per-edit records from `patch-graph:<workspace>:meta:<hash>` blobs.
/// Only agent-authored patches are kept — those are the ones attributable to
/// an agent session.
fn read_patch_graph(conn: &Connection) -> HashMap<PathBuf, Vec<Patch>> {
    let mut out: HashMap<PathBuf, Vec<Patch>> = HashMap::new();
    let Ok(mut stmt) =
        conn.prepare("SELECT value FROM cursorDiskKV WHERE key LIKE 'patch-graph:%:meta:%'")
    else {
        return out;
    };
    let rows = stmt.query_map([], |row| row.get::<_, Option<String>>(0));
    let Ok(rows) = rows else { return out };
    for value in rows.flatten().flatten() {
        let Ok(v) = serde_json::from_str::<Value>(&value) else {
            continue;
        };
        let Some(patches) = v.get("patches").and_then(|p| p.as_array()) else {
            continue;
        };
        for patch in patches {
            let Some(attribs) = patch.get("attribs") else {
                continue;
            };
            if attribs.get("patchSource").and_then(|s| s.as_str()) != Some("agent") {
                continue;
            }
            let (Some(path), Some(ts), Some(composer)) = (
                attribs
                    .get("fileUri")
                    .and_then(|u| u.get("path"))
                    .and_then(|p| p.as_str()),
                attribs.get("timestamp").and_then(|t| t.as_i64()),
                attribs
                    .get("composerMetadata")
                    .and_then(|m| m.get("composerId"))
                    .and_then(|c| c.as_str()),
            ) else {
                continue;
            };
            out.entry(PathBuf::from(path)).or_default().push(Patch {
                ts_ms: ts,
                composer: composer.to_string(),
            });
        }
    }
    out
}

/// Read a non-empty string field from a JSON object.
fn nonempty_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn global_state_db() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let db = PathBuf::from(home)
        .join("Library/Application Support/Cursor/User/globalStorage/state.vscdb");
    db.exists().then_some(db)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Non-hermetic smoke test against the local Cursor install. Ignored by
    /// default (`cargo test -- --ignored --nocapture` to run); verifies the
    /// map is populated and that each layer of `attribute` actually resolves.
    #[test]
    #[ignore]
    fn smoke_load_and_attribute() {
        let s = CursorSessions::load();
        eprintln!(
            "patches(paths)={} composers={} names={} prompts={}",
            s.patches.len(),
            s.composers.len(),
            s.names.len(),
            s.prompts.len(),
        );
        assert!(!s.composers.is_empty(), "expected some composers");
        assert!(!s.names.is_empty(), "expected some composer names");
        assert!(!s.prompts.is_empty(), "expected some composer prompts");
        let with_roots = s.prompts.iter().filter(|p| !p.project_roots.is_empty()).count();
        eprintln!("prompts with a project chain: {with_roots}/{}", s.prompts.len());

        // Every prompt must carry its composer as session_id and a Prompt kind.
        for p in s.prompts.iter().take(3) {
            assert!(p.is_prompt());
            assert!(p.session_id.is_some());
            eprintln!(
                "  prompt @ {} sess={:?} roots={} text={:?}",
                p.timestamp,
                p.session_id,
                p.project_roots.len(),
                p.prompt_text().map(|t| t.chars().take(50).collect::<String>()),
            );
        }

        // patch-graph layer: every recorded agent patch must attribute to its
        // own composer at its own timestamp.
        let mut patch_checks = 0;
        for (path, list) in s.patches.iter().take(20) {
            for p in list {
                assert_eq!(
                    s.attribute(path, p.ts_ms).as_deref(),
                    Some(p.composer.as_str()),
                    "patch at {} should attribute to its composer",
                    path.display()
                );
                patch_checks += 1;
            }
        }
        eprintln!("verified {patch_checks} patch-graph attributions");

        // window layer: an edited file at the composer's created time should
        // attribute to *some* composer (usually this one).
        let windowed = s
            .composers
            .iter()
            .find(|c| !c.files.is_empty())
            .expect("a composer with a known file set");
        let f = windowed.files.iter().next().unwrap();
        let attributed = s.attribute(f, windowed.created);
        eprintln!(
            "window sample: {} @ {} -> {:?} (name {:?})",
            f.display(),
            windowed.created,
            attributed,
            attributed.as_deref().and_then(|c| s.names_map().get(c)),
        );
        assert!(attributed.is_some(), "windowed edit should attribute");
    }
}
