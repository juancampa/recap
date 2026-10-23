//! The server's copy of the history, and the views that the API returns.
//!
//! [`Store`] wraps the [`HistoryStore`] and adds one [`EntryId`] and one
//! version to each entry. The store version increases each time a
//! [`Store::commit`] finds a change. An entry records the store version of
//! the last change to its wire form, which lets a client ask for the entries
//! that changed after a version (`?since=`).

use crate::cursor_sessions::CursorSessions;
use crate::history::{HistoryEntry, HistoryStore};
use recap_api::{
    Agent, EditSource, Entry, EntryFields, EntryId, EntryKind, FileInfo, Filter, HistoryGroup,
    HistoryResponse, ListItem, Page, Project,
};
use sha2::{Digest, Sha256};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// The identity of the searchable text of an entry, as a string that does not
/// depend on the cache location. A file change keys on its snapshot file (one
/// for each edit). A prompt and a compaction key on the synthetic
/// `file_path`. The part of a
/// path inside the cache root is relative, so the prod cache and a copy of it
/// give the same keys. The vector store (`embeddings.db`) uses the same key.
pub fn entry_key(entry: &HistoryEntry) -> String {
    let raw = match &entry.kind {
        EntryKind::Prompt { .. } | EntryKind::Compaction { .. } => {
            entry.file_path.to_string_lossy().into_owned()
        }
        // A Cursor blob snapshot has the name of its content hash, so two
        // edits that end at the same content have the same snapshot. Their
        // diffs differ, so the key also holds the base.
        EntryKind::FileChange => match &entry.base_path {
            Some(base) => format!("{}::{}", base.display(), entry.snapshot_path.display()),
            None => entry.snapshot_path.to_string_lossy().into_owned(),
        },
    };
    relative_to_cache(&raw)
}

/// Remove the cache root prefix from each path in `key`.
fn relative_to_cache(key: &str) -> String {
    let root = format!("{}/", crate::paths::cache_root().display());
    key.replace(&root, "")
}

/// The id of `entry`: the first 16 hex digits of the SHA-256 of its identity.
///
/// The identity is the [`entry_key`], except for an edit with a base. The
/// Cursor agent edits have a base, and Cursor names each snapshot and base
/// file with the hash of its content. Thus two Cursor edits can have the
/// same key: the same change to two files, or the same change at two times.
/// Their vectors are the same, but they are different entries, so the
/// identity also holds the file and the time. The Claude Code edits also
/// have a base. Their keys are unique already, and the extra parts do not
/// change that.
pub fn entry_id(entry: &HistoryEntry) -> EntryId {
    let key = entry_key(entry);
    let identity = match entry.base_path {
        Some(_) => format!(
            "{key}::{}::{}",
            entry.file_path.display(),
            entry.timestamp.timestamp_millis()
        ),
        None => key,
    };
    let digest = Sha256::digest(identity.as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    EntryId(hex)
}

/// The server data of one entry, at the same index as the entry in
/// `history.entries`.
#[derive(Debug, Clone)]
struct Meta {
    id: EntryId,
    /// The store version of the last change to the wire form of the entry.
    version: u64,
    /// A hash of the wire fields that the store calculates from other
    /// entries (the base snapshot, the session title, the prompt ordinal and
    /// the session). [`Store::commit`] compares it to find the entries that
    /// changed because other entries arrived.
    derived: u64,
    /// True when a merge added or replaced the entry after the last commit.
    dirty: bool,
}

pub struct Store {
    pub history: HistoryStore,
    meta: Vec<Meta>,
    by_id: HashMap<EntryId, usize>,
    version: u64,
    /// The position of each prompt in its own session chain, counted from
    /// the oldest prompt as 1. The key is the synthetic `file_path` of the
    /// prompt.
    /// [`Store::commit`] calculates it again from `history.entries`.
    ordinals: HashMap<PathBuf, usize>,
    /// The ids of the Claude Code sessions that Claude Code deleted. The
    /// loader read their entries from recap's archive (see
    /// [`crate::claude::load`]).
    archived_sessions: HashSet<String>,
    /// Agent session id → the summary that Claude Code shows while the
    /// session is idle (see [`crate::claude::load`]).
    session_summaries: HashMap<String, String>,
    /// Agent session id → the session that it continues (see
    /// [`crate::claude::SessionFacts::parents`]). [`Self::chain_id`] follows
    /// these links.
    session_parents: HashMap<String, String>,
    /// Subagent session id → the session that started it (see
    /// [`CursorSessions::subagent_parents`]). [`Self::chain_id`] also follows
    /// these links. The prompts of a subagent session come from the parent
    /// agent, so they get no ordinal and do not count as prompts of the
    /// agent.
    subagent_parents: HashMap<String, String>,
}

impl Default for Store {
    fn default() -> Self {
        Store {
            history: HistoryStore::from_entries(Vec::new()),
            meta: Vec::new(),
            by_id: HashMap::new(),
            version: 0,
            ordinals: HashMap::new(),
            archived_sessions: HashSet::new(),
            session_summaries: HashMap::new(),
            session_parents: HashMap::new(),
            subagent_parents: HashMap::new(),
        }
    }
}


/// The maximum number of chars in each text of an [`Agent`]. The Agents
/// panel shows each text on one line, so it never shows more chars than
/// this. The limit keeps the `/v1/agents` response small.
const AGENT_TEXT_CHARS: usize = 300;

/// The start of `text` on one line, at most [`AGENT_TEXT_CHARS`] chars.
fn agent_line(text: &str) -> String {
    let mut line = recap_api::single_line(text);
    if let Some((end, _)) = line.char_indices().nth(AGENT_TEXT_CHARS) {
        line.truncate(end);
    }
    line
}

impl Store {
    /// A store that holds `entries`, for tests and for a server without
    /// loaders.
    pub fn from_entries(entries: Vec<HistoryEntry>) -> Store {
        let mut store = Store::default();
        store.merge(entries);
        store.commit();
        store
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn len(&self) -> usize {
        self.history.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.history.entries.is_empty()
    }

    /// The index in `history.entries` of the entry with this id.
    pub fn index_of(&self, id: &EntryId) -> Option<usize> {
        self.by_id.get(id).copied()
    }

    pub fn id_at(&self, idx: usize) -> &EntryId {
        &self.meta[idx].id
    }

    /// The content of the entry at `idx` before the change: the base that the
    /// source recorded, else the previous snapshot of the same file.
    pub fn prev_snapshot(&self, idx: usize) -> Option<&Path> {
        self.history.prev_snapshot(idx)
    }

    /// Add a batch of new entries. An entry whose id the store already holds
    /// replaces the stored entry and keeps the id. A prompt arrives again
    /// when its source attached the agent's response after the earlier scan.
    /// The Cursor data can also hold identical copies of one edit. An
    /// identical copy is not a change. The caller must call [`Self::commit`]
    /// after the last change.
    pub fn merge(&mut self, entries: Vec<HistoryEntry>) {
        // `by_id` holds only the entries of the earlier merges, so the batch
        // adds its own entries to this map.
        let mut added: HashMap<EntryId, usize> = HashMap::new();
        for mut entry in entries {
            let id = entry_id(&entry);
            if let Some(&i) = self.by_id.get(&id).or_else(|| added.get(&id)) {
                if self.history.entries[i] != entry {
                    std::mem::swap(&mut self.history.entries[i], &mut entry);
                    self.meta[i].dirty = true;
                }
                continue;
            }
            added.insert(id.clone(), self.history.entries.len());
            self.history.entries.push(entry);
            self.meta.push(Meta {
                id,
                version: 0,
                derived: 0,
                dirty: true,
            });
        }

        // Sort the entries and their metadata with one permutation.
        let mut order: Vec<usize> = (0..self.history.entries.len()).collect();
        order.sort_by(|&a, &b| {
            recap_api::newest_first(
                (&self.history.entries[a].timestamp, &self.meta[a].id),
                (&self.history.entries[b].timestamp, &self.meta[b].id),
            )
        });
        let mut entries: Vec<Option<HistoryEntry>> = std::mem::take(&mut self.history.entries)
            .into_iter()
            .map(Some)
            .collect();
        let mut meta: Vec<Option<Meta>> = std::mem::take(&mut self.meta)
            .into_iter()
            .map(Some)
            .collect();
        for i in order {
            self.history
                .entries
                .push(entries[i].take().expect("each index once"));
            self.meta.push(meta[i].take().expect("each index once"));
        }
        self.history.recompute_projects();
        self.by_id = self
            .meta
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id.clone(), i))
            .collect();
    }

    /// The prompts and edits of the Cursor composer map that the store (and
    /// `pending`, the entries that wait for a merge) does not hold yet.
    ///
    /// A prompt keys on its synthetic `cursor:<composer>:<bubble>` path. A
    /// prompt with a response passes anyway, because [`Self::merge`] replaces
    /// the old entry, so a response that arrived after the last scan reaches
    /// the store. An edit keys on (path, time, snapshot), because the
    /// snapshot alone is content-addressed and two edits can produce one
    /// content.
    pub fn new_cursor_entries(
        &self,
        sessions: &CursorSessions,
        pending: &[HistoryEntry],
    ) -> Vec<HistoryEntry> {
        let all = || self.history.entries.iter().chain(pending.iter());
        let have: HashSet<&PathBuf> = all()
            .filter(|e| !e.is_file_change())
            .map(|e| &e.file_path)
            .collect();
        let mut fresh: Vec<HistoryEntry> = sessions
            .prompts()
            .iter()
            .filter(|p| !have.contains(&p.file_path) || p.response_text().is_some())
            .cloned()
            .collect();

        let have_edits: HashSet<(&PathBuf, chrono::DateTime<chrono::Local>, &PathBuf)> = all()
            .filter(|e| e.base_path.is_some())
            .map(|e| (&e.file_path, e.timestamp, &e.snapshot_path))
            .collect();
        fresh.extend(
            sessions
                .edits()
                .iter()
                .filter(|e| !have_edits.contains(&(&e.file_path, e.timestamp, &e.snapshot_path)))
                .cloned(),
        );
        fresh
    }

    /// Set the composer (agent session) of each Cursor Local History edit
    /// from the attribution map. The function gives the same result when it
    /// runs again.
    pub fn apply_cursor_attribution(&mut self, sessions: &CursorSessions) {
        for entry in &mut self.history.entries {
            if entry.source != EditSource::Cursor {
                continue;
            }
            // An entry with a base already has the composer that the tool
            // call named. A prompt has its own composer too, and its synthetic
            // path matches no file, so the fuzzy join would only erase that
            // composer. Only the Local History edits need the join.
            if entry.base_path.is_some() || !entry.is_file_change() {
                continue;
            }
            entry.session_id =
                sessions.attribute(&entry.file_path, entry.timestamp.timestamp_millis());
        }
    }

    /// Add session titles. Never replace the map: the Claude loader and the
    /// Cursor loader both write into it.
    pub fn extend_titles(&mut self, titles: impl IntoIterator<Item = (String, String)>) {
        self.history.session_titles.extend(titles);
    }

    /// Add session summaries. A newer summary of a session replaces the
    /// older one.
    pub fn extend_summaries(&mut self, summaries: impl IntoIterator<Item = (String, String)>) {
        self.session_summaries.extend(summaries);
    }

    fn session_summary(&self, entry: &HistoryEntry) -> Option<&String> {
        entry
            .session_id
            .as_ref()
            .and_then(|sid| self.session_summaries.get(sid))
    }

    /// Add the parent links of sessions.
    pub fn extend_parents(&mut self, parents: impl IntoIterator<Item = (String, String)>) {
        self.session_parents.extend(parents);
    }

    /// Add the links from subagent sessions to the sessions that started
    /// them.
    pub fn extend_subagent_parents(
        &mut self,
        parents: impl IntoIterator<Item = (String, String)>,
    ) {
        self.subagent_parents.extend(parents);
    }

    /// The first session of the session chain that holds `session_id` (see
    /// [`Entry::chain_id`]). The function follows the parent links and the
    /// subagent links. It stops after as many steps as there are links, so a
    /// loop in the links does not stop the server.
    pub fn chain_id<'a>(&'a self, session_id: &'a str) -> &'a str {
        let mut current = session_id;
        for _ in 0..self.session_parents.len() + self.subagent_parents.len() {
            let parent = self
                .session_parents
                .get(current)
                .or_else(|| self.subagent_parents.get(current));
            match parent {
                Some(parent) => current = parent,
                None => break,
            }
        }
        current
    }

    fn is_subagent(&self, session_id: Option<&str>) -> bool {
        session_id.is_some_and(|sid| self.subagent_parents.contains_key(sid))
    }

    fn entry_chain_id<'a>(&'a self, entry: &'a HistoryEntry) -> Option<&'a str> {
        entry.session_id.as_deref().map(|sid| self.chain_id(sid))
    }

    /// Replace the set of archived sessions. The Claude loader sends the full
    /// set in each load, so a session that Claude Code deleted after the
    /// last load becomes archived, and a restored session stops being
    /// archived.
    pub fn set_archived_sessions(&mut self, ids: HashSet<String>) {
        self.archived_sessions = ids;
    }

    fn is_archived(&self, session_id: Option<&str>) -> bool {
        session_id.is_some_and(|sid| self.archived_sessions.contains(sid))
    }

    /// Find the entries whose wire form changed since the last commit, give
    /// them a new version, and return their ids. The store version increases
    /// only when at least one entry changed.
    pub fn commit(&mut self) -> Vec<EntryId> {
        self.recompute_ordinals();
        let next = self.version + 1;
        let mut changed = Vec::new();
        for idx in 0..self.meta.len() {
            let derived = self.derived_hash(idx);
            let meta = &mut self.meta[idx];
            if meta.dirty || meta.derived != derived {
                meta.dirty = false;
                meta.derived = derived;
                meta.version = next;
                changed.push(meta.id.clone());
            }
        }
        if !changed.is_empty() {
            self.version = next;
        }
        changed
    }

    fn derived_hash(&self, idx: usize) -> u64 {
        let entry = &self.history.entries[idx];
        let mut h = DefaultHasher::new();
        self.history.prev_snapshot(idx).hash(&mut h);
        entry.session_id.hash(&mut h);
        self.entry_chain_id(entry).hash(&mut h);
        self.session_title(entry).hash(&mut h);
        self.session_summary(entry).hash(&mut h);
        self.is_archived(entry.session_id.as_deref()).hash(&mut h);
        self.is_subagent(entry.session_id.as_deref()).hash(&mut h);
        self.ordinal(entry).hash(&mut h);
        h.finish()
    }

    /// Calculate [`Self::ordinals`]. `history.entries` is newest first, so the
    /// walk runs backwards to count the prompts of each session chain from
    /// the oldest one. A subagent prompt does not count.
    fn recompute_ordinals(&mut self) {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let mut out: HashMap<PathBuf, usize> = HashMap::new();
        for e in self.history.entries.iter().rev() {
            if !e.is_prompt() || self.is_subagent(e.session_id.as_deref()) {
                continue;
            }
            let Some(chain) = self.entry_chain_id(e) else {
                continue;
            };
            let n = counts.entry(chain).or_insert(0);
            *n += 1;
            out.insert(e.file_path.clone(), *n);
        }
        self.ordinals = out;
    }

    fn ordinal(&self, entry: &HistoryEntry) -> Option<usize> {
        if !entry.is_prompt() {
            return None;
        }
        self.ordinals.get(&entry.file_path).copied()
    }

    fn session_title(&self, entry: &HistoryEntry) -> Option<&String> {
        entry
            .session_id
            .as_ref()
            .and_then(|sid| self.history.session_titles.get(sid))
    }

    /// The wire form of the entry at `idx`.
    pub fn wire(&self, idx: usize) -> Entry {
        let e = &self.history.entries[idx];
        let meta = &self.meta[idx];
        let is_file_change = e.is_file_change();
        Entry {
            id: meta.id.clone(),
            version: meta.version,
            timestamp: e.timestamp,
            file_path: e.file_path.clone(),
            project_roots: e.project_roots.clone(),
            source: e.source,
            kind: e.kind.clone(),
            session_id: e.session_id.clone(),
            chain_id: self.entry_chain_id(e).map(str::to_string),
            session_title: self.session_title(e).cloned(),
            session_summary: self.session_summary(e).cloned(),
            session_cwd: e.session_cwd.clone(),
            session_archived: self.is_archived(e.session_id.as_deref()),
            subagent: self.is_subagent(e.session_id.as_deref()),
            ordinal: self.ordinal(e),
            snapshot_path: is_file_change.then(|| e.snapshot_path.clone()),
            base_path: if is_file_change {
                self.history.prev_snapshot(idx).map(Path::to_path_buf)
            } else {
                None
            },
        }
    }

    /// Whether one entry passes the filter.
    pub fn keeps(&self, entry: &HistoryEntry, filter: &Filter) -> bool {
        if let Some(p) = &filter.project
            && !entry.project_roots.iter().any(|r| r == p)
        {
            return false;
        }
        if let Some(f) = &filter.file
            && &entry.file_path != f
        {
            return false;
        }
        // An empty agent id matches no entry, because an entry without a
        // session has no agent.
        if let Some(a) = &filter.agent
            && self.entry_chain_id(entry) != Some(a.as_str())
        {
            return false;
        }
        if let Some(after) = &filter.after
            && entry.timestamp < *after
        {
            return false;
        }
        filter.kind.keeps(entry)
    }

    /// The indices of the entries that pass the filter, newest first.
    pub fn filtered(&self, filter: &Filter) -> Vec<usize> {
        self.history
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.keeps(e, filter))
            .map(|(i, _)| i)
            .collect()
    }

    // ── Views ────────────────────────────────────────────────────────────────

    /// The projects, newest first.
    pub fn projects(&self) -> Vec<Project> {
        let h = &self.history;
        h.projects
            .iter()
            .map(|root| Project {
                root: root.clone(),
                name: recap_api::project_name(root, &h.project_names),
                chain: h.project_chains.get(root).cloned().unwrap_or_default(),
                latest: h.project_latest.get(root).copied().unwrap_or_default(),
                count: h.project_counts.get(root).copied().unwrap_or(0),
            })
            .collect()
    }

    /// The edited files inside `project` (or all files), newest first. A
    /// prompt and a compaction have no file: their `file_path` is synthetic.
    pub fn files(&self, project: Option<&Path>) -> Vec<FileInfo> {
        let mut by_path: HashMap<&PathBuf, FileInfo> = HashMap::new();
        for e in &self.history.entries {
            if !e.is_file_change() {
                continue;
            }
            if let Some(p) = project
                && !e.project_roots.iter().any(|r| r == p)
            {
                continue;
            }
            let info = by_path.entry(&e.file_path).or_insert_with(|| FileInfo {
                path: e.file_path.clone(),
                latest: e.timestamp,
                count: 0,
                chain: e.project_roots.clone(),
            });
            if e.timestamp > info.latest {
                info.latest = e.timestamp;
            }
            info.count += 1;
        }
        let mut files: Vec<FileInfo> = by_path.into_values().collect();
        files.sort_by(FileInfo::order);
        files
    }

    /// The session chains inside `project` (or all chains), newest first, as
    /// agents. An entry without a session id has no agent. `history.entries`
    /// is newest first, so the first entry of each chain is its newest entry,
    /// and the first prompt is its newest prompt. The session, the title and
    /// the prompts of the agent come from the sessions that are not
    /// subagents. A subagent session gives them only when the chain has no
    /// other session.
    pub fn agents(&self, project: Option<&Path>) -> Vec<Agent> {
        let mut order: Vec<&str> = Vec::new();
        let mut by_id: HashMap<&str, Agent> = HashMap::new();
        // The oldest prompt and the newest response of each chain. The loop
        // converts them to one line only after it finds the final values.
        let mut first_prompts: HashMap<&str, &HistoryEntry> = HashMap::new();
        let mut last_responses: HashMap<&str, &str> = HashMap::new();
        for e in &self.history.entries {
            let Some(sid) = e.session_id.as_deref() else {
                continue;
            };
            if let Some(p) = project
                && !e.project_roots.iter().any(|r| r == p)
            {
                continue;
            }
            let chain = self.chain_id(sid);
            let subagent = self.is_subagent(Some(sid));
            let agent = by_id.entry(chain).or_insert_with(|| {
                order.push(chain);
                Agent {
                    chain_id: chain.to_string(),
                    session_id: sid.to_string(),
                    source: e.source,
                    title: None,
                    archived: self.is_archived(Some(sid)),
                    last_ts: e.timestamp,
                    prompt_count: 0,
                    entry_count: 0,
                    project_roots: e.project_roots.clone(),
                    first_prompt: String::new(),
                    first_ordinal: None,
                    last_prompt: String::new(),
                    last_ordinal: None,
                    summary: None,
                    last_response: None,
                }
            });
            agent.entry_count += 1;
            // The first entry of a subagent session made the agent. Take the
            // session and the title again from the first entry of a session
            // that is not a subagent.
            if !subagent && self.is_subagent(Some(&agent.session_id)) {
                agent.session_id = sid.to_string();
                agent.archived = self.is_archived(Some(sid));
                agent.title = None;
            }
            if agent.title.is_none() && (!subagent || self.is_subagent(Some(&agent.session_id))) {
                agent.title = self.history.session_titles.get(sid).cloned();
            }
            if let Some(text) = e.prompt_text()
                && !subagent
            {
                if agent.prompt_count == 0 {
                    // The newest prompt also gives the agent its display
                    // time, which the user reads as "when the agent last ran".
                    agent.last_ts = e.timestamp;
                    agent.last_prompt = agent_line(text);
                    agent.last_ordinal = self.ordinal(e);
                    if agent.project_roots.is_empty() {
                        agent.project_roots = e.project_roots.clone();
                    }
                }
                agent.prompt_count += 1;
                first_prompts.insert(chain, e);
                if let Some(response) = e.response_text() {
                    last_responses.entry(chain).or_insert(response);
                }
            }
        }
        let mut agents: Vec<Agent> = order
            .into_iter()
            .filter_map(|id| by_id.remove(id))
            .collect();
        for agent in &mut agents {
            let chain = agent.chain_id.as_str();
            if let Some(e) = first_prompts.get(chain) {
                agent.first_prompt = agent_line(e.prompt_text().unwrap_or(""));
                agent.first_ordinal = self.ordinal(e);
            }
            agent.last_response = last_responses.get(chain).map(|r| agent_line(r));
            agent.summary = self
                .session_summaries
                .get(&agent.session_id)
                .map(|s| agent_line(s));
        }
        agents.sort_by(Agent::order);
        agents
    }

    /// The entries that pass the filter, in groups. With `page.since`, only
    /// the entries that changed after that version, and the ids of the
    /// changed entries that do not pass the filter.
    pub fn history(&self, filter: &Filter, page: &Page) -> HistoryResponse {
        let limit = page.limit.unwrap_or(usize::MAX);
        let mut entries: Vec<Entry> = Vec::new();
        let mut removed: Vec<EntryId> = Vec::new();
        for (idx, e) in self.history.entries.iter().enumerate() {
            if let Some(since) = page.since
                && self.meta[idx].version <= since
            {
                continue;
            }
            if self.keeps(e, filter) {
                if entries.len() < limit {
                    entries.push(self.wire(idx));
                }
            } else if page.since.is_some() {
                removed.push(self.meta[idx].id.clone());
            }
        }
        let groups = recap_api::group_runs(&entries)
            .into_iter()
            .map(|run| HistoryGroup {
                entries: entries[run].to_vec(),
            })
            .collect();
        HistoryResponse {
            version: self.version,
            groups,
            removed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    #[test]
    fn agent_line_keeps_only_the_start_of_a_long_text() {
        let long = "é".repeat(AGENT_TEXT_CHARS + 50);
        assert_eq!(agent_line(&long).chars().count(), AGENT_TEXT_CHARS);
        assert_eq!(agent_line("a\n  b"), "a b");
    }

    fn prompt_entry(text: &str, session: &str, minutes_ago: i64) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local::now() - chrono::Duration::minutes(minutes_ago),
            file_path: PathBuf::from(format!("claude:{session}:{text}")),
            snapshot_path: PathBuf::new(),
            base_path: None,
            project_roots: vec![PathBuf::from("/dev/proj")],
            source: EditSource::ClaudeCode,
            kind: EntryKind::Prompt {
                text: text.to_string(),
                response: None,
            },
            session_id: Some(session.to_string()),
            session_cwd: None,
        }
    }

    fn nvim_entry(path: &str, snapshot: &str, minutes_ago: i64) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local::now() - chrono::Duration::minutes(minutes_ago),
            file_path: PathBuf::from(path),
            snapshot_path: PathBuf::from(snapshot),
            base_path: None,
            project_roots: vec![PathBuf::from("/dev/proj")],
            source: EditSource::Neovim,
            kind: EntryKind::FileChange,
            session_id: None,
            session_cwd: None,
        }
    }

    /// A session that continues another session joins its session chain.
    /// The Agents panel shows one agent for the chain, the prompt numbers
    /// continue across the sessions, and the agent filter keeps the entries
    /// of all the sessions of the chain.
    #[test]
    fn the_sessions_of_a_chain_are_one_agent() {
        let mut store = Store::from_entries(vec![
            prompt_entry("start", "root", 30),
            prompt_entry("continue", "child", 20),
            prompt_entry("branch", "other-child", 10),
            prompt_entry("alone", "solo", 5),
        ]);
        store.extend_parents([
            ("child".to_string(), "root".to_string()),
            ("other-child".to_string(), "root".to_string()),
        ]);
        store.commit();

        let agents = store.agents(None);
        let chains: Vec<(&str, &str, usize)> = agents
            .iter()
            .map(|a| (a.chain_id.as_str(), a.session_id.as_str(), a.prompt_count))
            .collect();
        assert_eq!(
            chains,
            vec![("solo", "solo", 1), ("root", "other-child", 3)]
        );

        let filter = Filter {
            agent: Some("root".to_string()),
            ..Filter::default()
        };
        let texts: Vec<(String, Option<usize>, Option<String>)> = store
            .filtered(&filter)
            .into_iter()
            .map(|i| {
                let e = store.wire(i);
                (e.prompt_text().unwrap().to_string(), e.ordinal, e.chain_id)
            })
            .collect();
        let root = Some("root".to_string());
        assert_eq!(
            texts,
            vec![
                ("branch".to_string(), Some(3), root.clone()),
                ("continue".to_string(), Some(2), root.clone()),
                ("start".to_string(), Some(1), root),
            ]
        );
    }

    /// A subagent session joins the session chain of its parent. Its prompts
    /// get no ordinal, and the agent takes its session, title and prompts
    /// from the parent only.
    #[test]
    fn a_subagent_joins_the_chain_of_its_parent() {
        let mut store = Store::from_entries(vec![
            prompt_entry("plan it", "parent", 30),
            prompt_entry("explore the code", "sub", 20),
            prompt_entry("now build it", "parent", 10),
            prompt_entry("review the build", "sub2", 5),
        ]);
        store.extend_subagent_parents([
            ("sub".to_string(), "parent".to_string()),
            ("sub2".to_string(), "parent".to_string()),
        ]);
        store.extend_titles([
            ("parent".to_string(), "Main".to_string()),
            ("sub2".to_string(), "Review".to_string()),
        ]);
        store.extend_summaries([
            ("parent".to_string(), "Built it.".to_string()),
            ("sub2".to_string(), "Reviewed it.".to_string()),
        ]);
        store.commit();

        let agents = store.agents(None);
        assert_eq!(agents.len(), 1);
        let agent = &agents[0];
        assert_eq!(agent.chain_id, "parent");
        assert_eq!(agent.session_id, "parent");
        assert_eq!(agent.title.as_deref(), Some("Main"));
        assert_eq!(agent.prompt_count, 2);
        assert_eq!(agent.entry_count, 4);
        assert_eq!(agent.first_prompt, "plan it");
        assert_eq!(agent.last_prompt, "now build it");
        assert_eq!(agent.first_ordinal, Some(1));
        assert_eq!(agent.last_ordinal, Some(2));
        assert_eq!(agent.summary.as_deref(), Some("Built it."));

        let rows: Vec<(String, Option<usize>, bool)> = store
            .filtered(&Filter {
                agent: Some("parent".to_string()),
                ..Filter::default()
            })
            .into_iter()
            .map(|i| {
                let e = store.wire(i);
                (e.prompt_text().unwrap().to_string(), e.ordinal, e.subagent)
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("review the build".to_string(), None, true),
                ("now build it".to_string(), Some(2), false),
                ("explore the code".to_string(), None, true),
                ("plan it".to_string(), Some(1), false),
            ]
        );
    }

    /// A loop in the parent links does not stop the chain id calculation.
    #[test]
    fn a_loop_in_the_parent_links_ends() {
        let mut store = Store::default();
        store.extend_parents([
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
        ]);
        assert!(["a", "b"].contains(&store.chain_id("a")));
    }

    #[test]
    fn a_prompt_that_arrives_again_with_a_response_replaces_the_old_entry() {
        let mut store = Store::from_entries(vec![prompt_entry("fix it", "s1", 5)]);
        let id = store.id_at(0).clone();

        let mut again = prompt_entry("fix it", "s1", 5);
        again.kind = EntryKind::Prompt {
            text: "fix it".to_string(),
            response: Some("Fixed.".to_string()),
        };
        store.merge(vec![again, nvim_entry("/dev/proj/a.rs", "/snap/1", 1)]);
        let changed = store.commit();

        let prompts: Vec<&HistoryEntry> = store
            .history
            .entries
            .iter()
            .filter(|e| e.is_prompt())
            .collect();
        assert_eq!(prompts.len(), 1, "the prompt is replaced, not duplicated");
        assert_eq!(prompts[0].response_text(), Some("Fixed."));
        assert_eq!(store.len(), 2);
        assert!(
            changed.contains(&id),
            "the replaced prompt has a new version"
        );
        assert_eq!(
            store.index_of(&id).map(|i| store.id_at(i)),
            Some(&id),
            "the id stays"
        );
    }

    /// A Cursor prompt has the composer that wrote it. The fuzzy join runs on
    /// Local History edits only, so it must not clear that composer.
    #[test]
    fn cursor_attribution_keeps_the_composer_of_a_prompt() {
        let mut prompt = prompt_entry("fix it", "composer-1", 5);
        prompt.source = EditSource::Cursor;
        prompt.file_path = PathBuf::from("cursor:composer-1:bubble-1");
        let mut edit = nvim_entry("/dev/proj/a.rs", "/snap/1", 1);
        edit.source = EditSource::Cursor;
        edit.session_id = Some("stale".to_string());

        let mut store = Store::from_entries(vec![prompt, edit]);
        store.apply_cursor_attribution(&CursorSessions::default());

        let prompt = store
            .history
            .entries
            .iter()
            .find(|e| e.is_prompt())
            .unwrap();
        assert_eq!(prompt.session_id.as_deref(), Some("composer-1"));
        let edit = store
            .history
            .entries
            .iter()
            .find(|e| !e.is_prompt())
            .unwrap();
        assert_eq!(
            edit.session_id, None,
            "an unattributed edit loses its composer"
        );
    }

    /// An older entry that arrives later changes the base of a newer entry of
    /// the same file. The newer entry must get a new version, so a client
    /// that asks for the changes gets the new base.
    #[test]
    fn a_new_base_gives_the_entry_a_new_version() {
        let mut store = Store::from_entries(vec![nvim_entry("/dev/proj/a.rs", "/snap/2", 1)]);
        let v1 = store.version();
        let newer = store.id_at(0).clone();

        store.merge(vec![nvim_entry("/dev/proj/a.rs", "/snap/1", 5)]);
        let changed = store.commit();

        assert!(store.version() > v1);
        assert!(
            changed.contains(&newer),
            "the base of the newer edit changed"
        );
        let wire = store.wire(store.index_of(&newer).unwrap());
        assert_eq!(wire.base_path, Some(PathBuf::from("/snap/1")));

        // A commit without a change keeps the version.
        let v2 = store.version();
        assert!(store.commit().is_empty());
        assert_eq!(store.version(), v2);
    }

    #[test]
    fn an_archived_session_gives_its_entries_a_new_version() {
        let mut store = Store::from_entries(vec![
            prompt_entry("one", "s1", 5),
            prompt_entry("two", "s2", 1),
        ]);
        let v1 = store.version();

        store.set_archived_sessions(HashSet::from(["s1".to_string()]));
        let changed = store.commit();

        assert!(store.version() > v1);
        assert_eq!(changed.len(), 1, "only the entry of s1 changed");
        let wire = store.wire(store.index_of(&changed[0]).unwrap());
        assert_eq!(wire.session_id.as_deref(), Some("s1"));
        assert!(wire.session_archived);
        let agents = store.agents(None);
        let archived: Vec<(&str, bool)> = agents
            .iter()
            .map(|a| (a.session_id.as_str(), a.archived))
            .collect();
        assert_eq!(archived, vec![("s2", false), ("s1", true)]);

        // A restore: the next load sends a set without s1.
        store.set_archived_sessions(HashSet::new());
        assert_eq!(store.commit(), changed);
    }

    #[test]
    fn since_returns_the_changed_entries_and_the_removed_ids() {
        let mut store = Store::from_entries(vec![prompt_entry("one", "s1", 5)]);
        let v1 = store.version();
        store.merge(vec![
            prompt_entry("two", "s2", 1),
            nvim_entry("/dev/proj/a.rs", "/snap/1", 2),
        ]);
        store.commit();

        let filter = Filter {
            kind: recap_api::KindFilter::Prompts,
            ..Filter::default()
        };
        let page = Page {
            since: Some(v1),
            limit: None,
        };
        let delta = store.history(&filter, &page);
        let texts: Vec<_> = delta
            .groups
            .iter()
            .flat_map(|g| g.entries.iter())
            .map(|e| e.prompt_text().unwrap().to_string())
            .collect();
        assert_eq!(texts, vec!["two"], "only the new prompt");
        assert_eq!(
            delta.removed.len(),
            1,
            "the new edit does not pass the filter"
        );

        let full = store.history(&filter, &Page::default());
        assert_eq!(full.groups.len(), 2);
        assert!(full.removed.is_empty());
    }

    #[test]
    fn cursor_edits_with_one_key_get_one_id_per_file_and_time() {
        let cursor_edit = |path: &str, minutes_ago: i64| {
            let mut e = nvim_entry(path, "/cache/cursor/bb", minutes_ago);
            e.base_path = Some(PathBuf::from("/cache/cursor/aa"));
            e.source = EditSource::Cursor;
            e
        };
        let first = cursor_edit("/dev/proj/a.rs", 3);
        let mut store = Store::default();
        // The Cursor data holds the first edit two times.
        store.merge(vec![
            first.clone(),
            first.clone(),
            cursor_edit("/dev/proj/a.rs", 2),
            cursor_edit("/dev/proj/b.rs", 2),
        ]);
        store.commit();
        assert_eq!(store.len(), 3, "the identical copy is not a new entry");
        let ids: HashSet<&EntryId> = (0..store.len()).map(|i| store.id_at(i)).collect();
        assert_eq!(ids.len(), 3, "each entry has its own id");
        for i in 0..store.len() {
            assert_eq!(store.index_of(store.id_at(i)), Some(i));
        }
        let keys: HashSet<String> = store.history.entries.iter().map(entry_key).collect();
        assert_eq!(keys.len(), 1, "the entries share one vector key");

        store.merge(vec![first]);
        assert!(
            store.commit().is_empty(),
            "a later identical copy changes nothing"
        );
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn the_key_is_relative_to_the_cache_root() {
        let root = crate::paths::cache_root();
        let mut entry = nvim_entry("/dev/proj/a.rs", "", 1);
        entry.snapshot_path = root.join("nvim/x/1");
        assert_eq!(entry_key(&entry), "nvim/x/1");
        entry.base_path = Some(root.join("cursor/aa"));
        entry.snapshot_path = root.join("cursor/bb");
        assert_eq!(entry_key(&entry), "cursor/aa::cursor/bb");
    }
}
