//! The wire types of the recap HTTP API.
//!
//! The server (`recap-server`) and every Rust client (`recap-client`, the
//! TUI) use these types, so a type change reaches both sides at compile time.
//! The crate does no I/O. The [`Api`] trait lists the operations. The server
//! implements it in-process, and `recap-client` implements it over HTTP.
//!
//! All routes start with [`routes::PREFIX`]. A list response carries the store
//! `version` that it describes (see [`HistoryResponse::version`]).

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

pub mod routes;

// ── Entries ──────────────────────────────────────────────────────────────────

/// The stable identity of one entry. The server calculates it from the
/// entry's content key, so it does not change when the store sorts the
/// entries again, and it is the same on the prod server and the test server.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(pub String);

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The program that made an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditSource {
    Cursor,
    ClaudeCode,
    Neovim,
}

/// What an entry records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryKind {
    /// A file edit. `file_path` is the real path of the file.
    FileChange,
    /// A user prompt to an agent. `file_path` is a synthetic unique key
    /// (for example `claude:<session>:<uuid>`), so two prompts never join
    /// one History group. `response` is the final text of the agent for this
    /// prompt, after its last tool use. It is `None` when the turn has no
    /// final text: an interrupted turn, a queued prompt, or a session that
    /// still runs.
    Prompt {
        text: String,
        response: Option<String>,
    },
    /// The agent replaced the earlier messages of the session with a
    /// summary, because the context window was full or the user asked for
    /// it. `file_path` is a synthetic unique key, as for a prompt. `summary`
    /// is the text that the agent received in place of the earlier messages.
    Compaction { summary: String },
}

/// One entry of the history, as the server sends it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    /// The store version of the merge that added or replaced this entry.
    /// `GET /v1/history?since=N` returns the entries whose version is
    /// larger than N.
    pub version: u64,
    pub timestamp: DateTime<Local>,
    /// The edited file, or the synthetic key of a prompt.
    pub file_path: PathBuf,
    /// The projects that contain the file, outermost first.
    pub project_roots: Vec<PathBuf>,
    pub source: EditSource,
    pub kind: EntryKind,
    /// The agent session that made the entry, when the source has one.
    pub session_id: Option<String>,
    /// The first session of the session chain that holds `session_id`. A
    /// compaction can continue a Claude Code session in a new session, and
    /// the new session names the old one. The server follows these links
    /// back to the first session. A subagent session also joins the chain
    /// of the session that started it (see [`Entry::subagent`]). The Agents
    /// panel shows one agent for each session chain. Equal to `session_id`
    /// for a session without links.
    #[serde(default)]
    pub chain_id: Option<String>,
    /// The title of that session, when the source records one.
    pub session_title: Option<String>,
    /// The newest summary of that session that Claude Code shows while the
    /// session is idle. `None` for the other sources.
    #[serde(default)]
    pub session_summary: Option<String>,
    /// The working directory of that session, when the source records it.
    pub session_cwd: Option<PathBuf>,
    /// True when the agent deleted the files of the session, and the server
    /// read the entry from its archive copy. Only a Claude Code session can
    /// be archived. `POST /v1/sessions/{id}/restore` writes the files back.
    #[serde(default)]
    pub session_archived: bool,
    /// True when another agent started the session as a subagent. The
    /// parent agent wrote the prompts of a subagent session, not the user.
    /// The session joins the session chain of the parent.
    #[serde(default)]
    pub subagent: bool,
    /// The position of a prompt in its own session chain, counted from the
    /// oldest prompt as 1. `None` for a file change, for a subagent prompt
    /// and for a prompt without a session.
    pub ordinal: Option<usize>,
    /// The file on the server that holds the content after a file change.
    /// `None` for a prompt.
    pub snapshot_path: Option<PathBuf>,
    /// The file on the server that holds the content before a file change.
    /// `None` for a prompt and for the first snapshot of a file.
    pub base_path: Option<PathBuf>,
}

/// The fields that the shared entry helpers read. The server's own entry
/// type and [`Entry`] both implement it, so the display rules exist once.
pub trait EntryFields {
    fn kind(&self) -> &EntryKind;
    fn file_path(&self) -> &Path;
    fn project_roots(&self) -> &[PathBuf];

    fn is_prompt(&self) -> bool {
        matches!(self.kind(), EntryKind::Prompt { .. })
    }

    fn is_compaction(&self) -> bool {
        matches!(self.kind(), EntryKind::Compaction { .. })
    }

    fn is_file_change(&self) -> bool {
        matches!(self.kind(), EntryKind::FileChange)
    }

    fn prompt_text(&self) -> Option<&str> {
        match self.kind() {
            EntryKind::Prompt { text, .. } => Some(text),
            EntryKind::FileChange | EntryKind::Compaction { .. } => None,
        }
    }

    /// The summary of a compaction. `None` for the other kinds.
    fn compaction_summary(&self) -> Option<&str> {
        match self.kind() {
            EntryKind::Compaction { summary } => Some(summary),
            EntryKind::FileChange | EntryKind::Prompt { .. } => None,
        }
    }

    /// The final response of the agent to a prompt. `None` for a file change
    /// and for a prompt whose turn has no final text.
    fn response_text(&self) -> Option<&str> {
        match self.kind() {
            EntryKind::Prompt { response, .. } => response.as_deref(),
            EntryKind::FileChange | EntryKind::Compaction { .. } => None,
        }
    }

    fn file_name(&self) -> &str {
        self.file_path()
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
    }

    /// The innermost project root, if any.
    fn project_root(&self) -> Option<&Path> {
        self.project_roots().last().map(|p| p.as_path())
    }

    /// The path as "outer → inner → relative/path/in/inner". Without a
    /// project, the last components of the path. `names` gives the display
    /// name of each project root.
    fn display_path(&self, names: &HashMap<PathBuf, String>) -> String {
        const ARROW: &str = " → ";
        let (chain, rel) = self.display_path_segments(names);
        // The tail can have many lines (prompt text), so make it one line.
        let rel = single_line(&rel);
        if chain.is_empty() {
            rel
        } else {
            format!("{}{}{}", chain.join(ARROW), ARROW, rel)
        }
    }

    /// Split [`Self::display_path`] into (project chain names, tail). For a
    /// file change the tail is the path relative to the innermost project.
    /// For a prompt the tail is the prompt text on one line. For a
    /// compaction the tail is [`COMPACTION_LABEL`]. Without a
    /// project, the names list is empty and the tail is a short fallback.
    fn display_path_segments(&self, names: &HashMap<PathBuf, String>) -> (Vec<String>, String) {
        let chain: Vec<String> = self
            .project_roots()
            .iter()
            .map(|p| project_name(p, names))
            .collect();
        match self.kind() {
            EntryKind::Prompt { text, .. } => return (chain, single_line(text)),
            EntryKind::Compaction { .. } => return (chain, COMPACTION_LABEL.to_string()),
            EntryKind::FileChange => {}
        }
        let Some(deepest) = self.project_root() else {
            return (Vec::new(), short_path(self.file_path(), 3));
        };
        let rel = self
            .file_path()
            .strip_prefix(deepest)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| self.file_path().to_string_lossy().into_owned());
        (chain, rel)
    }
}

/// The text that a list row shows for a compaction.
pub const COMPACTION_LABEL: &str = "── context compacted ──";

impl EntryFields for Entry {
    fn kind(&self) -> &EntryKind {
        &self.kind
    }
    fn file_path(&self) -> &Path {
        &self.file_path
    }
    fn project_roots(&self) -> &[PathBuf] {
        &self.project_roots
    }
}

/// The order of the history: the newest entry first. Two entries with the
/// same time sort by id, so the server and a client that merges changes get
/// the same order.
pub fn newest_first(a: (&DateTime<Local>, &EntryId), b: (&DateTime<Local>, &EntryId)) -> Ordering {
    b.0.cmp(a.0).then_with(|| a.1.cmp(b.1))
}

/// Split a list of entries into groups: each group is a run of consecutive
/// entries of one file. A prompt has a unique `file_path`, so each prompt is
/// a group of its own.
pub fn group_runs<E: EntryFields>(entries: &[E]) -> Vec<Range<usize>> {
    let mut runs: Vec<Range<usize>> = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(last) = runs.last_mut()
            && entries[last.start].file_path() == entry.file_path()
        {
            last.end = i + 1;
            continue;
        }
        runs.push(i..i + 1);
    }
    runs
}

/// The regex of a content search: the query as a case-insensitive regex.
/// The server matches entries with it, and a client highlights the matched
/// text with it.
pub fn content_regex(query: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(query).case_insensitive(true).build()
}

/// The display name of a project root: the name that `names` holds, else the
/// last component of the path.
pub fn project_name(root: &Path, names: &HashMap<PathBuf, String>) -> String {
    match names.get(root) {
        Some(s) => s.clone(),
        None => root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string(),
    }
}

/// Replace each newline and each run of whitespace with one space, so a
/// prompt with many lines fits in one row.
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

// ── Diffs ────────────────────────────────────────────────────────────────────

/// One line of a diff. On the wire: `{"type": "added", "text": "…"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "text", rename_all = "snake_case")]
pub enum DiffLine {
    Added(String),
    Removed(String),
    Context(String),
    Header(String),
    /// One source line of a prompt.
    Prompt(String),
    /// One source line of the final response of the agent to a prompt.
    Response(String),
    /// The first line of one entry, when a diff joins the diffs of several
    /// entries (`GET /v1/diff?ids=a,b`). The text is the id of that entry.
    ChildSeparator(EntryId),
}

impl DiffLine {
    /// The text that a text search reads: the body of an added, removed,
    /// prompt or response line. The other lines hold no searchable text.
    pub fn search_text(&self) -> Option<&str> {
        match self {
            DiffLine::Added(s) | DiffLine::Removed(s) | DiffLine::Prompt(s) | DiffLine::Response(s) => {
                Some(s)
            }
            DiffLine::Context(_) | DiffLine::Header(_) | DiffLine::ChildSeparator(_) => None,
        }
    }
}

/// The number of added and removed lines of one file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiffSummary {
    pub added: usize,
    pub removed: usize,
}

// ── Views ────────────────────────────────────────────────────────────────────

/// Which entry kinds a view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KindFilter {
    #[default]
    All,
    Prompts,
    Changes,
}

impl KindFilter {
    /// The next value in the cycle All → Prompts → Changes → All.
    pub fn next(self) -> KindFilter {
        match self {
            KindFilter::All => KindFilter::Prompts,
            KindFilter::Prompts => KindFilter::Changes,
            KindFilter::Changes => KindFilter::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            KindFilter::All => "all",
            KindFilter::Prompts => "prompts",
            KindFilter::Changes => "changes",
        }
    }

    /// Whether one entry passes the filter. The prompts view also shows
    /// the compactions, because a compaction marks a point in the
    /// conversation, not a file change.
    pub fn keeps(self, entry: &impl EntryFields) -> bool {
        match self {
            KindFilter::All => true,
            KindFilter::Prompts => !entry.is_file_change(),
            KindFilter::Changes => entry.is_file_change(),
        }
    }
}

/// The filter of a history view or a search. On the wire it is a set of
/// query parameters. An absent field does not filter.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Filter {
    /// Keep the entries inside this project root.
    pub project: Option<PathBuf>,
    /// Keep the entries of this file.
    pub file: Option<PathBuf>,
    /// Keep the entries of the session chain with this id (see
    /// [`Entry::chain_id`]). An empty string keeps no entry: an agent view
    /// without a selected agent is empty.
    pub agent: Option<String>,
    pub kind: KindFilter,
    /// Keep the entries newer than this time.
    pub after: Option<DateTime<Local>>,
}

impl Filter {
    /// The query parameters of the filter. The server reads them back with
    /// serde, so an absent field gives the default value.
    pub fn query_pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        if let Some(p) = &self.project {
            pairs.push(("project", p.to_string_lossy().into_owned()));
        }
        if let Some(f) = &self.file {
            pairs.push(("file", f.to_string_lossy().into_owned()));
        }
        if let Some(a) = &self.agent {
            pairs.push(("agent", a.clone()));
        }
        if self.kind != KindFilter::All {
            pairs.push(("kind", self.kind.label().to_string()));
        }
        if let Some(after) = &self.after {
            pairs.push(("after", after.to_rfc3339()));
        }
        pairs
    }
}

/// The part of a history view to send.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Page {
    /// Send only the entries whose `version` is larger than this value.
    pub since: Option<u64>,
    /// Send at most this number of entries (the newest ones).
    pub limit: Option<usize>,
}

impl Page {
    /// The query parameters of the page.
    pub fn query_pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        if let Some(since) = self.since {
            pairs.push(("since", since.to_string()));
        }
        if let Some(limit) = self.limit {
            pairs.push(("limit", limit.to_string()));
        }
        pairs
    }
}

/// Join entry ids into the value of an `ids` query parameter.
pub fn join_ids(ids: &[EntryId]) -> String {
    ids.iter().map(|id| id.0.as_str()).collect::<Vec<_>>().join(",")
}

/// Split the value of an `ids` query parameter into entry ids.
pub fn split_ids(value: &str) -> Vec<EntryId> {
    value
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| EntryId(s.to_string()))
        .collect()
}

/// One project, as the Projects panel lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub root: PathBuf,
    /// The display name. A git worktree shows as `main-repo (worktree)`.
    pub name: String,
    /// The project roots from the outermost project down to this one.
    pub chain: Vec<PathBuf>,
    /// The time of the newest entry inside the project.
    pub latest: DateTime<Local>,
    /// The number of entries inside the project.
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectsResponse {
    pub version: u64,
    /// Newest first.
    pub projects: Vec<Project>,
}

/// One edited file, as the Files panel lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: PathBuf,
    pub latest: DateTime<Local>,
    pub count: usize,
    /// The project chain of the file, outermost first.
    pub chain: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilesResponse {
    pub version: u64,
    /// The version of the list that the change in this response applies to
    /// (see [`ListItem`]). `None` when `files` is the full list.
    #[serde(default)]
    pub since: Option<u64>,
    /// Newest first. With `since`, only the new files and the files that
    /// changed after that version.
    pub files: Vec<FileInfo>,
    /// With `since`, the files that left the list after that version.
    #[serde(default)]
    pub removed: Vec<PathBuf>,
}

impl ListItem for FileInfo {
    type Key = PathBuf;

    fn key(&self) -> &PathBuf {
        &self.path
    }

    fn order(a: &Self, b: &Self) -> Ordering {
        b.latest.cmp(&a.latest).then_with(|| a.path.cmp(&b.path))
    }
}

/// One session chain, as the Agents panel lists it (see
/// [`Entry::chain_id`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    /// The id of the session chain. [`Filter::agent`] takes this id.
    pub chain_id: String,
    /// The session of the newest entry of the chain. The run state and
    /// `claude --resume` use this session.
    pub session_id: String,
    pub source: EditSource,
    /// The title of the newest session of the chain that has a title.
    pub title: Option<String>,
    /// True when the agent deleted the files of `session_id` (see
    /// [`Entry::session_archived`]).
    #[serde(default)]
    pub archived: bool,
    /// The time of the newest prompt of the chain, or of its newest entry
    /// when the chain has no prompt.
    pub last_ts: DateTime<Local>,
    /// The time of the oldest entry of the chain. The Queue panel sorts the
    /// agents by this time.
    #[serde(default)]
    pub first_ts: Option<DateTime<Local>>,
    pub prompt_count: usize,
    /// The number of entries of the chain: prompts, file changes and
    /// compactions, subagent entries included.
    #[serde(default)]
    pub entry_count: usize,
    /// The project chain of the newest entry of the agent.
    pub project_roots: Vec<PathBuf>,
    /// The oldest prompt on one line. Empty when the agent has no prompt.
    #[serde(default)]
    pub first_prompt: String,
    /// The [`Entry::ordinal`] of the oldest prompt. It is 1, unless the
    /// project filter hides the older prompts of the chain.
    #[serde(default)]
    pub first_ordinal: Option<usize>,
    /// The newest prompt on one line. Empty when the agent has no prompt.
    pub last_prompt: String,
    /// The [`Entry::ordinal`] of the newest prompt.
    #[serde(default)]
    pub last_ordinal: Option<usize>,
    /// The newest summary of `session_id` (see [`Entry::session_summary`]),
    /// on one line.
    #[serde(default)]
    pub summary: Option<String>,
    /// The newest final response of the agent to a prompt, on one line.
    #[serde(default)]
    pub last_response: Option<String>,
    // The server sends only the start of `first_prompt`, `last_prompt`,
    // `summary` and `last_response`, because the Agents panel shows one line
    // of each text.
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentsResponse {
    pub version: u64,
    /// The version of the list that the change in this response applies to
    /// (see [`ListItem`]). `None` when `agents` is the full list.
    #[serde(default)]
    pub since: Option<u64>,
    /// Newest first. With `since`, only the new agents and the agents that
    /// changed after that version.
    pub agents: Vec<Agent>,
    /// With `since`, the chain ids of the agents that left the list after
    /// that version.
    #[serde(default)]
    pub removed: Vec<String>,
}

impl ListItem for Agent {
    type Key = String;

    fn key(&self) -> &String {
        &self.chain_id
    }

    fn order(a: &Self, b: &Self) -> Ordering {
        b.last_ts.cmp(&a.last_ts).then_with(|| a.chain_id.cmp(&b.chain_id))
    }
}

/// An item of a side list (the Files list and the Agents list). The client
/// sends the version of the list that it holds. When the server still knows
/// the list at that version, it sends only the new and the changed items and
/// the keys of the removed items. The client merges them with
/// [`apply_list_change`].
pub trait ListItem: Clone + PartialEq {
    type Key: Clone + Eq + std::hash::Hash;

    /// The identity of the item in its list.
    fn key(&self) -> &Self::Key;

    /// The order of the list: newest first. Two different items never
    /// compare equal, so the server and the client get the same order.
    fn order(a: &Self, b: &Self) -> Ordering;
}

/// The items of `new` that are not in `old` or that are different in `old`,
/// and the keys of the items of `old` that are not in `new`.
pub fn list_change<T: ListItem>(old: &[T], new: &[T]) -> (Vec<T>, Vec<T::Key>) {
    let old_by_key: HashMap<&T::Key, &T> = old.iter().map(|item| (item.key(), item)).collect();
    let new_keys: HashSet<&T::Key> = new.iter().map(ListItem::key).collect();
    let changed = new
        .iter()
        .filter(|item| old_by_key.get(item.key()) != Some(item))
        .cloned()
        .collect();
    let removed = old
        .iter()
        .map(ListItem::key)
        .filter(|key| !new_keys.contains(key))
        .cloned()
        .collect();
    (changed, removed)
}

/// Merge the result of [`list_change`] into `list`, and sort `list` again.
pub fn apply_list_change<T: ListItem>(list: &mut Vec<T>, changed: Vec<T>, removed: &[T::Key]) {
    let mut changed: HashMap<T::Key, T> = changed
        .into_iter()
        .map(|item| (item.key().clone(), item))
        .collect();
    let removed: HashSet<&T::Key> = removed.iter().collect();
    list.retain_mut(|item| {
        if removed.contains(item.key()) {
            return false;
        }
        if let Some(new) = changed.remove(item.key()) {
            *item = new;
        }
        true
    });
    list.extend(changed.into_values());
    list.sort_by(T::order);
}

/// A run of consecutive entries of one file, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryGroup {
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HistoryResponse {
    /// The store version that this response describes. Send it back as
    /// `since` to get only the entries that changed after it.
    pub version: u64,
    /// The entries that pass the filter, newest first, in groups (see
    /// [`group_runs`]). With `since`, only the entries that changed after
    /// that version.
    pub groups: Vec<HistoryGroup>,
    /// With `since` only: the entries that changed after that version and do
    /// not pass the filter now. A client that holds one of them must remove
    /// it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<EntryId>,
}

// ── Search ───────────────────────────────────────────────────────────────────

/// The corpus that a search reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    /// A fuzzy match against the display path (the project chain for a
    /// prompt).
    Path,
    /// A case-insensitive regex against the diff body of a file change and
    /// the text and response of a prompt.
    Content,
    /// The entries ranked by the cosine similarity of their embedding to the
    /// embedded query.
    Semantic,
}

impl SearchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchKind::Path => "path",
            SearchKind::Content => "content",
            SearchKind::Semantic => "semantic",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    pub id: EntryId,
    /// For a path search: the character positions in the display path that
    /// the query matched. Empty for the other kinds.
    pub indices: Vec<u32>,
    /// Larger is better. A semantic score is the cosine similarity times
    /// [`SEMANTIC_SCORE_SCALE`].
    pub score: u16,
}

/// The scale that packs a cosine similarity into [`SearchMatch::score`].
pub const SEMANTIC_SCORE_SCALE: f32 = 10_000.0;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub version: u64,
    /// Best match first for a path or a semantic search. Newest first for a
    /// content search.
    pub matches: Vec<SearchMatch>,
}

// ── Status and events ────────────────────────────────────────────────────────

/// What the semantic search subsystem does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EmbedStatus {
    /// The server has not started the model yet. `POST /v1/embed/start` or
    /// the first semantic search starts it.
    Off,
    /// The model server starts and loads the model.
    Starting,
    /// The indexer embeds the entries that the vector store does not hold.
    Indexing { done: usize, total: usize },
    /// Every entry has a vector.
    Ready,
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub version: u64,
    /// True while a load of the sources runs.
    pub loading: bool,
    pub entries: usize,
    pub embed: EmbedStatus,
    /// The number of running Claude Code sessions.
    pub running: usize,
    /// The cache directory of this server.
    pub cache_dir: PathBuf,
    /// The state of the triage runner. `None` when the server runs without
    /// `--triage-dir`.
    #[serde(default)]
    pub triage: Option<TriageStatus>,
}

/// The state of the triage runner of the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriageStatus {
    /// The directory of the text files of the triage agent.
    pub dir: PathBuf,
    /// The number of stops that wait for the next run.
    pub pending: usize,
    /// True when a review of the agents of the last 24 hours waits for the
    /// next run.
    #[serde(default)]
    pub review: bool,
    /// The time of the next scheduled run, from the `next-run` file.
    pub next_run: Option<DateTime<Local>>,
    /// The active run, or the last run when no run is active. None before
    /// the first run after the start of the server.
    #[serde(default)]
    pub run: Option<TriageRun>,
}

impl TriageStatus {
    /// True while a triage run is active.
    pub fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.ended.is_none())
    }
}

/// One run of the triage agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriageRun {
    /// The Claude Code session of the run. The transcript of the run is the
    /// agent with this chain id.
    pub session_id: String,
    pub started: DateTime<Local>,
    /// The number of stops that started the run.
    pub stops: usize,
    /// True for a review of the agents of the last 24 hours.
    pub review: bool,
    /// None while the run is active.
    pub ended: Option<DateTime<Local>>,
    /// True when the process ended with success. None while the run is
    /// active.
    pub success: Option<bool>,
    /// The newest action of the run on one line, for example
    /// `Read agents/4f2a.md`.
    #[serde(default)]
    pub activity: Option<String>,
}

/// A running Claude Code session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningSession {
    pub session_id: String,
    pub pid: i32,
    pub cwd: Option<PathBuf>,
    /// The status that Claude Code reports (`idle`, `busy`, `waiting`).
    pub status: Option<String>,
    /// What the session waits for when the status is `waiting`, for
    /// example `permission prompt`.
    #[serde(default)]
    pub waiting_for: Option<String>,
    /// The time of the last change of `status`.
    #[serde(default)]
    pub status_since: Option<DateTime<Local>>,
}

/// A change notice on `GET /v1/events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The store changed. Fetch the views again, or fetch
    /// `GET /v1/history?since=<old version>` for the changed entries only.
    Changed { version: u64 },
    /// A load of the sources started or ended.
    Loading { loading: bool },
    /// The semantic search subsystem changed its state.
    Embed(EmbedStatus),
    /// The set of running Claude Code sessions changed.
    Running { sessions: Vec<RunningSession> },
    /// The triage runner changed its state.
    Triage(TriageStatus),
}

// ── Blame ────────────────────────────────────────────────────────────────────

/// The body of `POST /v1/blame`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameRequest {
    /// A unified diff, as `git diff` writes it.
    pub diff: String,
    /// The root of the repository that the diff paths are relative to.
    pub repo_root: PathBuf,
}

/// One session-and-prompt group that made lines of a hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    pub source: EditSource,
    /// The Claude session or the Cursor composer. `None` for a manual edit.
    pub session_id: Option<String>,
    /// The Cursor composer name. Claude has no session name.
    pub session_name: Option<String>,
    /// The synthetic key of the prompt entry, the `file_path` of the prompt.
    pub prompt_key: Option<String>,
    pub prompt_time: Option<String>,
    pub prompt: Option<String>,
    pub response: Option<String>,
    /// The newest edit of this group, RFC 3339.
    pub edit_time: String,
    pub matched_lines: usize,
    /// The part of `matched_lines` that only a Bash command of the session
    /// holds: the agent wrote the file with a shell command, so recap has no
    /// snapshot of it. This is weaker evidence than a snapshot line.
    pub bash_lines: usize,
}

/// The attribution of one hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HunkBlame {
    pub file: String,
    pub hunk_index: usize,
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    /// The number of `+` and `-` lines that could vote.
    pub voting_lines: usize,
    /// The group with the most matched lines, when it explains at least a
    /// quarter of the voting lines. `None` otherwise; the groups are then
    /// all in `others`.
    pub winner: Option<Attribution>,
    /// The other groups, sorted by matched lines.
    pub others: Vec<Attribution>,
    /// The file header plus this hunk, for `git apply --cached`.
    pub patch: String,
}

// ── The operations ───────────────────────────────────────────────────────────

/// The error of an [`Api`] call: a message for a person to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError(pub String);

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ApiError {}

pub type ApiResult<T> = Result<T, ApiError>;

/// The operations of the recap server. The server implements this trait
/// in-process, and `recap-client` implements it over HTTP. Each method
/// matches one route in [`routes`].
pub trait Api: Send + Sync {
    fn status(&self) -> ApiResult<Status>;
    fn projects(&self) -> ApiResult<ProjectsResponse>;
    /// The files inside `project`, or all files. `since` is the version of
    /// the list that the caller holds (see [`ListItem`]).
    fn files(&self, project: Option<&Path>, since: Option<u64>) -> ApiResult<FilesResponse>;
    /// The agents inside `project`, or all agents. `since` is the version of
    /// the list that the caller holds (see [`ListItem`]).
    fn agents(&self, project: Option<&Path>, since: Option<u64>) -> ApiResult<AgentsResponse>;
    fn history(&self, filter: &Filter, page: &Page) -> ApiResult<HistoryResponse>;
    fn entry(&self, id: &EntryId) -> ApiResult<Entry>;
    /// The diff of one entry, or the diffs of several entries joined with a
    /// [`DiffLine::ChildSeparator`] in front of each one.
    fn diff(&self, ids: &[EntryId]) -> ApiResult<Vec<DiffLine>>;
    /// The line counts of the diffs of these entries. An unknown id is
    /// absent from the map.
    fn summaries(&self, ids: &[EntryId]) -> ApiResult<HashMap<EntryId, DiffSummary>>;
    fn search(&self, kind: SearchKind, query: &str, filter: &Filter) -> ApiResult<SearchResponse>;
    fn running(&self) -> ApiResult<Vec<RunningSession>>;
    /// Start a load of the sources now, if none runs.
    fn reload(&self) -> ApiResult<()>;
    /// Start the model server and the indexer, if they do not run.
    fn start_embedding(&self) -> ApiResult<()>;
    /// Ask the triage runner for a review of the agents of the last 24
    /// hours. The review starts when no run is active.
    fn triage_review(&self) -> ApiResult<()>;
    fn blame(&self, request: &BlameRequest) -> ApiResult<Vec<HunkBlame>>;
    /// Write the files of an archived session back where the agent reads
    /// them, so the agent can resume the session.
    fn restore_session(&self, session_id: &str) -> ApiResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, minutes_ago: i64, count: usize) -> FileInfo {
        FileInfo {
            path: PathBuf::from(path),
            latest: Local::now() - chrono::Duration::minutes(minutes_ago),
            count,
            chain: vec![],
        }
    }

    #[test]
    fn a_list_change_gives_the_new_list_from_the_old_list() {
        let old = vec![file("/a", 3, 1), file("/b", 2, 1), file("/c", 1, 1)];
        let mut new = vec![file("/d", 0, 1), file("/a", 3, 2), file("/c", 1, 1)];
        new[2].latest = old[2].latest;
        new.sort_by(FileInfo::order);

        let (changed, removed) = list_change(&old, &new);
        let mut paths: Vec<_> = changed.iter().map(|f| f.path.clone()).collect();
        paths.sort();
        assert_eq!(paths, vec![PathBuf::from("/a"), PathBuf::from("/d")]);
        assert_eq!(removed, vec![PathBuf::from("/b")]);

        let mut list = old.clone();
        apply_list_change(&mut list, changed, &removed);
        assert_eq!(list, new);
    }

    fn sample_entry() -> Entry {
        Entry {
            id: EntryId("0123456789abcdef".to_string()),
            version: 3,
            timestamp: Local::now(),
            file_path: PathBuf::from("claude:s1:u1"),
            project_roots: vec![PathBuf::from("/dev/proj")],
            source: EditSource::ClaudeCode,
            kind: EntryKind::Prompt {
                text: "fix the bug".to_string(),
                response: Some("Fixed.".to_string()),
            },
            session_id: Some("s1".to_string()),
            chain_id: Some("s1".to_string()),
            session_title: Some("Bug fix".to_string()),
            session_summary: Some("I fixed the bug.".to_string()),
            session_cwd: Some(PathBuf::from("/dev/proj")),
            session_archived: false,
            subagent: false,
            ordinal: Some(1),
            snapshot_path: None,
            base_path: None,
        }
    }

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug>(value: &T) {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value, "the JSON was {json}");
    }

    #[test]
    fn every_wire_type_survives_a_json_round_trip() {
        let entry = sample_entry();
        round_trip(&entry);
        round_trip(&HistoryResponse {
            version: 3,
            groups: vec![HistoryGroup { entries: vec![entry.clone()] }],
            removed: vec![EntryId("fedcba9876543210".to_string())],
        });
        round_trip(&ProjectsResponse {
            version: 3,
            projects: vec![Project {
                root: PathBuf::from("/dev/proj"),
                name: "proj".to_string(),
                chain: vec![PathBuf::from("/dev/proj")],
                latest: entry.timestamp,
                count: 1,
            }],
        });
        round_trip(&FilesResponse {
            version: 3,
            since: Some(2),
            files: vec![FileInfo {
                path: PathBuf::from("/dev/proj/a.rs"),
                latest: entry.timestamp,
                count: 2,
                chain: vec![PathBuf::from("/dev/proj")],
            }],
            removed: vec![PathBuf::from("/dev/proj/b.rs")],
        });
        round_trip(&AgentsResponse {
            version: 3,
            since: None,
            removed: vec![],
            agents: vec![Agent {
                chain_id: "s0".to_string(),
                session_id: "s1".to_string(),
                source: EditSource::ClaudeCode,
                title: None,
                archived: false,
                last_ts: entry.timestamp,
                first_ts: Some(entry.timestamp),
                prompt_count: 1,
                entry_count: 3,
                project_roots: vec![],
                first_prompt: "plan the fix".to_string(),
                first_ordinal: Some(1),
                last_prompt: "fix the bug".to_string(),
                last_ordinal: Some(2),
                summary: Some("I fixed the bug.".to_string()),
                last_response: Some("Done.".to_string()),
            }],
        });
        round_trip(&vec![
            DiffLine::Header("@@ -1,1 +1,1 @@".to_string()),
            DiffLine::Added("a".to_string()),
            DiffLine::ChildSeparator(entry.id.clone()),
        ]);
        round_trip(&SearchResponse {
            version: 3,
            matches: vec![SearchMatch { id: entry.id.clone(), indices: vec![1, 2], score: 7 }],
        });
        round_trip(&Status {
            version: 3,
            loading: false,
            entries: 1,
            embed: EmbedStatus::Indexing { done: 1, total: 2 },
            running: 0,
            cache_dir: PathBuf::from("/tmp/c"),
            triage: Some(TriageStatus {
                dir: PathBuf::from("/tmp/t"),
                pending: 2,
                review: true,
                next_run: Some(entry.timestamp),
                run: Some(TriageRun {
                    session_id: "s1".to_string(),
                    started: entry.timestamp,
                    stops: 1,
                    review: false,
                    ended: None,
                    success: None,
                    activity: Some("Read memory.md".to_string()),
                }),
            }),
        });
        round_trip(&Filter {
            project: Some(PathBuf::from("/dev/proj")),
            kind: KindFilter::Prompts,
            ..Filter::default()
        });
        for event in [
            Event::Changed { version: 4 },
            Event::Loading { loading: true },
            Event::Embed(EmbedStatus::Failed { error: "no model".to_string() }),
            Event::Embed(EmbedStatus::Off),
            Event::Running { sessions: vec![] },
        ] {
            round_trip(&event);
        }
    }

    /// The field names are the contract with `curl` and `jq` scripts.
    #[test]
    fn the_json_shape_is_easy_to_read_with_jq() {
        let event = serde_json::to_value(Event::Embed(EmbedStatus::Indexing { done: 1, total: 9 })).unwrap();
        assert_eq!(event["type"], "embed");
        assert_eq!(event["state"], "indexing");
        assert_eq!(event["done"], 1);

        let entry = serde_json::to_value(sample_entry()).unwrap();
        assert_eq!(entry["kind"]["type"], "prompt");
        assert_eq!(entry["kind"]["text"], "fix the bug");
        assert_eq!(entry["source"], "claude_code");

        let line = serde_json::to_value(DiffLine::Added("x".to_string())).unwrap();
        assert_eq!(line, serde_json::json!({"type": "added", "text": "x"}));
    }

    #[test]
    fn a_group_is_a_run_of_one_file() {
        let mut entries = Vec::new();
        for path in ["/p/a.rs", "/p/a.rs", "/p/b.rs", "/p/a.rs"] {
            let mut e = sample_entry();
            e.kind = EntryKind::FileChange;
            e.file_path = PathBuf::from(path);
            entries.push(e);
        }
        assert_eq!(group_runs(&entries), vec![0..2, 2..3, 3..4]);
    }

    #[test]
    fn display_path_shows_the_project_chain() {
        let mut entry = sample_entry();
        entry.kind = EntryKind::FileChange;
        entry.file_path = PathBuf::from("/dev/proj/src/main.rs");
        entry.project_roots = vec![PathBuf::from("/dev"), PathBuf::from("/dev/proj")];
        let names = HashMap::from([(PathBuf::from("/dev/proj"), "proj (worktree)".to_string())]);
        assert_eq!(entry.display_path(&names), "dev → proj (worktree) → src/main.rs");
    }
}
