//! Cursor **agent session** ("composer" — one chat/agent conversation)
//! integration. Two jobs, both from `globalStorage/state.vscdb`:
//!
//! - **Prompts.** Each composer's user messages (type-1 bubbles) become
//!   [`EntryKind::Prompt`] entries keyed to the `composerId`, exactly
//!   like Claude Code prompts — so a Cursor session shows its prompts and
//!   edits together under one colored session bar. Each prompt carries the
//!   agent's final response of its turn; see [`attach_responses`].
//! - **Agent edits.** Every `edit_file*` / `search_replace` tool call records
//!   the whole file content before and after the call, as two blobs in the
//!   same store. Each call becomes a [`EntryKind::FileChange`] entry
//!   with an exact composer and an exact diff base. Cursor writes these
//!   content ids from 2026-01; an older call has none.
//! - **Edit attribution.** VS Code Local History records the *user's* saves
//!   (and the agent edits of old clients); this module tags each with the
//!   composer that produced it (see below), so an old agent edit still shares
//!   its session's bar and a manual save stays unattributed.
//!
//! **The scan result lives in a cache file.** The store is a 9 GB SQLite
//! database, and a scan of every message bubble needs about a minute. The
//! loader keeps the result of each composer in
//! `~/.cache/recap/cursor/sessions.json`, with the number of bubble rows and
//! the `lastUpdatedAt` of the composer at scan time. A later load reads only
//! the composers whose count or `lastUpdatedAt` changed.
//!
//! **Local History alone is not enough.** It only records a save that the
//! editor window performs. The modern `edit_file_v2` tool writes the file
//! itself, so it leaves no snapshot: of the agent edits after 2026-08-01, only
//! 3% have one. The content blobs cover the rest.
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
//! The join stays fuzzy because a Local History snapshot carries no author and
//! no content id — hence the timestamp/window match. An edit that comes from a
//! content blob skips both layers: the tool call names its composer.

use crate::history::{find_project_chain, HistoryEntry};
use recap_api::{EditSource, EntryKind};
use chrono::{DateTime, Local};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
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
    /// The composer's bubbles in conversation order, as `(bubbleId, type)`
    /// from `composerData.fullConversationHeadersOnly`. Type 1 is a user
    /// prompt, type 2 an agent bubble. Empty for an old composer that has no
    /// header list; such a composer gets no responses.
    bubble_order: Vec<(String, i64)>,
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
    /// Cursor agent **file edits**, read from the tool call's own before/after
    /// content blobs. Each carries its composer in `session_id` and its exact
    /// before-content in `base_path`, so no fuzzy attribution runs on them.
    edits: Vec<HistoryEntry>,
}

impl CursorSessions {
    /// Build from Cursor's `globalStorage/state.vscdb`. Best-effort: any read
    /// error yields an empty map (attribution simply degrades to `None`), so
    /// this never blocks the loader.
    ///
    /// NOTE: the first load scans a multi-gigabyte SQLite store, which needs
    /// about a minute. A later load reads only the composers that changed
    /// since the cache file was written, but it must still run off the UI
    /// thread — see [`Self::load_cached`].
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
        let counts = read_bubble_counts(&conn);

        // Every composer that has a header or a bubble. A composer with only
        // a `composerData` row has no prompts and no edits, so it is not
        // visible and gets no scan.
        let ids: HashSet<&String> = metas.keys().chain(counts.keys()).collect();

        let mut cache = SessionsCache::load();
        let mut changed = cache.composers.len() != ids.len();
        cache.composers.retain(|id, _| ids.contains(id));
        let mut chain_cache: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for id in ids {
            let count = counts.get(id).copied().unwrap_or(0);
            let header = metas.get(id);
            let fresh = cache.composers.get(id).is_some_and(|c| {
                c.bubble_count == count
                    && header
                        .and_then(|m| m.updated)
                        .is_none_or(|updated| Some(updated) == c.updated)
            });
            if fresh {
                continue;
            }
            let mut meta = header.cloned().unwrap_or_default();
            read_composer_data(&conn, id, &mut meta);
            let scanned = scan_composer(&conn, id, &meta, count, &mut chain_cache);
            cache.composers.insert(id.clone(), scanned);
            changed = true;
        }
        if changed {
            cache.save();
        }

        let patches = read_patch_graph(&conn);

        let mut composers = Vec::new();
        let mut names = HashMap::new();
        let mut prompts = Vec::new();
        let mut edits = Vec::new();
        for (id, cached) in cache.composers {
            // The header holds the current name and window: the user can
            // rename a chat after the scan.
            let header = metas.get(&id);
            let name = header.and_then(|m| m.name.clone()).or(cached.name);
            let created = header.and_then(|m| m.created).or(cached.created);
            let updated = header.and_then(|m| m.updated).or(cached.updated);
            if let Some(name) = name {
                names.insert(id.clone(), name);
            }
            if let (Some(created), Some(updated)) = (created, updated) {
                composers.push(Composer {
                    files: cached.files.into_iter().collect(),
                    id,
                    created,
                    updated,
                });
            }
            prompts.extend(cached.prompts.into_iter().map(HistoryEntry::from));
            edits.extend(cached.edits.into_iter().map(HistoryEntry::from));
        }

        Ok(Self {
            patches,
            composers,
            names,
            prompts,
            edits,
        })
    }

    /// The Cursor agent prompt entries (user messages), ready to merge into the
    /// history. Each carries `session_id = composerId`.
    pub fn prompts(&self) -> &[HistoryEntry] {
        &self.prompts
    }

    /// The Cursor agent file edits, ready to merge into the history. Each
    /// carries `session_id = composerId` and an exact `base_path`.
    pub fn edits(&self) -> &[HistoryEntry] {
        &self.edits
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

/// The format version of the cache file. A change to the cached types bumps
/// it; a file with another version gets a full rebuild. Version 2 stores the
/// snapshot paths relative to the cache root. The migration in
/// [`crate::cache`] changes a version 1 file.
const CACHE_VERSION: u32 = 2;

/// The scan results, one per composer, in `~/.cache/recap/cursor/sessions.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionsCache {
    version: u32,
    composers: HashMap<String, CachedComposer>,
}

/// The scan result of one composer, with the two values that tell whether
/// the composer changed after the scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedComposer {
    /// The number of `bubbleId:<composerId>:*` rows at scan time.
    bubble_count: usize,
    created: Option<i64>,
    updated: Option<i64>,
    name: Option<String>,
    files: Vec<PathBuf>,
    prompts: Vec<CachedEntry>,
    edits: Vec<CachedEntry>,
}

/// A [`HistoryEntry`] of the Cursor source in the cache file. The timestamp
/// is unix milliseconds, the precision of Cursor's own timestamps. The
/// snapshot paths are relative to the cache root, so a copy of the cache at
/// another location stays valid.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedEntry {
    ts_ms: i64,
    file_path: PathBuf,
    snapshot_path: PathBuf,
    base_path: Option<PathBuf>,
    project_roots: Vec<PathBuf>,
    /// `Some((text, response))` for a prompt, `None` for a file change.
    prompt: Option<(String, Option<String>)>,
    session_id: Option<String>,
    session_cwd: Option<PathBuf>,
}

impl From<&HistoryEntry> for CachedEntry {
    fn from(e: &HistoryEntry) -> Self {
        let prompt = match &e.kind {
            EntryKind::Prompt { text, response } => Some((text.clone(), response.clone())),
            // The Cursor loader makes no compaction entry.
            EntryKind::FileChange | EntryKind::Compaction { .. } => None,
        };
        Self {
            ts_ms: e.timestamp.timestamp_millis(),
            file_path: e.file_path.clone(),
            snapshot_path: relative_to_cache(&e.snapshot_path),
            base_path: e.base_path.as_deref().map(relative_to_cache),
            project_roots: e.project_roots.clone(),
            prompt,
            session_id: e.session_id.clone(),
            session_cwd: e.session_cwd.clone(),
        }
    }
}

impl From<CachedEntry> for HistoryEntry {
    fn from(c: CachedEntry) -> Self {
        let kind = match c.prompt {
            Some((text, response)) => EntryKind::Prompt { text, response },
            None => EntryKind::FileChange,
        };
        Self {
            timestamp: DateTime::from_timestamp_millis(c.ts_ms)
                .unwrap_or_default()
                .with_timezone(&Local),
            file_path: c.file_path,
            snapshot_path: absolute_in_cache(c.snapshot_path),
            base_path: c.base_path.map(absolute_in_cache),
            project_roots: c.project_roots,
            source: EditSource::Cursor,
            kind,
            session_id: c.session_id,
            session_cwd: c.session_cwd,
        }
    }
}

/// `path` without the cache root, if the path is inside the cache root.
fn relative_to_cache(path: &Path) -> PathBuf {
    path.strip_prefix(crate::paths::cache_root())
        .unwrap_or(path)
        .to_path_buf()
}

/// A path of the cache file as an absolute path. An empty path (the snapshot
/// of a prompt) stays empty.
fn absolute_in_cache(path: PathBuf) -> PathBuf {
    if path.as_os_str().is_empty() || path.is_absolute() {
        path
    } else {
        crate::paths::cache_root().join(path)
    }
}

impl SessionsCache {
    fn path() -> Option<PathBuf> {
        Some(blob_cache_dir()?.join("sessions.json"))
    }

    /// Read the cache file. A missing file, a parse error, or another format
    /// version gives an empty cache, so every composer gets a scan.
    fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(data) = fs::read(&path) else {
            return Self::default();
        };
        match serde_json::from_slice::<Self>(&data) {
            Ok(cache) if cache.version == CACHE_VERSION => cache,
            _ => Self::default(),
        }
    }

    /// Write the cache file through a temporary file, so a crash during the
    /// write leaves the old file in place.
    fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        let write = || -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let data = serde_json::to_vec(&Self {
                version: CACHE_VERSION,
                composers: self.composers.clone(),
            })?;
            crate::cache::write_atomic(&path, &data)
        };
        if let Err(e) = write() {
            log::warn!("cursor sessions cache write failed: {e}");
        }
    }
}

/// Scan every bubble of one composer and build its cache record.
fn scan_composer(
    conn: &Connection,
    composer: &str,
    meta: &ComposerMeta,
    bubble_count: usize,
    chain_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
) -> CachedComposer {
    let (files, mut prompts, records) = read_bubbles(conn, composer, meta, chain_cache);
    attach_responses(conn, composer, meta, &mut prompts);

    // A composer's `trackedGitRepos` are often empty, leaving its prompts
    // with no project chain. The files the agent actually edited are a
    // better signal of where the work happened, so backfill any empty
    // chain from the composer's edited file set (deterministic pick).
    if let Some(file) = files.iter().min() {
        for p in &mut prompts {
            if p.project_roots.is_empty() {
                p.project_roots = chain_cache
                    .entry(file.clone())
                    .or_insert_with(|| find_project_chain(file))
                    .clone();
            }
        }
    }

    let edits = build_edits(conn, records, chain_cache);
    CachedComposer {
        bubble_count,
        created: meta.created,
        updated: meta.updated,
        name: meta.name.clone(),
        files: files.into_iter().collect(),
        prompts: prompts.iter().map(CachedEntry::from).collect(),
        edits: edits.iter().map(CachedEntry::from).collect(),
    }
}

/// The number of `bubbleId:<composerId>:*` rows per composer. The key column
/// has a covering index, so this reads no bubble content.
fn read_bubble_counts(conn: &Connection) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let Ok(mut stmt) =
        conn.prepare("SELECT key FROM cursorDiskKV WHERE key >= 'bubbleId:' AND key < 'bubbleId;'")
    else {
        return counts;
    };
    let rows = stmt.query_map([], |row| row.get::<_, String>(0));
    let Ok(rows) = rows else { return counts };
    for key in rows.flatten() {
        // key = "bubbleId:<composerId>:<bubbleId>"
        let mut parts = key.splitn(3, ':');
        let (_, Some(composer), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        *counts.entry(composer.to_string()).or_default() += 1;
    }
    counts
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

/// Fill `meta` from the composer's `cursorDiskKV` `composerData:<id>` blob.
/// Only fills fields left unset by the header pass, so newer metadata wins.
/// The bubble order comes from here alone.
fn read_composer_data(conn: &Connection, composer: &str, meta: &mut ComposerMeta) {
    let Ok(mut stmt) = conn.prepare("SELECT value FROM cursorDiskKV WHERE key = ?1") else {
        return;
    };
    let key = format!("composerData:{composer}");
    let Ok(value) = stmt.query_row([&key], |row| row.get::<_, Option<String>>(0)) else {
        return;
    };
    let Some(v) = value.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()) else {
        return;
    };
    if meta.created.is_none() {
        meta.created = v.get("createdAt").and_then(|x| x.as_i64());
    }
    if meta.updated.is_none() {
        meta.updated = v.get("lastUpdatedAt").and_then(|x| x.as_i64());
    }
    if meta.name.is_none() {
        meta.name = nonempty_str(&v, "name");
    }
    if let Some(headers) = v.get("fullConversationHeadersOnly").and_then(|h| h.as_array()) {
        meta.bubble_order = headers
            .iter()
            .filter_map(|h| {
                let id = h.get("bubbleId").and_then(|b| b.as_str())?;
                let kind = h.get("type").and_then(|t| t.as_i64())?;
                Some((id.to_string(), kind))
            })
            .collect();
    }
}

/// Attach the agent's final response to each prompt of one composer. The
/// response is the last type-2 bubble with text and no tool call before the
/// next prompt, in the composer's header order. The function reads only the
/// bubbles at the end of each turn, by exact key.
fn attach_responses(
    conn: &Connection,
    composer: &str,
    meta: &ComposerMeta,
    prompts: &mut [HistoryEntry],
) {
    let Ok(mut stmt) = conn.prepare("SELECT value FROM cursorDiskKV WHERE key = ?1") else {
        return;
    };
    // bubble → index in `prompts`.
    let mut by_bubble: HashMap<String, usize> = HashMap::new();
    for (i, p) in prompts.iter().enumerate() {
        let key = p.file_path.to_string_lossy();
        let mut parts = key.splitn(3, ':');
        if let (Some("cursor"), Some(_), Some(bubble)) = (parts.next(), parts.next(), parts.next())
        {
            by_bubble.insert(bubble.to_string(), i);
        }
    }

    {
        let order = &meta.bubble_order;
        for (start, (bubble, kind)) in order.iter().enumerate() {
            if *kind != 1 {
                continue;
            }
            let Some(&idx) = by_bubble.get(bubble) else {
                continue;
            };
            // The turn runs until the next prompt. Walk it backwards: the
            // first bubble with text is the response; a tool call before
            // that means the turn ended in a tool call and has no response.
            let end = order[start + 1..]
                .iter()
                .position(|(_, k)| *k == 1)
                .map(|n| start + 1 + n)
                .unwrap_or(order.len());
            let mut response: Option<String> = None;
            for (id, _) in order[start + 1..end].iter().rev() {
                let key = format!("bubbleId:{composer}:{id}");
                let Ok(value) = stmt.query_row([&key], |row| row.get::<_, Option<String>>(0))
                else {
                    continue;
                };
                let Some(v) = value.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok())
                else {
                    continue;
                };
                if v.get("toolFormerData").is_some() {
                    break;
                }
                if let Some(text) = nonempty_str(&v, "text") {
                    response = Some(text);
                    break;
                }
            }
            if let EntryKind::Prompt { response: slot, .. } = &mut prompts[idx].kind {
                *slot = response;
            }
        }
    }
}

/// Scan the message bubbles of one composer, extracting three things:
///
/// - the **set of files edited** (for the fuzzy window attribution),
/// - the **user prompts** (type-1 bubbles), as [`EntryKind::Prompt`]
///   entries keyed to the composer — the Cursor analogue of Claude prompts,
/// - the **edit records** of the agent's edit tool calls.
///
/// The key range `bubbleId:<composerId>:` uses the key index. A `LIKE` on
/// the key would scan the whole index instead, which needs 10 s on a large
/// store.
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
    composer: &str,
    meta: &ComposerMeta,
    chain_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
) -> (HashSet<PathBuf>, Vec<HistoryEntry>, Vec<EditRecord>) {
    let mut set: HashSet<PathBuf> = HashSet::new();
    let mut prompts: Vec<HistoryEntry> = Vec::new();
    let mut records: Vec<EditRecord> = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT key, value FROM cursorDiskKV WHERE key >= ?1 AND key < ?2 \
         AND (value LIKE '%\"codeBlocks\":[{%' OR value LIKE '%\"diffsSinceLastApply\":[{%' \
              OR value LIKE '%search_replace%' OR value LIKE '%edit_file%' \
              OR value LIKE '%\"type\":1,%')",
    ) else {
        return (set, prompts, records);
    };
    let lower = format!("bubbleId:{composer}:");
    let upper = format!("bubbleId:{composer};");
    let rows = stmt.query_map([&lower, &upper], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    });
    let Ok(rows) = rows else { return (set, prompts, records) };
    let repos = meta.repo_paths.as_slice();
    for (key, value) in rows.flatten() {
        // key = "bubbleId:<composerId>:<bubbleId>"
        let Some(bubble) = key.rsplit(':').next() else {
            continue;
        };
        let Some(v) = value.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()) else {
            continue;
        };

        // User prompt (type-1) bubble → a Prompt history entry.
        if v.get("type").and_then(|t| t.as_i64()) == Some(1) {
            if let Some(entry) = prompt_entry(&v, composer, bubble, Some(meta), chain_cache) {
                prompts.push(entry);
            }
        }

        let set = &mut set;

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
                    if let Some(record) = edit_record(&v, tfd, p, composer, repos) {
                        records.push(record);
                    }
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
    (set, prompts, records)
}

/// One `edit_file*` / `search_replace` tool call that changed a file. Cursor
/// stores the whole file content before and after the call, so an edit needs
/// no fuzzy attribution and no previous snapshot.
#[derive(Debug, Clone)]
struct EditRecord {
    composer: String,
    path: PathBuf,
    timestamp: DateTime<Local>,
    /// `cursorDiskKV` keys of the two content blobs (`composer.content.<sha256>`).
    before_key: String,
    after_key: String,
}

/// Read the before/after content ids out of one edit tool call. Returns `None`
/// when the client wrote no content ids (Cursor added them in 2026-01), when
/// the bubble has no timestamp, or when the call changed nothing.
fn edit_record(
    v: &Value,
    tfd: &Value,
    path: &str,
    composer: &str,
    repos: &[PathBuf],
) -> Option<EditRecord> {
    let result = decode_json(tfd.get("result")?)?;
    let before_key = nonempty_str(&result, "beforeContentId")?;
    let after_key = nonempty_str(&result, "afterContentId")?;
    if before_key == after_key {
        return None;
    }
    let ts_str = v.get("createdAt").and_then(|t| t.as_str())?;
    let timestamp = DateTime::parse_from_rfc3339(ts_str).ok()?.with_timezone(&Local);

    // `relativeWorkspacePath` is usually absolute; resolve a relative one
    // against the composer's first repo root, as `insert_path` does.
    let file_path = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        repos.first()?.join(path)
    };

    Some(EditRecord {
        composer: composer.to_string(),
        path: file_path,
        timestamp,
        before_key,
        after_key,
    })
}

/// Turn the collected edit records into history entries. Each entry needs two
/// files on disk (the diff code reads paths), so this copies the content blobs
/// into `~/.cache/recap/cursor/<sha256>`. The blob key already **is** the
/// sha256 of the content, so the cache file name deduplicates content that
/// several edits share, and a blob that the cache already holds needs no read
/// from the multi-gigabyte store.
fn build_edits(
    conn: &Connection,
    records: Vec<EditRecord>,
    chain_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
) -> Vec<HistoryEntry> {
    let Some(cache) = blob_cache_dir() else {
        return Vec::new();
    };
    if let Err(e) = fs::create_dir_all(&cache) {
        log::warn!("cursor blob cache unavailable: {e}");
        return Vec::new();
    }
    let Ok(mut stmt) = conn.prepare("SELECT value FROM cursorDiskKV WHERE key = ?1") else {
        return Vec::new();
    };

    let mut out = Vec::with_capacity(records.len());
    for record in records {
        let (Some(after), Some(before)) = (
            materialize(&mut stmt, &cache, &record.after_key),
            materialize(&mut stmt, &cache, &record.before_key),
        ) else {
            // Cursor keeps the blobs of recent edits only. An edit whose
            // content is gone stays with the Local History source.
            continue;
        };
        let project_roots = chain_cache
            .entry(record.path.clone())
            .or_insert_with(|| find_project_chain(&record.path))
            .clone();
        out.push(HistoryEntry {
            timestamp: record.timestamp,
            file_path: record.path,
            snapshot_path: after,
            base_path: Some(before),
            project_roots,
            source: EditSource::Cursor,
            kind: EntryKind::FileChange,
            session_id: Some(record.composer),
            session_cwd: None,
        });
    }
    out
}

/// Path of the cache file that holds the content of blob `key`, after a copy
/// from the store when the cache does not hold it yet. Returns `None` when the
/// store has no such blob.
fn materialize(stmt: &mut rusqlite::Statement<'_>, cache: &Path, key: &str) -> Option<PathBuf> {
    let sha = key.rsplit('.').next()?;
    if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let path = cache.join(sha);
    if path.exists() {
        return Some(path);
    }
    // Cursor stores the content as TEXT, but a blob column can hold either.
    let content: Vec<u8> = match stmt.query_row([key], |row| row.get::<_, String>(0)) {
        Ok(text) => text.into_bytes(),
        Err(_) => stmt.query_row([key], |row| row.get::<_, Vec<u8>>(0)).ok()?,
    };
    match crate::cache::write_atomic(&path, &content) {
        Ok(()) => Some(path),
        Err(e) => {
            log::warn!("cursor blob write failed: {e}");
            None
        }
    }
}

fn blob_cache_dir() -> Option<PathBuf> {
    Some(crate::paths::cache_root().join("cursor"))
}

/// Build a [`EntryKind::Prompt`] entry from a type-1 (user) bubble, or
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
        base_path: None,
        project_roots,
        source: EditSource::Cursor,
        kind: EntryKind::Prompt {
            text,
            response: None,
        },
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
    decode_json(tfd.get("params")?)
}

/// Cursor stores a tool call's `params` and `result` as an object, as a
/// JSON-encoded string, or as a JSON-encoded string of a JSON-encoded string.
/// Unwrap the string layers until an object appears.
fn decode_json(v: &Value) -> Option<Value> {
    let mut current = v.clone();
    for _ in 0..3 {
        match current {
            Value::Object(_) => return Some(current),
            Value::String(ref s) => current = serde_json::from_str(s).ok()?,
            _ => return None,
        }
    }
    None
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
        conn.prepare(
            "SELECT value FROM cursorDiskKV WHERE key >= 'patch-graph:' AND key < 'patch-graph;' \
             AND key LIKE 'patch-graph:%:meta:%'",
        )
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
    use recap_api::EntryFields;

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
        let with_response = s.prompts.iter().filter(|p| p.response_text().is_some()).count();
        eprintln!("prompts with a response: {with_response}/{}", s.prompts.len());
        if let Some(p) = s.prompts.iter().find(|p| p.response_text().is_some()) {
            eprintln!(
                "  sample response: {:?}",
                p.response_text().map(|t| t.chars().take(80).collect::<String>()),
            );
        }

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

    /// Non-hermetic smoke test for the blob edit source. Ignored by default.
    #[test]
    #[ignore]
    fn smoke_blob_edits() {
        let s = CursorSessions::load();
        eprintln!("blob edits = {}", s.edits.len());
        assert!(!s.edits.is_empty(), "expected agent edits from content blobs");
        let mut by_file: HashMap<PathBuf, usize> = HashMap::new();
        for e in &s.edits {
            assert!(e.base_path.is_some());
            assert!(e.session_id.is_some());
            *by_file.entry(e.file_path.clone()).or_default() += 1;
        }
        let mut top: Vec<_> = by_file.iter().collect();
        top.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        for (f, n) in top.iter().take(5) {
            eprintln!("  {n:4} edits  {}", f.display());
        }
        let with_roots = s.edits.iter().filter(|e| !e.project_roots.is_empty()).count();
        eprintln!("edits with a project chain: {with_roots}/{}", s.edits.len());
        let newest = s.edits.iter().max_by_key(|e| e.timestamp).unwrap();
        eprintln!("newest: {} {}", newest.timestamp, newest.file_path.display());
    }
}
