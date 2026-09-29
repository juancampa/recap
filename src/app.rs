use crate::cursor_workspace;
use crate::diff::{build_diff, compute_summary, DiffLine, DiffSummary};
use crate::embed::{cosine, EmbedClient, EmbedConfig, EmbedDb, EmbedIndex, EmbedKind, EmbedServer};
use crate::history::{find_project_chain, EditSource, HistoryEntry, HistoryEntryKind, HistoryStore};
use crate::search_history::{SearchHistories, SearchMode};
use crate::{claude, nvim};
use chrono::{DateTime, Local, NaiveDate};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use regex::{Regex, RegexBuilder};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

/// External entry sources we can rescan on demand. Holds per-source mtime
/// caches and per-source dedup keys (snapshot_path for FileChange,
/// synthetic file_path for Prompt) so a reload only emits truly-new
/// entries. Each source has its own seen-set because the three sources
/// produce disjoint synthetic paths today, and split sets let the
/// background loader threads each own their state without locks.
#[derive(Default)]
pub struct Sources {
    pub cursor_dir: Option<PathBuf>,
    pub claude_dir: Option<PathBuf>,
    pub nvim_dir: Option<PathBuf>,
    pub cursor_mtimes: HashMap<PathBuf, SystemTime>,
    pub claude_mtimes: HashMap<PathBuf, SystemTime>,
    pub nvim_mtimes: HashMap<PathBuf, SystemTime>,
    pub cursor_seen: HashSet<PathBuf>,
    pub claude_seen: HashSet<PathBuf>,
    pub nvim_seen: HashSet<PathBuf>,
    /// `~/.cursor/plans/X.plan.md` → workspace folder it was opened in.
    /// Computed once at detect time from Cursor's workspaceStorage.
    pub plan_project_map: HashMap<PathBuf, PathBuf>,
    /// Cached `find_project_chain` results for resolved plan workspaces, so
    /// we don't re-walk filesystem ancestors per plan entry.
    plan_chain_cache: HashMap<PathBuf, Vec<PathBuf>>,
    /// Cursor composer-attribution map + the DB mtime it was built from, so a
    /// reload rebuilds it (a multi-second, multi-GB scan) only when Cursor's
    /// store actually changed. See [`crate::cursor_sessions`].
    cursor_sessions: Option<std::sync::Arc<crate::cursor_sessions::CursorSessions>>,
    cursor_sessions_mtime: Option<SystemTime>,
}

impl Sources {
    /// Pull every source's directory once (Cursor/VSCode history, Claude
    /// projects dir, neovim undodir). Missing sources stay `None`.
    pub fn detect() -> Self {
        Self {
            cursor_dir: cursor_history_dir(),
            claude_dir: claude::projects_dir(),
            nvim_dir: nvim::undodir(),
            plan_project_map: cursor_workspace::build_plan_project_map(),
            ..Self::default()
        }
    }

    /// `find_project_chain` walks up from a file looking for project
    /// markers. For `~/.cursor/plans/<x>.plan.md` that walk yields nothing
    /// (the plan dir has no marker). Override the chain using the workspace
    /// storage map: plans we recognize get their workspace's chain; the
    /// rest fall back to a synthetic `cursor plans` project so they don't
    /// disappear into "no project".
    fn attach_plan_project(&mut self, entries: &mut [HistoryEntry]) {
        for entry in entries.iter_mut() {
            if !cursor_workspace::is_plan_file(&entry.file_path) {
                continue;
            }
            if let Some(workspace) = self.plan_project_map.get(&entry.file_path).cloned() {
                let chain = self
                    .plan_chain_cache
                    .entry(workspace.clone())
                    .or_insert_with(|| chain_for_dir(&workspace))
                    .clone();
                if !chain.is_empty() {
                    entry.project_roots = chain;
                    continue;
                }
            }
            if let Some(root) = cursor_workspace::cursor_plans_root() {
                entry.project_roots = vec![root];
            }
        }
    }
}

/// Tag for routing a thread's final cache state back to the right `Sources`
/// slot when it finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Cursor,
    Claude,
    Nvim,
}

/// Messages emitted by the background loader threads. Batches stream while
/// the threads run; each thread emits exactly one `SourceFinished` carrying
/// back its updated mtime and seen-key caches so the next reload can pick
/// up where this scan left off.
pub enum LoadEvent {
    Batch(Vec<HistoryEntry>),
    /// The Cursor composer-attribution map, built off-thread. Carries the DB
    /// mtime it was built from so the receiver can cache it. Applied as a
    /// post-pass over already-loaded Cursor entries.
    CursorSessions {
        sessions: std::sync::Arc<crate::cursor_sessions::CursorSessions>,
        mtime: Option<SystemTime>,
    },
    /// Agent session id → session title, from the Claude Code session files.
    SessionTitles(HashMap<String, String>),
    SourceFinished {
        kind: SourceKind,
        mtimes: HashMap<PathBuf, SystemTime>,
        seen: HashSet<PathBuf>,
    },
}

/// The searchable text of a prompt entry: the prompt, then the response
/// after a blank line when the turn has one.
fn turn_text(entry: &HistoryEntry) -> String {
    let prompt = entry.prompt_text().unwrap_or("");
    match entry.response_text() {
        Some(response) => format!("{prompt}\n\n{response}"),
        None => prompt.to_string(),
    }
}

/// Stable cache key for an entry's searchable text. FileChange entries key on
/// their snapshot file (one per edit); Prompt entries key on the synthetic
/// `file_path` they already use as their unique id. Both survive the
/// re-sort of `store.entries` that follows a load-batch merge.
fn content_cache_key(entry: &HistoryEntry) -> PathBuf {
    match &entry.kind {
        HistoryEntryKind::Prompt { .. } => entry.file_path.clone(),
        // A Cursor blob snapshot is named after its content hash, so two edits
        // that end at the same content share the name. Their diffs differ, so
        // the key must carry the base too.
        HistoryEntryKind::FileChange => match &entry.base_path {
            Some(base) => PathBuf::from(format!(
                "{}::{}",
                base.display(),
                entry.snapshot_path.display()
            )),
            None => entry.snapshot_path.clone(),
        },
    }
}

/// Project chain as if a file inside `dir` were the entry. Probes `dir/.`
/// so `dir` itself is eligible as a project root.
fn chain_for_dir(dir: &Path) -> Vec<PathBuf> {
    let probe = dir.join(".");
    let mut chain = find_project_chain(&probe);
    // If the workspace folder itself isn't a project marker (e.g.
    // `~/git/mnode` containing several sub-projects), fall back to using
    // the folder as the only "project" so display names still show it.
    if chain.is_empty() {
        chain.push(dir.to_path_buf());
    }
    chain
}

fn cursor_history_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    for candidate in [
        "Library/Application Support/Cursor/User/History",
        "Library/Application Support/Code/User/History",
        ".config/Cursor/User/History",
        ".config/Code/User/History",
    ] {
        let p = PathBuf::from(&home).join(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    None
}



/// How many entries the freshly-loaded index already covers. The indexer
/// counted every entry as work before it read the store, so the UI subtracts
/// these to show a true total.
fn count_known(index: &EmbedIndex, entries: &[HistoryEntry]) -> usize {
    entries
        .iter()
        .filter(|e| index.contains(&content_cache_key(e).to_string_lossy()))
        .count()
}

// ── Semantic search state ────────────────────────────────────────────────────

/// What the embedding subsystem is doing. The help bar prints this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedStatus {
    /// recap has not started the server yet. `F` starts it.
    Off,
    /// The server process runs and loads the model.
    Starting,
    /// The indexer embeds entries that the store does not hold yet.
    Indexing,
    /// Every entry has a vector.
    Ready,
    /// The server or the indexer stopped with this message.
    Failed(String),
}

/// One entry that the indexer must embed. The job carries everything the
/// worker thread needs, so the thread never touches [`HistoryStore`].
pub struct EmbedJob {
    key: String,
    kind: EmbedKind,
    /// The prompt body. `None` for a file change, whose text the worker
    /// builds from the two snapshots.
    text: Option<String>,
    new_path: PathBuf,
    old_path: Option<PathBuf>,
}

/// Messages from the indexer thread.
pub enum EmbedEvent {
    /// The vectors that the store already held, read once at startup.
    Index(EmbedIndex),
    /// One embedded batch, already written to sqlite.
    Batch(Vec<(String, EmbedKind, Vec<f32>)>),
    Done,
    Failed(String),
}

/// Everything the semantic search owns.
pub struct EmbedState {
    pub cfg: EmbedConfig,
    pub index: EmbedIndex,
    pub client: Option<EmbedClient>,
    /// Killed on drop, which stops the server when recap exits.
    server: Option<EmbedServer>,
    rx: Option<mpsc::Receiver<EmbedEvent>>,
    pub status: EmbedStatus,
    /// Entries embedded and entries queued in the current indexer round.
    pub done: usize,
    pub total: usize,
}

impl EmbedState {
    fn new() -> EmbedState {
        let cfg = EmbedConfig::load();
        let index = EmbedIndex::new(cfg.dims);
        EmbedState {
            cfg,
            index,
            client: None,
            server: None,
            rx: None,
            status: EmbedStatus::Off,
            done: 0,
            total: 0,
        }
    }

    /// Text for the help bar, or `None` while the state needs no report.
    pub fn label(&self) -> Option<String> {
        match &self.status {
            EmbedStatus::Off => None,
            EmbedStatus::Starting => Some("  [embed: starting…]".to_string()),
            EmbedStatus::Indexing => {
                Some(format!("  [embedding {}/{}]", self.done, self.total))
            }
            EmbedStatus::Ready => None,
            EmbedStatus::Failed(e) => Some(format!("  [embed: {e}]")),
        }
    }
}

/// Read the two file contents that a file-change entry diffs.
///
/// This function and [`diff_text`] hold the shared definition of "the
/// searchable text of an entry": the `f` search and the embedding indexer
/// both build their text from them, so the two searches never disagree about
/// what one entry says.
fn entry_snapshots(store: &HistoryStore, idx: usize) -> (String, String) {
    let entry = &store.entries[idx];
    let new = std::fs::read_to_string(&entry.snapshot_path).unwrap_or_default();
    let old = match store.prev_snapshot(idx) {
        Some(p) => std::fs::read_to_string(p).unwrap_or_default(),
        None => String::new(),
    };
    (old, new)
}

/// Concatenate the `+` and `-` bodies of a diff, one per line.
fn diff_text(old: &str, new: &str) -> String {
    let mut text = String::new();
    for line in crate::diff::build_diff_from(old, new) {
        if let DiffLine::Added(l) | DiffLine::Removed(l) = line {
            text.push_str(&l);
            text.push('\n');
        }
    }
    text
}

/// Embed every job, write each batch to sqlite, and report the batch back so
/// the UI thread can search it right away.
fn run_indexer(
    cfg: EmbedConfig,
    client: EmbedClient,
    jobs: Vec<EmbedJob>,
    load_index: bool,
    tx: mpsc::Sender<EmbedEvent>,
) {
    if let Err(e) = client.wait_ready(Duration::from_secs(120)) {
        let _ = tx.send(EmbedEvent::Failed(e.to_string()));
        return;
    }
    let mut db = match EmbedDb::open(&cfg) {
        Ok(db) => db,
        Err(e) => {
            let _ = tx.send(EmbedEvent::Failed(e.to_string()));
            return;
        }
    };

    // The first round reads what earlier runs already embedded.
    let mut known = std::collections::HashSet::new();
    if load_index {
        match db.load_index() {
            Ok(index) => {
                known = db.keys().unwrap_or_default();
                let _ = tx.send(EmbedEvent::Index(index));
            }
            Err(e) => {
                let _ = tx.send(EmbedEvent::Failed(e.to_string()));
                return;
            }
        }
    }

    let todo: Vec<&EmbedJob> = jobs.iter().filter(|j| !known.contains(&j.key)).collect();
    for chunk in todo.chunks(crate::embed::BATCH) {
        let texts: Vec<String> = chunk
            .iter()
            .map(|job| match &job.text {
                Some(t) => t.clone(),
                None => {
                    let new = std::fs::read_to_string(&job.new_path).unwrap_or_default();
                    let old = match &job.old_path {
                        Some(p) => std::fs::read_to_string(p).unwrap_or_default(),
                        None => String::new(),
                    };
                    diff_text(&old, &new)
                }
            })
            .collect();

        let vecs = match client.embed_documents(&texts) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(EmbedEvent::Failed(e.to_string()));
                return;
            }
        };
        let rows: Vec<(String, EmbedKind, Vec<f32>)> = chunk
            .iter()
            .zip(vecs)
            .map(|(job, vec)| (job.key.clone(), job.kind, vec))
            .collect();
        if let Err(e) = db.insert_batch(&rows) {
            let _ = tx.send(EmbedEvent::Failed(e.to_string()));
            return;
        }
        if tx.send(EmbedEvent::Batch(rows)).is_err() {
            return; // The UI closed.
        }
    }
    let _ = tx.send(EmbedEvent::Done);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Projects,
    Files,
    /// Agent mode only. Lists one row per agent session.
    Agents,
    History,
    Diff,
}

/// A mouse text selection in the Diff panel.
///
/// The two ends name a cell of the panel's inner rect: `row` counts the
/// screen lines below the top border, `col` counts the characters after the
/// left border. The user drags in either direction, so `anchor` is the cell
/// where the drag started and `cursor` is the cell under the pointer;
/// [`App::diff_selection_range`] puts the pair in reading order.
///
/// `scroll` and `generation` record what the panel showed when the user made
/// the selection. The app reads the selection only while both still match, so
/// a scroll or a new diff drops it instead of highlighting the wrong text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffSelection {
    pub anchor: (usize, usize),
    pub cursor: (usize, usize),
    scroll: usize,
    generation: u64,
}

/// How the app pivots the entries.
///
/// `File` is the original layout: Projects / Files → History → Diff. `Agent`
/// inserts an Agents panel in front of History and filters History down to the
/// selected agent, so the panel chain gains one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    File,
    Agent,
}

impl Mode {
    /// The mode the `m` key switches to.
    pub fn other(self) -> Mode {
        match self {
            Mode::File => Mode::Agent,
            Mode::Agent => Mode::File,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::File => "file",
            Mode::Agent => "agent",
        }
    }
}

/// Which entry kinds the History panel shows. The filter narrows
/// [`App::filtered_indices`], which every History search reads, so a search
/// never reaches an entry that the filter hides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindFilter {
    All,
    Prompts,
    Changes,
}

impl KindFilter {
    /// The next value that the `p` key selects.
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

    /// Whether one entry passes the filter.
    pub fn keeps(self, entry: &HistoryEntry) -> bool {
        match self {
            KindFilter::All => true,
            KindFilter::Prompts => entry.is_prompt(),
            KindFilter::Changes => !entry.is_prompt(),
        }
    }
}

/// Whether the agent session behind an entry still runs.
///
/// `Unknown` covers the sources that report nothing: Neovim has no session,
/// and Cursor gives a composer id that no registry tracks. The renderer keeps
/// the bright color for `Unknown` and dims only `Stopped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Running,
    Stopped,
    Unknown,
}

/// Identifies one history entry across a re-sort. `store.entries` re-sorts on
/// every load batch, so an index is not stable; the file path and the
/// timestamp together are.
pub type EntryId = (PathBuf, DateTime<Local>);

/// What the app must restore when it returns to File mode.
///
/// Switching File → Agent maps the selected row onto its one agent. The switch
/// back is one agent to many rows, so the app remembers the exact File-mode
/// row instead of guessing. `agent_row` records the row that Agent mode
/// selected on arrival: when the user has not moved off it, the return trip
/// goes back to `file_row`; when the user has moved, the new row wins, because
/// that is where the user now works.
#[derive(Debug, Clone, Default)]
struct ModeReturn {
    file_row: Option<EntryId>,
    agent_row: Option<EntryId>,
}

/// One agent session, as the Agents panel shows it. Rebuilt by
/// [`App::recompute_agents`] from `store.entries`.
#[derive(Debug, Clone)]
pub struct Agent {
    pub session_id: String,
    pub source: EditSource,
    /// Time of the agent's newest prompt, or of its newest entry when the
    /// agent has no prompt.
    pub last_ts: DateTime<Local>,
    pub prompt_count: usize,
    /// Project chain of the agent's newest entry, outermost → innermost.
    pub project_roots: Vec<PathBuf>,
    /// Text of the agent's newest prompt. Empty when the agent has none.
    pub last_prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPhase {
    /// No search interaction.
    Inactive,
    /// User is editing the query at the prompt.
    Typing,
    /// Query committed; matches highlighted, n/N navigates.
    Active,
}

/// Which corpus the current search runs against. `Path` fuzzy-matches the
/// rendered display path (project chain + filename / inline prompt). `Content`
/// regex-matches the diff body (added + removed lines) for FileChange entries
/// and the full prompt text for Prompt entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Path,
    Content,
    /// Ranks entries by the cosine similarity of their embedding against the
    /// embedded query. See [`crate::embed`].
    Semantic,
}

#[derive(Debug, Clone)]
pub struct SearchMatch {
    /// Index into `store.entries`.
    pub entry_idx: usize,
    /// Character indices (over `display_path`) that participated in the match.
    /// Empty for content matches — they don't highlight in the History panel
    /// (the match is in the diff body, not in `display_path`).
    pub indices: Vec<u32>,
    pub score: u16,
}

pub struct SearchState {
    pub phase: SearchPhase,
    pub kind: SearchKind,
    pub query: String,
    pub matches: Vec<SearchMatch>,
    /// Index into `matches` of the entry the user is currently parked on.
    pub current: usize,
    /// Whether matches should be visually highlighted right now. Vim-like:
    /// Esc flips this off without discarding the search; n/N flips it back on.
    pub highlights: bool,
    /// The query the user last ran, and the corpus it ran against. They
    /// outlive `query` and `matches`, which Esc at the prompt and a panel
    /// switch both drop, so `n` can run the same search again. This is the
    /// text the user committed, not the text in the prompt right now.
    pub last_query: String,
    pub last_kind: SearchKind,
    /// (sel_group, sel_child) snapshot taken when entering Typing — restored on cancel.
    pub restore: Option<(usize, usize)>,
    /// Compiled regex for the current content-search query. `None` while the
    /// query is empty or fails to parse. Used both for picking matches and
    /// for highlighting hits in the Diff panel.
    pub content_regex: Option<Regex>,
    /// Content search only: when the query last changed, while the corpus scan
    /// is still deferred. See [`App::maybe_run_pending_search`].
    pending_since: Option<Instant>,
    matcher: Matcher,
}

/// How long typing has to pause before a content (`f`) search scans the corpus.
/// Content search reads and diffs every snapshot the filter exposes, so running
/// it per keystroke means a fast typist queues several full sweeps, each of
/// which blocks the UI thread.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// Shortest content-search query that's worth scanning for. A one-character
/// regex matches nearly every entry, so that keystroke's sweep is pure cost —
/// and it's the keystroke that would otherwise pay for the whole cold corpus.
const CONTENT_MIN_CHARS: usize = 2;

/// How many results a semantic search keeps. The list is ranked, so a long
/// tail of weak matches adds noise and no information.
const SEMANTIC_TOP_K: usize = 50;

/// How far below the best score a result may fall and still appear. Qwen3
/// cosine scores cluster between 0.2 and 0.8, so an absolute floor is
/// fragile; the distance to the best result is stable.
const SEMANTIC_REL_FLOOR: f32 = 0.15;

/// Scale that packs a cosine score into `SearchMatch::score`, which the path
/// search already uses for its own ranking.
const SEMANTIC_SCORE_SCALE: f32 = 10_000.0;

/// How many rows after (or before) a revealed search match stay visible, so
/// the match does not sit on the last (or the first) line of the panel.
const REVEAL_CONTEXT: usize = 2;

/// Which side of a revealed row gets the [`REVEAL_CONTEXT`] rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RevealDir {
    /// The search moves forward, so show the rows after the match.
    Down,
    /// The search moves backward, so show the rows before the match.
    Up,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            phase: SearchPhase::Inactive,
            kind: SearchKind::Path,
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            highlights: true,
            last_query: String::new(),
            last_kind: SearchKind::Path,
            restore: None,
            content_regex: None,
            pending_since: None,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }
}

impl SearchState {
    /// Lookup table from entry_idx → position in `matches`.
    pub fn match_pos_for(&self, entry_idx: usize) -> Option<usize> {
        self.matches.iter().position(|m| m.entry_idx == entry_idx)
    }

    /// True while the query has changed but the corpus scan hasn't run yet, so
    /// `matches` describes an older query. Drives the prompt's `…` indicator.
    pub fn scan_pending(&self) -> bool {
        self.pending_since.is_some()
    }

    /// True when the query is too short for a content search to be worth
    /// running — `matches` is deliberately empty and the prompt says so
    /// instead of reading "(0 matches)".
    pub fn needs_more_chars(&self) -> bool {
        self.kind == SearchKind::Content
            && !self.query.is_empty()
            && self.query.chars().count() < CONTENT_MIN_CHARS
    }
}

/// A fuzzy hit inside a side panel (Projects or Files).
#[derive(Debug, Clone)]
pub struct PanelSearchMatch {
    /// Index into the panel's items list (0-based; does NOT count the
    /// leading "All …" row).
    pub item_idx: usize,
    /// Char positions in the matched display string that participated in
    /// the fuzzy match — for visual highlighting.
    pub indices: Vec<u32>,
    pub score: u16,
}

/// Search state shared by the Projects and Files panels. Mirrors
/// `SearchState`'s lifecycle (Typing → Active → Inactive) but matches
/// against panel item labels rather than history rows. Each side panel
/// owns its own instance so toggling focus doesn't clobber the other's
/// matches.
pub struct PanelSearchState {
    pub phase: SearchPhase,
    pub query: String,
    pub matches: Vec<PanelSearchMatch>,
    pub current: usize,
    pub highlights: bool,
    /// Selection captured when the prompt opened, restored on cancel.
    /// `None` means the "All …" row was selected. The field is set only
    /// while `phase == Typing` — `commit` and `clear` zero it out.
    pub restore: Option<PathBuf>,
    matcher: Matcher,
}

impl Default for PanelSearchState {
    fn default() -> Self {
        Self {
            phase: SearchPhase::Inactive,
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            highlights: true,
            restore: None,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }
}

/// Vim-like `/` search restricted to the diff currently rendered in the Diff
/// panel. Matches are positions in `App::diff_lines` whose text contains the
/// regex; n/N steps between them and scrolls the Diff panel. Shares the
/// regex-highlight rendering path with the History panel's content search.
pub struct DiffSearchState {
    pub phase: SearchPhase,
    pub query: String,
    /// Indices into `App::diff_lines` whose searchable text matched `regex`.
    pub matches: Vec<usize>,
    pub current: usize,
    pub highlights: bool,
    pub regex: Option<Regex>,
    /// `diff_scroll` snapshot taken when Typing opens — restored on cancel.
    pub restore_scroll: Option<usize>,
}

impl Default for DiffSearchState {
    fn default() -> Self {
        Self {
            phase: SearchPhase::Inactive,
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            highlights: true,
            regex: None,
            restore_scroll: None,
        }
    }
}

/// Per-line searchable text for diff content search. Matches the corpus the
/// History panel's `f` search builds (Added/Removed body lines, plus Prompt
/// lines for prompt entries), so `/` in the Diff panel feels like the same
/// search restricted to whatever the Diff currently shows.
fn diff_line_text(dl: &DiffLine) -> Option<&str> {
    match dl {
        DiffLine::Added(s) | DiffLine::Removed(s) | DiffLine::Prompt(s) | DiffLine::Response(s) => Some(s),
        DiffLine::Context(_) | DiffLine::Header(_) | DiffLine::ChildSeparator { .. } => None,
    }
}

impl PanelSearchState {
    pub fn match_pos_for(&self, item_idx: usize) -> Option<usize> {
        self.matches.iter().position(|m| m.item_idx == item_idx)
    }

    pub fn clear(&mut self) {
        self.phase = SearchPhase::Inactive;
        self.query.clear();
        self.matches.clear();
        self.current = 0;
        self.highlights = true;
        self.restore = None;
    }

    fn hide(&mut self) {
        if self.phase == SearchPhase::Active {
            self.highlights = false;
        }
    }

    /// Fuzzy-rank `items` against the current query. Items are referenced by
    /// their position in the slice; that index becomes `PanelSearchMatch::item_idx`.
    fn recompute(&mut self, items: &[String]) {
        self.matches.clear();
        self.current = 0;
        if self.query.is_empty() {
            return;
        }
        // nucleo's raw `Matcher` requires a lowercase needle when
        // `ignore_case` is on (the high-level `Pattern` API normalizes this);
        // an uppercase char trips a debug assertion inside the matcher.
        let query = self.query.to_lowercase();
        let mut needle_buf: Vec<char> = Vec::new();
        let needle = Utf32Str::new(&query, &mut needle_buf);
        let mut hay_buf: Vec<char> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        let mut results: Vec<PanelSearchMatch> = Vec::new();
        for (i, s) in items.iter().enumerate() {
            hay_buf.clear();
            let haystack = Utf32Str::new(s, &mut hay_buf);
            indices.clear();
            if let Some(score) =
                self.matcher.fuzzy_indices(haystack, needle, &mut indices)
            {
                results.push(PanelSearchMatch {
                    item_idx: i,
                    indices: indices.clone(),
                    score,
                });
            }
        }
        results.sort_by(|a, b| b.score.cmp(&a.score));
        self.matches = results;
    }
}

/// A consecutive run of edits on the same file.
pub struct HistoryGroup {
    /// Indices into store.entries, sorted most-recent first.
    pub entry_indices: Vec<usize>,
    pub expanded: bool,
}

/// A row in the flattened display list.
#[derive(Debug, Clone)]
pub enum DisplayRow {
    Group(usize),
    Child { group_idx: usize, child_local: usize },
}

/// Default / clamp bounds for the prompt line-count (`>`/`<`).
pub const DEFAULT_PROMPT_LINES: usize = 5;
const MIN_PROMPT_LINES: usize = 1;
const MAX_PROMPT_LINES: usize = 20;

pub struct App {
    pub store: HistoryStore,
    pub active_panel: Panel,

    pub selected_project_idx: Option<usize>,
    pub project_scroll: usize,

    /// Files (file_path) visible in the Files panel — derived from
    /// `store.entries` filtered by `selected_project_idx`, sorted newest
    /// first by `file_latest`. Prompt entries are excluded since their
    /// `file_path` is a synthetic per-prompt key.
    pub files: Vec<PathBuf>,
    pub file_latest: HashMap<PathBuf, DateTime<Local>>,
    pub file_counts: HashMap<PathBuf, usize>,
    /// Project chain (outermost → innermost) for each listed file. Cached
    /// off the first entry encountered for that path so the UI doesn't have
    /// to re-walk `store.entries` per visible row.
    pub file_chains: HashMap<PathBuf, Vec<PathBuf>>,
    /// `None` → "All files" row. Path-based (not index) so the selection
    /// survives a re-sort after a load batch arrives.
    pub selected_file_path: Option<PathBuf>,
    pub file_scroll: usize,

    /// Global pivot. See [`Mode`].
    pub mode: Mode,

    /// Which entry kinds History shows. See [`KindFilter`].
    pub kind_filter: KindFilter,

    /// Agents visible in the Agents panel — derived from `store.entries`
    /// filtered by `selected_project_idx`, newest first. Entries without a
    /// session id (Neovim edits, unattributed Cursor saves) have no agent, so
    /// they do not appear here and Agent mode does not reach them.
    pub agents: Vec<Agent>,
    /// Session id of the selected agent. Stored by id (not index) so the
    /// selection survives the re-sort that each load batch causes.
    pub selected_agent: Option<String>,
    pub agent_scroll: usize,
    /// Selection memory for the `m` toggle. See [`ModeReturn`].
    mode_return: ModeReturn,

    /// Position of each prompt inside its own agent session, counted from the
    /// oldest prompt as 1. Keyed by the prompt's synthetic `file_path`, which
    /// is unique and stable across the entry re-sort. The History panel prints
    /// the number in front of the prompt text.
    pub prompt_ordinals: HashMap<PathBuf, usize>,

    pub groups: Vec<HistoryGroup>,
    pub display_rows: Vec<DisplayRow>,

    /// Logical selection — always points at a Child row.
    pub sel_group: usize,
    pub sel_child: usize,

    /// Scroll offset into display_rows.
    pub history_scroll: usize,

    pub diff_scroll: usize,
    pub absolute_time: bool,
    pub diff_lines: Vec<DiffLine>,
    /// Mouse text selection in the Diff panel. See [`DiffSelection`].
    diff_selection: Option<DiffSelection>,
    /// Counts the rebuilds of `diff_lines`. A selection that names an older
    /// count describes text the panel no longer shows.
    diff_generation: u64,

    /// Max visual lines a prompt entry may span in the History list. Adjusted
    /// with `>`/`<`, persisted across restarts.
    pub prompt_lines: usize,

    /// Which columns the History panel draws, and whether it prints a date
    /// row for each day. The `S` key opens the menu that changes them.
    pub settings: crate::settings::Settings,
    /// Width (chars) of the aligned project column: the widest project chain
    /// of the current display rows, capped by `MAX_PROJECT_COL_CHARS`.
    /// `rebuild_display` computes it, so it does not change while the user
    /// scrolls. Read it with [`Self::project_col_width`].
    project_col_chars: usize,
    pub settings_menu: crate::settings::SettingsMenu,

    /// Per-`display_rows` visual height in terminal lines. Recomputed each
    /// frame from the panel width (prompts wrap to up to `prompt_lines`
    /// lines; everything else is 1). Drives scroll/click math so a tall
    /// prompt row doesn't confuse the single-line-per-row assumption.
    pub hist_row_heights: Vec<usize>,

    /// The inputs that `hist_row_heights` was computed from. The main loop
    /// calls `recompute_row_heights` every frame, but the heights only change
    /// when one of these inputs changes.
    row_heights_key: Option<(usize, usize, usize, u64, u8)>,

    /// The History list width that the last `recompute_row_heights` call used.
    /// `adjust_scroll` needs it to refresh stale heights before it measures
    /// rows, because a refilter changes `display_rows` between two frames.
    last_list_width: usize,

    /// The number of text lines that the Diff panel showed on the last frame.
    /// `reveal_diff_line` needs it to scroll a search match into view, but
    /// `refresh_diff` runs outside the draw, so the main loop stores it here.
    last_diff_height: usize,

    /// Counter that increases each time `rebuild_display` writes
    /// `display_rows`. It is part of the row-height cache key.
    display_gen: u64,

    /// Lazily-computed +/- line counts, keyed by entry index.
    pub diff_summaries: HashMap<usize, DiffSummary>,

    /// Lazily-computed per-entry searchable text for content search. For
    /// FileChange entries the value is the concatenation of `+`/`-` diff
    /// lines; for Prompt entries it's the full prompt text. Keyed by
    /// `snapshot_path` (FileChange) or the synthetic prompt `file_path`
    /// (Prompt) so the cache survives the entry-vec re-sort that happens
    /// when a load batch is merged.
    content_cache: HashMap<PathBuf, String>,

    pub search: SearchState,
    /// The semantic-search subsystem: the model server, the vector index and
    /// the indexer thread. See [`crate::embed`].
    pub embed: EmbedState,
    pub project_search: PanelSearchState,
    pub file_search: PanelSearchState,
    pub diff_search: DiffSearchState,
    /// The committed query of every search prompt, which Up and Down walk.
    /// See [`crate::search_history`].
    pub search_history: SearchHistories,

    /// When true (default), focusing a multi-entry group expands it and the
    /// cursor traverses individual children. When false, groups stay
    /// collapsed and selectable as a single row; the diff panel shows a
    /// concatenation of every child's diff with per-child separators.
    ///
    /// Read through `effective_auto_expand()` everywhere — selecting a
    /// single file forces expand on regardless of this user toggle, since
    /// otherwise every edit to the file collapses into one group.
    pub auto_expand: bool,

    /// External entry sources for incremental reloads (`r` keybinding).
    pub sources: Sources,

    /// Snapshot of currently-running Claude Code sessions, indexed by session
    /// id. Drives the "running" indicator column and the `o` (focus/resume)
    /// keybinding. Refreshed on a timer via [`Self::maybe_rescan_running`].
    pub running: crate::session::RunningSessions,
    running_scanned_at: Option<Instant>,

    /// True while at least one loader thread is running. Drives the
    /// `[loading]` indicator in the help bar.
    pub loading: bool,

    /// Receiver for batches/finish events from the loader threads. `None`
    /// when no scan is in flight.
    rx: Option<mpsc::Receiver<LoadEvent>>,

    /// Number of loader threads that haven't reported `SourceFinished` yet.
    loading_sources: u8,

    /// The oldest file mtime that the next load reads. Each loader skips a
    /// file that is older, which makes a load of a short period much faster.
    /// `None` reads the full history, which is what the TUI does.
    pub load_since: Option<SystemTime>,

    /// Persisted project selection (by root path) awaiting resolution to an
    /// index. The projects list is empty at startup and fills in async, so the
    /// path is held here and resolved against `store.projects` as batches
    /// arrive. Only honored while `loading`; cleared once loading finishes.
    pending_project_restore: Option<PathBuf>,
    /// Persisted file selection awaiting its entries to load. Keeps
    /// `selected_file_path` from being dropped by `recompute_files` before the
    /// file's first entry has streamed in. Cleared once loading finishes.
    pending_file_restore: Option<PathBuf>,
    /// Persisted agent selection awaiting its entries. The Agents panel fills
    /// in async, so the id waits here and [`Self::recompute_agents`] keeps the
    /// selection on it until loading finishes.
    pending_agent_restore: Option<String>,
}

impl App {
    pub fn new(store: HistoryStore, sources: Sources) -> Self {
        let mut app = Self {
            store,
            active_panel: Panel::History,
            selected_project_idx: None,
            project_scroll: 0,
            files: Vec::new(),
            file_latest: HashMap::new(),
            file_counts: HashMap::new(),
            file_chains: HashMap::new(),
            selected_file_path: None,
            file_scroll: 0,
            mode: Mode::File,
            kind_filter: KindFilter::All,
            agents: Vec::new(),
            selected_agent: None,
            agent_scroll: 0,
            mode_return: ModeReturn::default(),
            prompt_ordinals: HashMap::new(),
            groups: Vec::new(),
            display_rows: Vec::new(),
            sel_group: 0,
            sel_child: 0,
            history_scroll: 0,
            diff_scroll: 0,
            absolute_time: false,
            diff_lines: Vec::new(),
            diff_selection: None,
            diff_generation: 0,
            prompt_lines: DEFAULT_PROMPT_LINES,
            settings: crate::settings::Settings::default(),
            project_col_chars: 0,
            settings_menu: crate::settings::SettingsMenu::default(),
            hist_row_heights: Vec::new(),
            row_heights_key: None,
            last_list_width: 0,
            last_diff_height: 0,
            display_gen: 0,
            diff_summaries: HashMap::new(),
            content_cache: HashMap::new(),
            search: SearchState::default(),
            embed: EmbedState::new(),
            project_search: PanelSearchState::default(),
            file_search: PanelSearchState::default(),
            diff_search: DiffSearchState::default(),
            search_history: SearchHistories::default(),
            auto_expand: false,
            sources,
            running: crate::session::RunningSessions::scan(),
            running_scanned_at: Some(Instant::now()),
            loading: false,
            rx: None,
            loading_sources: 0,
            load_since: None,
            pending_project_restore: None,
            pending_file_restore: None,
            pending_agent_restore: None,
        };
        app.refilter();
        app
    }

    /// Seed selection/toggles from persisted state. The project and file are
    /// stored by path and parked in the `pending_*_restore` fields; they are
    /// resolved as load batches arrive (see [`Self::merge_batch`] and
    /// [`Self::recompute_files`]). `absolute_time` is left to the caller so a
    /// CLI override can take precedence.
    pub fn apply_persisted(&mut self, state: &crate::persist::PersistedState) {
        self.pending_project_restore = state.selected_project.clone();
        self.selected_file_path = state.selected_file.clone();
        self.pending_file_restore = state.selected_file.clone();
        self.auto_expand = state.auto_expand;
        self.prompt_lines = state.prompt_lines.clamp(MIN_PROMPT_LINES, MAX_PROMPT_LINES);
        self.mode = if state.agent_mode { Mode::Agent } else { Mode::File };
        self.selected_agent = state.selected_agent.clone();
        self.pending_agent_restore = state.selected_agent.clone();
        self.search_history = SearchHistories::from_snapshot(&state.search_history);
        self.settings = state.settings;
        if self.mode == Mode::Agent {
            self.active_panel = Panel::Agents;
        }
    }

    /// Snapshot the bits of UI state worth surviving a restart.
    pub fn persisted_state(&self) -> crate::persist::PersistedState {
        let selected_project = self
            .selected_project_idx
            .and_then(|i| self.store.projects.get(i).cloned())
            .or_else(|| self.pending_project_restore.clone());
        let selected_file = self
            .selected_file_path
            .clone()
            .or_else(|| self.pending_file_restore.clone());
        let selected_agent = self
            .selected_agent
            .clone()
            .or_else(|| self.pending_agent_restore.clone());
        crate::persist::PersistedState {
            selected_project,
            selected_file,
            absolute_time: self.absolute_time,
            auto_expand: self.auto_expand,
            prompt_lines: self.prompt_lines,
            agent_mode: self.mode == Mode::Agent,
            selected_agent,
            search_history: self.search_history.snapshot(),
            settings: self.settings,
        }
    }

    /// Spawn one background thread per detected source. Each thread streams
    /// per-folder/session/file batches in newest-first order through the
    /// returned channel. Returns immediately; the UI loop should call
    /// `drain_load_events` each frame to merge new batches.
    pub fn start_background_load(&mut self) {
        if self.loading {
            return;
        }
        let (tx, rx) = mpsc::channel::<LoadEvent>();
        let since = self.load_since;
        self.loading_sources = 0;
        crate::history::clear_dir_states();

        if let Some(dir) = self.sources.cursor_dir.clone() {
            let mut mtimes = std::mem::take(&mut self.sources.cursor_mtimes);
            let mut seen = std::mem::take(&mut self.sources.cursor_seen);
            let loader_tx = tx.clone();
            self.loading_sources += 1;
            std::thread::spawn(move || {
                let mut on_batch = |batch: Vec<HistoryEntry>| -> bool {
                    loader_tx.send(LoadEvent::Batch(batch)).is_ok()
                };
                if let Err(e) =
                    HistoryStore::load_cursor(&dir, &mut mtimes, &mut seen, since, &mut on_batch)
                {
                    crate::log_warn!("recap: cursor history failed: {e}");
                }
                let _ = loader_tx.send(LoadEvent::SourceFinished {
                    kind: SourceKind::Cursor,
                    mtimes,
                    seen,
                });
            });

            // Build the composer-attribution map on its own thread so the
            // multi-GB DB scan never blocks Cursor edits from streaming. It's
            // cached by DB mtime, so an unchanged store returns instantly.
            let prev = self.sources.cursor_sessions.clone();
            let prev_mtime = self.sources.cursor_sessions_mtime;
            let attrib_tx = tx.clone();
            // Counted so the load channel stays open until attribution lands.
            self.loading_sources += 1;
            std::thread::spawn(move || {
                let (sessions, mtime) =
                    crate::cursor_sessions::CursorSessions::load_cached(prev, prev_mtime);
                let _ = attrib_tx.send(LoadEvent::CursorSessions { sessions, mtime });
            });
        }

        if let Some(dir) = self.sources.claude_dir.clone() {
            let mut mtimes = std::mem::take(&mut self.sources.claude_mtimes);
            let mut seen = std::mem::take(&mut self.sources.claude_seen);
            let tx = tx.clone();
            self.loading_sources += 1;
            std::thread::spawn(move || {
                let mut on_batch = |batch: Vec<HistoryEntry>,
                                    titles: HashMap<String, String>|
                 -> bool {
                    if !titles.is_empty()
                        && tx.send(LoadEvent::SessionTitles(titles)).is_err()
                    {
                        return false;
                    }
                    if batch.is_empty() {
                        return true;
                    }
                    tx.send(LoadEvent::Batch(batch)).is_ok()
                };
                if let Err(e) = claude::load(&dir, &mut mtimes, &mut seen, since, &mut on_batch)
                {
                    crate::log_warn!("recap: claude history failed: {e}");
                }
                let _ = tx.send(LoadEvent::SourceFinished {
                    kind: SourceKind::Claude,
                    mtimes,
                    seen,
                });
            });
        }

        if let Some(dir) = self.sources.nvim_dir.clone() {
            let mut mtimes = std::mem::take(&mut self.sources.nvim_mtimes);
            let mut seen = std::mem::take(&mut self.sources.nvim_seen);
            let tx = tx.clone();
            self.loading_sources += 1;
            std::thread::spawn(move || {
                let mut on_batch = |batch: Vec<HistoryEntry>| -> bool {
                    tx.send(LoadEvent::Batch(batch)).is_ok()
                };
                if let Err(e) = nvim::load(&dir, &mut mtimes, &mut seen, since, &mut on_batch) {
                    crate::log_warn!("recap: neovim history failed: {e}");
                }
                let _ = tx.send(LoadEvent::SourceFinished {
                    kind: SourceKind::Nvim,
                    mtimes,
                    seen,
                });
            });
        }

        // Drop the original sender so the receiver disconnects once every
        // thread's clone is dropped (covers the no-sources case too).
        drop(tx);

        if self.loading_sources > 0 {
            self.loading = true;
            self.rx = Some(rx);
        }
    }

    /// Pull every available message from the loader channel and apply them.
    /// All batches accumulated in a single drain are merged once so we
    /// re-sort and recompute project metadata at most once per frame.
    pub fn drain_load_events(&mut self, visible: usize) {
        let Some(rx) = self.rx.as_ref() else {
            return;
        };

        let mut accumulated: Vec<HistoryEntry> = Vec::new();
        let mut disconnected = false;
        let mut cursor_attribution_pending = false;
        loop {
            match rx.try_recv() {
                Ok(LoadEvent::Batch(mut entries)) => accumulated.append(&mut entries),
                Ok(LoadEvent::SessionTitles(titles)) => {
                    self.store.session_titles.extend(titles);
                }
                Ok(LoadEvent::CursorSessions { sessions, mtime }) => {
                    self.sources.cursor_sessions_mtime = mtime;
                    // The composer map always covers the whole database. Drop
                    // what is older than the load period, so a short period
                    // stays cheap here too.
                    let cutoff = self.load_since.map(DateTime::<Local>::from);
                    // Extend, never replace: the Claude loader writes its
                    // titles into the same map.
                    self.store
                        .session_titles
                        .extend(sessions.names_map().clone());
                    // Merge the composer's user prompts like Claude's, deduped
                    // by their unique synthetic key (`cursor:<composer>:<bubble>`)
                    // so a rebuild on mtime change doesn't re-add old prompts.
                    // A prompt with a response passes anyway: `merge_batch`
                    // replaces the entry, so a response that arrived after
                    // the last rebuild reaches the store.
                    let have: HashSet<&PathBuf> = self
                        .store
                        .entries
                        .iter()
                        .filter(|e| e.is_prompt())
                        .map(|e| &e.file_path)
                        .chain(accumulated.iter().filter(|e| e.is_prompt()).map(|e| &e.file_path))
                        .collect();
                    let fresh: Vec<HistoryEntry> = sessions
                        .prompts()
                        .iter()
                        .filter(|p| cutoff.is_none_or(|c| p.timestamp >= c))
                        .filter(|p| !have.contains(&p.file_path) || p.response_text().is_some())
                        .cloned()
                        .collect();
                    drop(have);
                    accumulated.extend(fresh);

                    // Merge the composer's file edits the same way. Their key
                    // is (path, time, snapshot), because the snapshot alone is
                    // content-addressed and two edits can produce one content.
                    let have_edits: HashSet<(&PathBuf, DateTime<Local>, &PathBuf)> = self
                        .store
                        .entries
                        .iter()
                        .chain(accumulated.iter())
                        .filter(|e| e.base_path.is_some())
                        .map(|e| (&e.file_path, e.timestamp, &e.snapshot_path))
                        .collect();
                    let fresh_edits: Vec<HistoryEntry> = sessions
                        .edits()
                        .iter()
                        .filter(|e| cutoff.is_none_or(|c| e.timestamp >= c))
                        .filter(|e| {
                            !have_edits.contains(&(&e.file_path, e.timestamp, &e.snapshot_path))
                        })
                        .cloned()
                        .collect();
                    drop(have_edits);
                    accumulated.extend(fresh_edits);
                    self.sources.cursor_sessions = Some(sessions);
                    cursor_attribution_pending = true;
                    self.loading_sources = self.loading_sources.saturating_sub(1);
                }
                Ok(LoadEvent::SourceFinished { kind, mtimes, seen }) => {
                    match kind {
                        SourceKind::Cursor => {
                            self.sources.cursor_mtimes = mtimes;
                            self.sources.cursor_seen = seen;
                        }
                        SourceKind::Claude => {
                            self.sources.claude_mtimes = mtimes;
                            self.sources.claude_seen = seen;
                        }
                        SourceKind::Nvim => {
                            self.sources.nvim_mtimes = mtimes;
                            self.sources.nvim_seen = seen;
                        }
                    }
                    self.loading_sources = self.loading_sources.saturating_sub(1);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        let merged = !accumulated.is_empty();
        if merged {
            self.merge_batch(accumulated, visible);
        }

        // (Re)attribute Cursor edits when the map just arrived, or when new
        // Cursor entries merged while a map is already cached.
        if (cursor_attribution_pending || merged) && self.sources.cursor_sessions.is_some() {
            self.apply_cursor_attribution();
        }

        if disconnected || self.loading_sources == 0 {
            self.loading = false;
            self.rx = None;

            // Loading is done — pending restores have had their chance. Clear
            // them and reconcile once so a file selection that never
            // materialized is dropped rather than left filtering to nothing.
            if self.pending_project_restore.take().is_some()
                | self.pending_file_restore.take().is_some()
                | self.pending_agent_restore.take().is_some()
            {
                let selection_id = self
                    .current_entry()
                    .map(|e| (e.file_path.clone(), e.timestamp));
                self.refilter_preserving(selection_id, visible);
            }
        }
    }

    /// Tag every Cursor file-edit entry with the composer (agent session) that
    /// produced it, using the cached attribution map. Idempotent; run after a
    /// merge or when the map arrives. No-op without a map.
    fn apply_cursor_attribution(&mut self) {
        let Some(sessions) = self.sources.cursor_sessions.clone() else {
            return;
        };
        for entry in &mut self.store.entries {
            if entry.source != crate::history::EditSource::Cursor {
                continue;
            }
            // An entry with a base already carries the composer that the tool
            // call named. A prompt carries its own composer too, and its
            // synthetic `cursor:<composer>:<bubble>` path matches no file, so
            // the fuzzy join would only erase that composer. Only the Local
            // History edits need the join.
            if entry.base_path.is_some() || entry.is_prompt() {
                continue;
            }
            entry.session_id = sessions.attribute(&entry.file_path, entry.timestamp.timestamp_millis());
        }
    }

    fn merge_batch(&mut self, mut entries: Vec<HistoryEntry>, visible: usize) {
        self.sources.attach_plan_project(&mut entries);

        let selection_id = self
            .current_entry()
            .map(|e| (e.file_path.clone(), e.timestamp));

        // Remember the selected project by path: the projects list re-sorts on
        // every merge, so the index would otherwise drift. Fall back to a
        // pending restore from persisted state when nothing is selected yet.
        let desired_project = self
            .selected_project_idx
            .and_then(|i| self.store.projects.get(i).cloned())
            .or_else(|| self.pending_project_restore.clone());

        // A prompt the store already holds arrives again when its source
        // attached the agent's response after the earlier scan. Replace the
        // old entry in place and drop its cached search text.
        if entries.iter().any(|e| e.is_prompt()) {
            let by_key: HashMap<PathBuf, usize> = self
                .store
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.is_prompt())
                .map(|(i, e)| (e.file_path.clone(), i))
                .collect();
            entries.retain_mut(|e| {
                let Some(&i) = by_key.get(&e.file_path).filter(|_| e.is_prompt()) else {
                    return true;
                };
                self.content_cache.remove(&content_cache_key(e));
                std::mem::swap(&mut self.store.entries[i], e);
                false
            });
        }
        self.store.entries.extend(entries);
        self.store
            .entries
            .sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        self.store.recompute_projects();

        // Re-resolve the selection against the rebuilt projects list. Stays
        // `None` (the "All projects" row) until the target project loads.
        self.selected_project_idx = desired_project
            .as_ref()
            .and_then(|p| self.store.projects.iter().position(|q| q == p));

        // Entry indices shifted; cached summaries are keyed by index.
        self.diff_summaries.clear();

        // Drop search results: history matches reference entry indices (which
        // shift on sort), and the side-panel matches reference positions in
        // the projects/files lists (which a new batch can grow or reorder).
        self.search_clear();
        self.project_search.clear();
        self.file_search.clear();

        self.refilter_preserving(selection_id, visible);
    }

    /// Rescan all sources in the background. No-op while a scan is already
    /// in flight. Batches are merged incrementally via `drain_load_events`.
    pub fn reload(&mut self, _visible: usize) {
        self.running = crate::session::RunningSessions::scan();
        self.running_scanned_at = Some(Instant::now());
        self.start_background_load();
    }

    /// Like `refilter` but tries to keep selection on the entry identified by
    /// `(file_path, timestamp)`. Falls back to row 0 if not found.
    fn refilter_preserving(
        &mut self,
        selection_id: Option<(PathBuf, chrono::DateTime<chrono::Local>)>,
        visible: usize,
    ) {
        self.recompute_files();
        self.recompute_prompt_ordinals();
        self.recompute_agents();
        let filtered = self.filtered_indices();
        self.groups = Self::build_groups(&self.store.entries, &filtered);

        let target = selection_id.and_then(|(fp, ts)| {
            self.groups.iter().enumerate().find_map(|(gi, g)| {
                g.entry_indices.iter().position(|&ei| {
                    let e = &self.store.entries[ei];
                    e.file_path == fp && e.timestamp == ts
                }).map(|ci| (gi, ci))
            })
        });
        let (sg, sc) = target.unwrap_or((0, 0));
        self.sel_group = sg;
        self.sel_child = sc;
        self.diff_scroll = 0;

        if !self.groups.is_empty() && self.effective_auto_expand() {
            for g in &mut self.groups {
                g.expanded = false;
            }
            self.groups[self.sel_group].expanded = true;
        }
        self.rebuild_display();
        self.adjust_scroll(visible);
        self.refresh_diff();
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn filtered_indices(&self) -> Vec<usize> {
        let project = self
            .selected_project_idx
            .and_then(|i| self.store.projects.get(i));
        let file = self.selected_file_path.as_ref();
        // Agent mode narrows History to one agent; File mode ignores this.
        // With no agent selected, Agent mode reaches no entry at all — the
        // entries without a session have no agent to select.
        let agent = match self.mode {
            Mode::Agent => Some(self.selected_agent.as_deref().unwrap_or("")),
            Mode::File => None,
        };
        self.store
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| match project {
                None => true,
                Some(p) => e.project_roots.iter().any(|r| r == p),
            })
            .filter(|(_, e)| match file {
                None => true,
                Some(f) => &e.file_path == f,
            })
            .filter(|(_, e)| match agent {
                None => true,
                Some(a) => e.session_id.as_deref() == Some(a),
            })
            .filter(|(_, e)| self.kind_filter.keeps(e))
            .map(|(i, _)| i)
            .collect()
    }

    /// Rebuild the Agents panel list from `store.entries`. Honors the project
    /// filter so the panel mirrors what History can reach. Entries without a
    /// session id have no agent and drop out. `store.entries` is newest first,
    /// so the first prompt each session yields is that agent's newest one.
    fn recompute_agents(&mut self) {
        let project = self
            .selected_project_idx
            .and_then(|i| self.store.projects.get(i))
            .cloned();
        let mut order: Vec<String> = Vec::new();
        let mut by_id: HashMap<String, Agent> = HashMap::new();
        for e in &self.store.entries {
            let Some(sid) = e.session_id.as_deref() else {
                continue;
            };
            if let Some(p) = &project
                && !e.project_roots.iter().any(|r| r == p)
            {
                continue;
            }
            let agent = by_id.entry(sid.to_string()).or_insert_with(|| {
                order.push(sid.to_string());
                Agent {
                    session_id: sid.to_string(),
                    source: e.source,
                    last_ts: e.timestamp,
                    prompt_count: 0,
                    project_roots: e.project_roots.clone(),
                    last_prompt: String::new(),
                }
            });
            if let Some(text) = e.prompt_text() {
                if agent.prompt_count == 0 {
                    // The newest prompt also gives the agent its display time,
                    // which the user reads as "when the agent last ran".
                    agent.last_ts = e.timestamp;
                    agent.last_prompt = crate::history::single_line(text);
                    if agent.project_roots.is_empty() {
                        agent.project_roots = e.project_roots.clone();
                    }
                }
                agent.prompt_count += 1;
            }
        }
        let mut agents: Vec<Agent> = order
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .collect();
        agents.sort_by(|a, b| b.last_ts.cmp(&a.last_ts));

        // Keep the selection on an agent that still exists, and give Agent
        // mode a selection as soon as the first agent loads. A pending
        // restore is exempt: its entries may not have streamed in yet.
        let still_there = self
            .selected_agent
            .as_deref()
            .is_some_and(|id| agents.iter().any(|a| a.session_id == id));
        let restoring = self.pending_agent_restore.is_some()
            && self.pending_agent_restore == self.selected_agent;
        if !still_there && !restoring {
            self.selected_agent = agents.first().map(|a| a.session_id.clone());
            self.agent_scroll = 0;
        }
        self.agents = agents;
    }

    /// Recompute [`Self::prompt_ordinals`]. `store.entries` is newest first,
    /// so the walk runs backwards to count each session's prompts from the
    /// oldest one. A session whose older prompts have not loaded yet counts
    /// from the oldest prompt loaded so far; the numbers settle once the
    /// session's file finishes loading.
    fn recompute_prompt_ordinals(&mut self) {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let mut out: HashMap<PathBuf, usize> = HashMap::new();
        for e in self.store.entries.iter().rev() {
            if !e.is_prompt() {
                continue;
            }
            let Some(sid) = e.session_id.as_deref() else {
                continue;
            };
            let n = counts.entry(sid).or_insert(0);
            *n += 1;
            out.insert(e.file_path.clone(), *n);
        }
        self.prompt_ordinals = out;
    }

    /// Position of a prompt inside its own agent session, counted from 1.
    /// `None` for a file change, or for a prompt with no session.
    pub fn prompt_ordinal(&self, entry: &HistoryEntry) -> Option<usize> {
        if !entry.is_prompt() {
            return None;
        }
        self.prompt_ordinals.get(&entry.file_path).copied()
    }

    /// Rebuild the Files panel list from `store.entries`. Honors the current
    /// project filter so the Files list mirrors what's actually reachable in
    /// History. Prompts are skipped because their `file_path` is synthetic.
    /// Sorting matches Projects: latest-touched first.
    fn recompute_files(&mut self) {
        let project = self
            .selected_project_idx
            .and_then(|i| self.store.projects.get(i))
            .cloned();
        let mut latest: HashMap<PathBuf, DateTime<Local>> = HashMap::new();
        let mut counts: HashMap<PathBuf, usize> = HashMap::new();
        let mut chains: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for e in &self.store.entries {
            if matches!(e.kind, HistoryEntryKind::Prompt { .. }) {
                continue;
            }
            if let Some(p) = &project {
                if !e.project_roots.iter().any(|r| r == p) {
                    continue;
                }
            }
            let slot = latest.entry(e.file_path.clone()).or_insert(e.timestamp);
            if e.timestamp > *slot {
                *slot = e.timestamp;
            }
            *counts.entry(e.file_path.clone()).or_insert(0) += 1;
            chains
                .entry(e.file_path.clone())
                .or_insert_with(|| e.project_roots.clone());
        }
        let mut files: Vec<PathBuf> = latest.keys().cloned().collect();
        files.sort_by(|a, b| {
            let ta = latest
                .get(a)
                .copied()
                .unwrap_or(DateTime::UNIX_EPOCH.into());
            let tb = latest
                .get(b)
                .copied()
                .unwrap_or(DateTime::UNIX_EPOCH.into());
            tb.cmp(&ta)
        });

        // Drop the file selection if it's no longer in the list (e.g.
        // project filter changed away from it). A pending restore is exempt:
        // its entries may simply not have streamed in yet.
        if let Some(sel) = &self.selected_file_path {
            let restoring = self.pending_file_restore.as_deref() == Some(sel.as_path());
            if !latest.contains_key(sel) && !restoring {
                self.selected_file_path = None;
                self.file_scroll = 0;
            }
        }

        self.files = files;
        self.file_latest = latest;
        self.file_counts = counts;
        self.file_chains = chains;
    }

    fn build_groups(entries: &[HistoryEntry], filtered: &[usize]) -> Vec<HistoryGroup> {
        let mut groups: Vec<HistoryGroup> = Vec::new();
        for &idx in filtered {
            let fp = &entries[idx].file_path;
            if let Some(last) = groups.last_mut() {
                if &entries[last.entry_indices[0]].file_path == fp {
                    last.entry_indices.push(idx);
                    continue;
                }
            }
            groups.push(HistoryGroup {
                entry_indices: vec![idx],
                expanded: false,
            });
        }
        groups
    }

    fn rebuild_display(&mut self) {
        let auto_expand = self.effective_auto_expand();
        self.display_rows = self
            .groups
            .iter()
            .enumerate()
            .flat_map(|(gi, g)| {
                let mut rows = vec![DisplayRow::Group(gi)];
                // Single-entry groups render as a flat row — don't emit a child.
                // With auto-expand off, multi-entry groups also stay flat.
                if auto_expand && g.expanded && g.entry_indices.len() > 1 {
                    for ci in 0..g.entry_indices.len() {
                        rows.push(DisplayRow::Child {
                            group_idx: gi,
                            child_local: ci,
                        });
                    }
                }
                rows
            })
            .collect();
        self.project_col_chars = self.widest_project_chain();
        self.display_gen = self.display_gen.wrapping_add(1);
    }

    /// The widest project chain of the display rows, in chars, as
    /// `project_column` renders it with every segment visible: the names
    /// joined by ` → `, and no separator after the last name. The cap keeps
    /// one deep project from eating the panel.
    fn widest_project_chain(&self) -> usize {
        const MAX_PROJECT_COL_CHARS: usize = 40;
        let mut widest = 0;
        for idx in 0..self.display_rows.len() {
            let entry = &self.store.entries[self.row_entry_idx(idx)];
            if entry.project_roots.is_empty() {
                continue;
            }
            let chars: usize = entry
                .project_roots
                .iter()
                .map(|root| {
                    let name = match self.store.project_names.get(root) {
                        Some(n) => n.chars().count(),
                        None => root
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("?")
                            .chars()
                            .count(),
                    };
                    name + 3
                })
                .sum();
            // The last name carries no ` → ` separator.
            widest = widest.max(chars.saturating_sub(3).max(crate::ui::PROJECT_MIN_CHARS));
        }
        widest.min(MAX_PROJECT_COL_CHARS)
    }

    /// Width of the project column for one row whose own chain is
    /// `own_chars` wide. With the align setting on, every row uses the one
    /// width; otherwise each row uses its own. 0 means the panel draws no
    /// project column for that row.
    pub fn project_col_width(&self, own_chars: usize) -> usize {
        if !self.settings.show_project {
            0
        } else if self.settings.align_project {
            self.project_col_chars
        } else {
            own_chars
        }
    }

    pub fn refilter(&mut self) {
        self.recompute_files();
        self.recompute_prompt_ordinals();
        self.recompute_agents();
        let filtered = self.filtered_indices();
        self.groups = Self::build_groups(&self.store.entries, &filtered);
        self.sel_group = 0;
        self.sel_child = 0;
        self.history_scroll = 0;
        self.diff_scroll = 0;
        if !self.groups.is_empty() && self.effective_auto_expand() {
            self.groups[0].expanded = true;
        }
        self.rebuild_display();
        self.refresh_diff();
        // Project filter changed — search results no longer reflect the visible set.
        self.search_clear();
    }

    pub fn refresh_diff(&mut self) {
        // The panel is about to show other text, so any selection that names
        // the old text stops being valid.
        self.diff_generation = self.diff_generation.wrapping_add(1);
        self.diff_lines = self.build_diff_lines();
        self.after_diff_lines_changed();
    }

    /// The lines the Diff panel shows for the current selection. `refresh_diff`
    /// is the only caller: it stores the result and then runs
    /// `after_diff_lines_changed` on every path.
    fn build_diff_lines(&self) -> Vec<DiffLine> {
        if self.groups.is_empty() {
            return vec![DiffLine::Header("(no history)".to_string())];
        }
        let Some(group) = self.groups.get(self.sel_group) else {
            return vec![DiffLine::Header("(no selection)".to_string())];
        };

        // Collapsed multi-entry group → concatenate every child's diff,
        // separated by per-child header rows.
        if !self.effective_auto_expand() && group.entry_indices.len() > 1 {
            let mut lines: Vec<DiffLine> = Vec::new();
            for (i, &entry_idx) in group.entry_indices.iter().enumerate() {
                let entry = &self.store.entries[entry_idx];
                let prev = self.store.prev_snapshot(entry_idx);
                if i > 0 {
                    lines.push(DiffLine::Header(String::new()));
                }
                lines.push(DiffLine::ChildSeparator { entry_idx });
                lines.extend(build_diff(prev, &entry.snapshot_path));
            }
            return lines;
        }

        let Some(&entry_idx) = group.entry_indices.get(self.sel_child) else {
            return vec![DiffLine::Header("(no selection)".to_string())];
        };
        let entry = &self.store.entries[entry_idx];
        if let Some(text) = entry.prompt_text() {
            let mut lines: Vec<DiffLine> = text
                .lines()
                .map(|l| DiffLine::Prompt(l.to_string()))
                .collect();
            if lines.is_empty() {
                lines.push(DiffLine::Prompt(String::new()));
            }
            // The response follows the prompt after one blank line.
            if let Some(response) = entry.response_text() {
                lines.push(DiffLine::Context(String::new()));
                lines.extend(response.lines().map(|l| DiffLine::Response(l.to_string())));
            }
            return lines;
        }
        let prev = self.store.prev_snapshot(entry_idx);
        build_diff(prev, &entry.snapshot_path)
    }

    /// Hook every `refresh_diff` runs on its way out: rebuild any Diff-panel
    /// `/` search matches for the new `diff_lines`, and, when the History
    /// panel's content (`f`) search is active, scroll the Diff so the first
    /// matching line is on top. Centralizing this here means every code path
    /// that lands a new diff in the panel (history navigation, search
    /// commit, n/N, mouse, refilter, reload) ends with the diff parked on
    /// the match — so the user never sees an entry that "matched" but a
    /// blank-looking diff.
    fn after_diff_lines_changed(&mut self) {
        if self.diff_search.phase != SearchPhase::Inactive {
            self.recompute_diff_matches();
            self.jump_to_current_diff_match();
        }
        if self.search.phase != SearchPhase::Inactive && self.search.highlights {
            self.reveal_diff_content_match();
        }
    }

    /// Scroll the Diff panel onto the first line that the History panel's
    /// content (`f`) search matches. Does nothing for the other search kinds.
    fn reveal_diff_content_match(&mut self) {
        if self.search.kind != SearchKind::Content {
            return;
        }
        let Some(re) = self.search.content_regex.clone() else {
            return;
        };
        let Some(line) = self.first_diff_line_matching(&re) else {
            return;
        };
        self.reveal_diff_line(line);
    }

    /// Store the height of the Diff panel that the last frame drew. The main
    /// loop calls this once per frame, before it draws.
    pub fn set_diff_height(&mut self, height: usize) {
        self.last_diff_height = height;
    }

    /// Scroll the Diff panel so line `line` and the `REVEAL_CONTEXT` lines
    /// after it are visible. If that move scrolls more than one page, put the
    /// line in the vertical middle instead. This is the rule that
    /// `reveal_selection` applies to the History panel.
    fn reveal_diff_line(&mut self, line: usize) {
        let last = self.diff_lines.len().saturating_sub(1);
        let visible = self.last_diff_height;
        if visible == 0 {
            self.diff_scroll = line.min(last);
            return;
        }
        let (lo, hi) = (line, (line + REVEAL_CONTEXT).min(last));

        let old = self.diff_scroll;
        let mut new = old;
        if lo < new {
            new = lo;
        } else if hi >= new + visible {
            new = hi + 1 - visible;
        }
        if new.abs_diff(old) > visible {
            new = line.saturating_sub(visible / 2);
        }
        // The top of the window never passes the line it must reveal.
        self.diff_scroll = new.min(line);
    }

    /// Returns the display-row index of the current selection.
    pub fn sel_display_row(&self) -> usize {
        let on_group_row = !self.effective_auto_expand()
            || self
                .groups
                .get(self.sel_group)
                .map(|g| g.entry_indices.len() == 1)
                .unwrap_or(true);
        self.display_rows
            .iter()
            .position(|r| {
                if on_group_row {
                    matches!(r, DisplayRow::Group(gi) if *gi == self.sel_group)
                } else {
                    matches!(r, DisplayRow::Child { group_idx, child_local }
                        if *group_idx == self.sel_group && *child_local == self.sel_child)
                }
            })
            .unwrap_or(0)
    }

    /// Keep the selected display row within the `visible` lines below
    /// `history_scroll`. Accounts for variable row heights (a prompt row can
    /// span several lines), so a tall selection scrolls fully into view.
    fn adjust_scroll(&mut self, visible: usize) {
        // A refilter replaces `display_rows`, so the cached heights describe
        // the previous list. Refresh them first, or the loop below measures
        // the wrong rows and leaves the selected row off the screen.
        self.refresh_row_heights(visible);
        let row = self.sel_display_row();
        if row < self.history_scroll {
            self.history_scroll = row;
            return;
        }
        let lines = self.row_lines(visible);
        if lines == 0 {
            return;
        }
        // Advance the top of the window until the selected row fits in
        // `lines` lines. Bounded by `row` so we never scroll past it.
        while self.history_scroll < row {
            let used: usize = (self.history_scroll..=row).map(|i| self.row_height(i)).sum();
            if used <= lines {
                break;
            }
            self.history_scroll += 1;
        }
    }

    /// Scroll so the selected row and `REVEAL_CONTEXT` rows after it (`Down`)
    /// or before it (`Up`) are visible. If that move scrolls more than one
    /// page, put the selected row in the vertical middle instead. This copies
    /// how vim reveals the next or the previous search match.
    fn reveal_selection(&mut self, visible: usize, dir: RevealDir) {
        let lines = self.row_lines(visible);
        if lines == 0 {
            return;
        }
        let row = self.sel_display_row();
        let last = self.display_rows.len().saturating_sub(1);
        let (lo, hi) = match dir {
            RevealDir::Down => (row, (row + REVEAL_CONTEXT).min(last)),
            RevealDir::Up => (row.saturating_sub(REVEAL_CONTEXT), row),
        };

        let old = self.history_scroll;
        let mut new = old;
        if lo < new {
            new = lo;
        } else {
            // Advance the top of the window until [new..=hi] fits. `lo` bounds
            // the walk, so the selected row always stays in view.
            while new < lo {
                let used: usize = (new..=hi).map(|i| self.row_height(i)).sum();
                if used <= lines {
                    break;
                }
                new += 1;
            }
        }

        // Lines between the old and the new top of the window.
        let moved: usize = if new >= old {
            (old..new).map(|i| self.row_height(i)).sum()
        } else {
            (new..old).map(|i| self.row_height(i)).sum()
        };
        if moved > lines {
            new = self.centered_scroll(row, lines);
        }

        self.history_scroll = new;
        // A row taller than the window, or a centered target, can still fall
        // outside the window. `adjust_scroll` corrects it.
        self.adjust_scroll(visible);
    }

    /// Top-of-window row that puts row `row` in the vertical middle of
    /// `visible` lines.
    fn centered_scroll(&self, row: usize, visible: usize) -> usize {
        let half = visible / 2;
        let mut top = row;
        let mut above = 0usize;
        while top > 0 {
            let h = self.row_height(top - 1);
            if above + h > half {
                break;
            }
            above += h;
            top -= 1;
        }
        top
    }

    /// Scroll the History panel so the selected row sits near the vertical
    /// middle. The scroll never goes past the last row: if the rows after the
    /// selected row do not fill the lower half, the panel shows more rows
    /// before it instead of empty lines.
    fn center_history_selection(&mut self, visible: usize) {
        if visible == 0 || self.display_rows.is_empty() {
            return;
        }
        self.refresh_row_heights(visible);
        let row = self.sel_display_row();
        let lines = self.row_lines(visible);
        let top = self.centered_scroll(row, lines).min(self.max_history_scroll(lines));
        self.history_scroll = top;
        // A row taller than the window can still fall outside it.
        self.adjust_scroll(visible);
    }

    /// The largest top-of-window row that still fills `visible` lines with
    /// rows. Scrolling past it only adds empty lines at the bottom.
    fn max_history_scroll(&self, visible: usize) -> usize {
        let mut top = self.display_rows.len();
        let mut used = 0usize;
        while top > 0 {
            let h = self.row_height(top - 1);
            if used + h > visible {
                break;
            }
            used += h;
            top -= 1;
        }
        top
    }

    /// The number of History lines that show rows, of the `visible` lines
    /// of the panel. When the user turns the date row on, the panel keeps
    /// the top line for the sticky date row (see [`Self::sticky_date`]).
    /// The scroll code always keeps that line, so the value does not
    /// change when the top row changes.
    fn row_lines(&self, visible: usize) -> usize {
        visible.saturating_sub(self.settings.date_separator as usize)
    }

    /// The day that the sticky date row shows at the top of the History
    /// panel. It is the day of the top row, when the top row has no date
    /// row of its own. The value is `None` when the user turned the date row off.
    pub fn sticky_date(&self) -> Option<NaiveDate> {
        let top = self.history_scroll;
        if top >= self.display_rows.len() || self.row_starts_day(top) || !self.settings.date_separator {
            return None;
        }
        Some(self.store.entries[self.row_entry_idx(top)].timestamp.date_naive())
    }

    /// Visual height (terminal lines) of display row `idx`. Defaults to 1
    /// when heights haven't been computed yet.
    pub fn row_height(&self, idx: usize) -> usize {
        self.hist_row_heights.get(idx).copied().unwrap_or(1).max(1)
    }

    /// Entry index behind display row `idx` (the group's first entry for a
    /// Group row, or the specific child for a Child row).
    pub fn row_entry_idx(&self, idx: usize) -> usize {
        match &self.display_rows[idx] {
            DisplayRow::Group(gi) => self.groups[*gi].entry_indices[0],
            DisplayRow::Child { group_idx, child_local } => {
                self.groups[*group_idx].entry_indices[*child_local]
            }
        }
    }

    /// True if display row `idx` is the first row of its day, and the user
    /// turned the date row on. The History panel then draws one extra line
    /// above the row, and [`Self::recompute_row_heights`] adds that line to
    /// the height of the row.
    pub fn row_starts_day(&self, idx: usize) -> bool {
        if !self.settings.date_separator || idx >= self.display_rows.len() {
            return false;
        }
        let day = self.store.entries[self.row_entry_idx(idx)]
            .timestamp
            .date_naive();
        match idx.checked_sub(1) {
            None => true,
            Some(prev) => {
                self.store.entries[self.row_entry_idx(prev)]
                    .timestamp
                    .date_naive()
                    != day
            }
        }
    }

    /// Map a 0-based line offset below `history_scroll` to the display-row
    /// index that occupies it (accounting for multi-line prompt rows). Used
    /// to translate a mouse click into a selection.
    pub fn history_item_at_line(&self, line_offset: usize) -> Option<usize> {
        // The sticky date row is above the rows. A click on it selects no row.
        let line_offset = line_offset.checked_sub(self.sticky_date().is_some() as usize)?;
        let mut acc = 0usize;
        for idx in self.history_scroll..self.display_rows.len() {
            let h = self.row_height(idx);
            if line_offset < acc + h {
                return Some(idx);
            }
            acc += h;
        }
        None
    }

    /// Recompute `hist_row_heights` for the current `display_rows` given the
    /// History list's inner width and height. A prompt row wraps its prompt
    /// to up to `prompt_lines` lines, then its response to up to
    /// `prompt_lines` more; every other row is a single line.
    /// Recompute the row heights at the width of the last frame. Call this
    /// after a refilter, before any code that measures rows.
    fn refresh_row_heights(&mut self, list_height: usize) {
        if self.last_list_width == 0 {
            return;
        }
        self.recompute_row_heights(self.last_list_width, list_height);
    }

    pub fn recompute_row_heights(&mut self, list_width: usize, list_height: usize) {
        self.last_list_width = list_width;
        let ts_width = self.estimate_ts_width(list_height);
        let key = (
            list_width,
            ts_width,
            self.prompt_lines,
            self.display_gen,
            self.settings.cache_bits(),
        );
        if self.row_heights_key == Some(key)
            && self.hist_row_heights.len() == self.display_rows.len()
        {
            return;
        }
        let mut heights = Vec::with_capacity(self.display_rows.len());
        for (idx, row) in self.display_rows.iter().enumerate() {
            let h = match row {
                DisplayRow::Group(gi) => {
                    let g = &self.groups[*gi];
                    let entry = &self.store.entries[g.entry_indices[0]];
                    if g.entry_indices.len() == 1 && entry.is_prompt() {
                        self.prompt_row_height(entry, list_width, ts_width)
                    } else {
                        1
                    }
                }
                DisplayRow::Child { .. } => 1,
            };
            // The date row of a day sits on top of the first row of that day.
            heights.push(h + self.row_starts_day(idx) as usize);
        }
        self.hist_row_heights = heights;
        self.row_heights_key = Some(key);
    }

    /// Number of wrapped lines a prompt row occupies at the given panel
    /// geometry — mirrors the wrapping the renderer performs so scroll math
    /// and rendering agree.
    fn prompt_row_height(&self, entry: &HistoryEntry, list_width: usize, ts_width: usize) -> usize {
        let (chain, _) = entry.display_path_segments(&self.store.project_names);
        let own_chars = if chain.is_empty() {
            0
        } else {
            (chain.iter().map(|s| s.chars().count()).sum::<usize>() + 3 * (chain.len() - 1))
                .max(crate::ui::PROJECT_MIN_CHARS)
        };
        // The renderer adds a blank gap after the column.
        let width = self.project_col_width(own_chars);
        let chain_chars = if width == 0 {
            0
        } else {
            width + crate::ui::PROJECT_GAP
        };
        let ordinal = self.prompt_ordinal(entry);
        let text_start = crate::ui::history_prefix_cols(ts_width, list_width, &self.settings)
            + chain_chars
            + crate::ui::ordinal_prefix_width(ordinal);
        let avail = list_width
            .saturating_sub(text_start + crate::ui::RUNNING_RESERVE)
            .max(1);
        // The row height must match what the renderer draws. Highlighting
        // never changes the characters, so the plain wrap gives the same
        // count and costs far less over a long list.
        let raw = entry.prompt_text().unwrap_or("");
        crate::prompt::wrap_turn_plain(raw, entry.response_text(), avail, self.prompt_lines)
            .len()
            .max(1)
    }

    /// Estimate the timestamp-column width for the currently-visible window.
    /// Used only to size prompt wrapping; the renderer recomputes its own
    /// (near-identical) width, and any small drift is absorbed by the
    /// pad/truncate step when rows are drawn.
    fn estimate_ts_width(&self, list_height: usize) -> usize {
        if !self.settings.show_time {
            return 0;
        }
        if self.absolute_time {
            return "%Y-%m-%d %H:%M".len();
        }
        let tss: Vec<DateTime<Local>> = self
            .display_rows
            .iter()
            .enumerate()
            .skip(self.history_scroll)
            .take(list_height.max(1))
            .map(|(idx, _)| self.store.entries[self.row_entry_idx(idx)].timestamp)
            .collect();
        crate::history::align_relative_times(&tss)
            .first()
            .map(|s| s.chars().count())
            .unwrap_or(0)
    }

    pub fn inc_prompt_lines(&mut self) {
        self.prompt_lines = (self.prompt_lines + 1).min(MAX_PROMPT_LINES);
    }

    pub fn dec_prompt_lines(&mut self) {
        self.prompt_lines = self.prompt_lines.saturating_sub(1).max(MIN_PROMPT_LINES);
    }

    /// After mutating selection state from outside the App (e.g. mouse
    /// click), rebuild the display rows, refresh the diff, and keep the
    /// selection inside the visible window.
    pub fn rebuild_display_after_external_mutation(&mut self, visible: usize) {
        self.rebuild_display();
        self.diff_scroll = 0;
        self.adjust_scroll(visible);
        self.refresh_diff();
    }

    /// Epoch (0 = yellow, 1 = orange) per group, flipping on gaps > 1 hour.
    pub fn group_epochs(&self) -> Vec<u8> {
        let mut epochs = vec![0u8; self.groups.len()];
        let mut epoch = 0u8;
        for i in 1..self.groups.len() {
            let prev_oldest_idx = *self.groups[i - 1].entry_indices.last().unwrap();
            let cur_newest_idx = self.groups[i].entry_indices[0];
            let gap = self.store.entries[prev_oldest_idx]
                .timestamp
                .signed_duration_since(self.store.entries[cur_newest_idx].timestamp);
            if gap.num_minutes() > 60 {
                epoch = 1 - epoch;
            }
            epochs[i] = epoch;
        }
        epochs
    }

    /// Compute diff summaries for the entries currently visible in the
    /// History panel. Cached entries are not recomputed.
    pub fn ensure_summaries_visible(&mut self, visible: usize) {
        let end = (self.history_scroll + visible).min(self.display_rows.len());
        for row_idx in self.history_scroll..end {
            let entry_idx = match self.display_rows[row_idx] {
                DisplayRow::Group(gi) => self.groups[gi].entry_indices[0],
                DisplayRow::Child {
                    group_idx,
                    child_local,
                } => self.groups[group_idx].entry_indices[child_local],
            };
            if self.diff_summaries.contains_key(&entry_idx) {
                continue;
            }
            let entry = &self.store.entries[entry_idx];
            let prev = self.store.prev_snapshot(entry_idx);
            let summary = compute_summary(prev, &entry.snapshot_path);
            self.diff_summaries.insert(entry_idx, summary);
        }
    }

    pub fn current_entry(&self) -> Option<&HistoryEntry> {
        let group = self.groups.get(self.sel_group)?;
        let &idx = group.entry_indices.get(self.sel_child)?;
        self.store.entries.get(idx)
    }

    /// Index into `store.entries` of the current selection (parallels
    /// [`Self::current_entry`]).
    pub fn current_entry_idx(&self) -> Option<usize> {
        let group = self.groups.get(self.sel_group)?;
        group.entry_indices.get(self.sel_child).copied()
    }

    /// Re-scan the running-session registry if it hasn't been refreshed
    /// recently. Called once per frame from the event loop; the throttle keeps
    /// the cost (a few small JSON reads) off the hot path while keeping the
    /// running indicator live as sessions come and go.
    pub fn maybe_rescan_running(&mut self) {
        const REFRESH: std::time::Duration = std::time::Duration::from_secs(2);
        let due = self
            .running_scanned_at
            .map(|t| t.elapsed() >= REFRESH)
            .unwrap_or(true);
        if due {
            self.running = crate::session::RunningSessions::scan();
            self.running_scanned_at = Some(Instant::now());
        }
    }

    /// Whether the agent session behind `entry_idx` still runs. Only Claude
    /// Code publishes a session registry, so every other source reports
    /// [`RunState::Unknown`] and the renderer leaves its colors bright.
    pub fn entry_run_state(&self, entry_idx: usize) -> RunState {
        let Some(entry) = self.store.entries.get(entry_idx) else {
            return RunState::Unknown;
        };
        self.run_state_of(entry.source, entry.session_id.as_deref())
    }

    /// [`Self::entry_run_state`] for a source and session id that the caller
    /// already holds (the Agents panel has no entry index).
    pub fn run_state_of(&self, source: EditSource, session_id: Option<&str>) -> RunState {
        if source != EditSource::ClaudeCode {
            return RunState::Unknown;
        }
        match session_id {
            None => RunState::Unknown,
            Some(sid) if self.running.is_running(sid) => RunState::Running,
            Some(_) => RunState::Stopped,
        }
    }

    /// Whether the entry at `entry_idx` originated from a Claude Code session
    /// that is currently running.
    pub fn is_entry_running(&self, entry_idx: usize) -> bool {
        self.store
            .entries
            .get(entry_idx)
            .and_then(|e| e.session_id.as_deref())
            .map(|sid| self.running.is_running(sid))
            .unwrap_or(false)
    }

    /// Absolute path for the currently selected row in the active panel.
    /// Returns None when nothing is selected, or the active panel is Diff.
    pub fn selected_path(&self) -> Option<PathBuf> {
        match self.active_panel {
            Panel::Projects => self
                .selected_project_idx
                .and_then(|i| self.store.projects.get(i).cloned()),
            Panel::Files => self.selected_file_path.clone(),
            Panel::Agents => self
                .sel_agent_idx()
                .and_then(|i| self.agents[i].project_roots.last().cloned()),
            Panel::History => self.current_entry().map(|e| e.file_path.clone()),
            // The Diff panel shows the selected History row, so `y` there
            // copies that row's path.
            Panel::Diff => self.current_entry().map(|e| e.file_path.clone()),
        }
    }

    /// The text that `y` copies, with the noun the status bar shows for it.
    /// A prompt row has no real file: its `file_path` is a synthetic key, so
    /// `y` copies the prompt text instead.
    pub fn copy_target(&self) -> Option<(String, &'static str)> {
        if matches!(self.active_panel, Panel::History | Panel::Diff)
            && let Some(text) = self.current_entry().and_then(|e| e.prompt_text())
        {
            return Some((text.to_string(), "prompt"));
        }
        self.selected_path()
            .map(|p| (p.to_string_lossy().into_owned(), "path"))
    }

    /// The text that `Y` copies: the response of the selected prompt. The
    /// Diff panel is the only panel that shows a response, so the other
    /// panels return None.
    pub fn copy_response_target(&self) -> Option<String> {
        if self.active_panel != Panel::Diff {
            return None;
        }
        self.current_entry()
            .and_then(|e| e.response_text())
            .map(|t| t.to_string())
    }

    /// True if `O` has a file to open. A prompt row has none.
    pub fn can_open_file(&self) -> bool {
        self.current_entry().is_some_and(|e| !e.is_prompt())
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    pub fn history_down(&mut self, visible: usize) {
        if self.groups.is_empty() {
            return;
        }
        let group_len = self.groups[self.sel_group].entry_indices.len();
        let auto_expand = self.effective_auto_expand();
        if auto_expand && self.sel_child + 1 < group_len {
            self.sel_child += 1;
        } else if self.sel_group + 1 < self.groups.len() {
            let leaving = self.sel_group;
            self.sel_group += 1;
            if auto_expand {
                self.groups[leaving].expanded = false;
                self.groups[self.sel_group].expanded = true;
            }
            self.rebuild_display();
            self.sel_child = 0;
        } else {
            return;
        }
        self.diff_scroll = 0;
        self.adjust_scroll(visible);
        self.refresh_diff();
    }

    pub fn history_up(&mut self, visible: usize) {
        if self.groups.is_empty() {
            return;
        }
        let auto_expand = self.effective_auto_expand();
        if auto_expand && self.sel_child > 0 {
            self.sel_child -= 1;
        } else if self.sel_group > 0 {
            let leaving = self.sel_group;
            self.sel_group -= 1;
            if auto_expand {
                self.groups[leaving].expanded = false;
                self.groups[self.sel_group].expanded = true;
                self.sel_child = self.groups[self.sel_group].entry_indices.len() - 1;
            } else {
                self.sel_child = 0;
            }
            self.rebuild_display();
        } else {
            return;
        }
        self.diff_scroll = 0;
        self.adjust_scroll(visible);
        self.refresh_diff();
    }

    pub fn history_page_down(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        for _ in 0..steps {
            let before = (self.sel_group, self.sel_child);
            self.history_down(visible);
            if (self.sel_group, self.sel_child) == before {
                break;
            }
        }
    }

    pub fn history_page_up(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        for _ in 0..steps {
            let before = (self.sel_group, self.sel_child);
            self.history_up(visible);
            if (self.sel_group, self.sel_child) == before {
                break;
            }
        }
    }

    // ── Diff panel text selection ─────────────────────────────────────────

    /// Start a selection at the cell `(row, col)` of the Diff panel's inner
    /// rect. The caller maps the mouse position to that cell.
    pub fn diff_select_start(&mut self, row: usize, col: usize) {
        self.diff_selection = Some(DiffSelection {
            anchor: (row, col),
            cursor: (row, col),
            scroll: self.diff_scroll,
            generation: self.diff_generation,
        });
    }

    /// Move the open end of the selection to the cell `(row, col)`. No-op
    /// without a live selection.
    pub fn diff_select_to(&mut self, row: usize, col: usize) {
        if self.diff_selection_live().is_none() {
            self.diff_selection = None;
            return;
        }
        if let Some(sel) = self.diff_selection.as_mut() {
            sel.cursor = (row, col);
        }
    }

    /// Drop the selection. The Diff panel then draws no highlight.
    pub fn diff_select_clear(&mut self) {
        self.diff_selection = None;
    }

    /// The selection, if it still describes the text the panel shows now.
    fn diff_selection_live(&self) -> Option<&DiffSelection> {
        let sel = self.diff_selection.as_ref()?;
        (sel.scroll == self.diff_scroll && sel.generation == self.diff_generation).then_some(sel)
    }

    /// The two ends of the selection in reading order: `(start, end)`, each
    /// a `(row, col)` cell. `end` names the cell after the last selected
    /// character. Returns None when no selection describes the current text,
    /// and also when the two ends name the same cell: a plain click must
    /// neither paint a highlight nor replace the clipboard.
    pub fn diff_selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let sel = self.diff_selection_live().filter(|s| s.anchor != s.cursor)?;
        // The end of the range covers the cell under the pointer, so the
        // character the user points at belongs to the selection.
        let (a, b) = (sel.anchor, sel.cursor);
        Some(if a <= b {
            (a, (b.0, b.1 + 1))
        } else {
            (b, (a.0, a.1 + 1))
        })
    }

    pub fn diff_page_down(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        let max = self.diff_lines.len().saturating_sub(1);
        self.diff_scroll = (self.diff_scroll + steps).min(max);
    }

    pub fn diff_page_up(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        self.diff_scroll = self.diff_scroll.saturating_sub(steps);
    }

    pub fn diff_down(&mut self) {
        if self.diff_scroll + 1 < self.diff_lines.len() {
            self.diff_scroll += 1;
        }
    }

    pub fn diff_up(&mut self) {
        if self.diff_scroll > 0 {
            self.diff_scroll -= 1;
        }
    }

    pub fn project_down(&mut self, visible: usize) {
        let max = self.store.projects.len();
        let current = self.selected_project_idx.map(|i| i + 1).unwrap_or(0);
        if current < max {
            self.selected_project_idx = Some(current);
            let display_idx = current + 1; // +1 for "All" row
            if display_idx >= self.project_scroll + visible {
                self.project_scroll += 1;
            }
            // Different project = different files; drop any file-panel search
            // before refilter rebuilds the files list.
            self.file_search.clear();
            self.refilter();
        }
    }

    pub fn project_up(&mut self, _visible: usize) {
        match self.selected_project_idx {
            None => {}
            Some(0) => {
                self.selected_project_idx = None;
                self.project_scroll = self.project_scroll.saturating_sub(1);
                self.file_search.clear();
                self.refilter();
            }
            Some(i) => {
                self.selected_project_idx = Some(i - 1);
                let display_idx = i; // i-1 project = display row i (0 = "All")
                if display_idx < self.project_scroll {
                    self.project_scroll = display_idx;
                }
                self.file_search.clear();
                self.refilter();
            }
        }
    }

    pub fn project_page_down(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        for _ in 0..steps {
            let before = self.selected_project_idx;
            self.project_down(visible);
            if self.selected_project_idx == before {
                break;
            }
        }
    }

    pub fn project_page_up(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        for _ in 0..steps {
            let before = self.selected_project_idx;
            self.project_up(visible);
            if self.selected_project_idx == before {
                break;
            }
        }
    }

    pub fn history_top(&mut self, visible: usize) {
        if self.groups.is_empty() {
            return;
        }
        // Collapse whichever group currently holds the cursor.
        if let Some(g) = self.groups.get_mut(self.sel_group) {
            g.expanded = false;
        }
        self.sel_group = 0;
        self.sel_child = 0;
        if self.effective_auto_expand() {
            self.groups[0].expanded = true;
        }
        self.rebuild_display();
        self.diff_scroll = 0;
        self.history_scroll = 0;
        self.adjust_scroll(visible);
        self.refresh_diff();
    }

    pub fn history_bottom(&mut self, visible: usize) {
        if self.groups.is_empty() {
            return;
        }
        if let Some(g) = self.groups.get_mut(self.sel_group) {
            g.expanded = false;
        }
        let last = self.groups.len() - 1;
        self.sel_group = last;
        if self.effective_auto_expand() {
            self.groups[last].expanded = true;
            self.sel_child = self.groups[last].entry_indices.len() - 1;
        } else {
            self.sel_child = 0;
        }
        self.rebuild_display();
        self.diff_scroll = 0;
        self.adjust_scroll(visible);
        self.refresh_diff();
    }

    pub fn project_top(&mut self) {
        self.selected_project_idx = None;
        self.project_scroll = 0;
        self.file_search.clear();
        self.refilter();
    }

    pub fn project_bottom(&mut self, visible: usize) {
        let n = self.store.projects.len();
        if n == 0 {
            return;
        }
        self.selected_project_idx = Some(n - 1);
        let total_rows = n + 1;
        self.project_scroll = total_rows.saturating_sub(visible);
        self.file_search.clear();
        self.refilter();
    }

    // ── Files panel navigation ───────────────────────────────────────────────
    //
    // Mirrors Projects. Display index 0 = "All files" (None);
    // display index 1+ = self.files[i-1].

    /// Position in the file list of the currently selected file (or None
    /// if "All files" is selected). Path-based selection drifts as the list
    /// re-sorts on each batch — recompute it on demand.
    fn sel_file_idx(&self) -> Option<usize> {
        let f = self.selected_file_path.as_ref()?;
        self.files.iter().position(|p| p == f)
    }

    pub fn file_down(&mut self, visible: usize) {
        let max = self.files.len();
        let current = self.sel_file_idx().map(|i| i + 1).unwrap_or(0);
        if current < max {
            self.selected_file_path = Some(self.files[current].clone());
            let display_idx = current + 1; // +1 for "All files" row
            if display_idx >= self.file_scroll + visible {
                self.file_scroll += 1;
            }
            self.refilter();
        }
    }

    pub fn file_up(&mut self, _visible: usize) {
        match self.sel_file_idx() {
            None => {}
            Some(0) => {
                self.selected_file_path = None;
                self.file_scroll = self.file_scroll.saturating_sub(1);
                self.refilter();
            }
            Some(i) => {
                self.selected_file_path = Some(self.files[i - 1].clone());
                let display_idx = i; // i-1 file = display row i (0 = "All files")
                if display_idx < self.file_scroll {
                    self.file_scroll = display_idx;
                }
                self.refilter();
            }
        }
    }

    pub fn file_page_down(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        for _ in 0..steps {
            let before = self.selected_file_path.clone();
            self.file_down(visible);
            if self.selected_file_path == before {
                break;
            }
        }
    }

    pub fn file_page_up(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        for _ in 0..steps {
            let before = self.selected_file_path.clone();
            self.file_up(visible);
            if self.selected_file_path == before {
                break;
            }
        }
    }

    pub fn file_top(&mut self) {
        self.selected_file_path = None;
        self.file_scroll = 0;
        self.refilter();
    }

    pub fn file_bottom(&mut self, visible: usize) {
        let n = self.files.len();
        if n == 0 {
            return;
        }
        self.selected_file_path = Some(self.files[n - 1].clone());
        let total_rows = n + 1;
        self.file_scroll = total_rows.saturating_sub(visible);
        self.refilter();
    }

    pub fn diff_top(&mut self) {
        self.diff_scroll = 0;
    }

    pub fn diff_bottom(&mut self, visible: usize) {
        self.diff_scroll = self
            .diff_lines
            .len()
            .saturating_sub(visible.max(1));
    }

    /// The panels of the current mode, left to right. Agent mode inserts the
    /// Agents panel between Files and History.
    pub fn panel_order(&self) -> &'static [Panel] {
        match self.mode {
            Mode::File => &[Panel::Projects, Panel::Files, Panel::History, Panel::Diff],
            Mode::Agent => &[
                Panel::Projects,
                Panel::Files,
                Panel::Agents,
                Panel::History,
                Panel::Diff,
            ],
        }
    }

    fn panel_pos(&self) -> usize {
        self.panel_order()
            .iter()
            .position(|p| *p == self.active_panel)
            .unwrap_or(0)
    }

    pub fn toggle_panel(&mut self) {
        let order = self.panel_order();
        self.active_panel = order[(self.panel_pos() + 1) % order.len()];
    }

    /// Rotate focus the other direction (Shift+Tab).
    pub fn toggle_panel_backward(&mut self) {
        let order = self.panel_order();
        let pos = self.panel_pos();
        self.active_panel = order[(pos + order.len() - 1) % order.len()];
    }

    /// Move focus one panel right. No wrap-around.
    pub fn focus_right(&mut self) {
        let order = self.panel_order();
        let pos = (self.panel_pos() + 1).min(order.len() - 1);
        self.active_panel = order[pos];
    }

    /// Move focus one panel left. No wrap-around.
    pub fn focus_left(&mut self) {
        let order = self.panel_order();
        let pos = self.panel_pos().saturating_sub(1);
        self.active_panel = order[pos];
    }

    /// Switch between File mode and Agent mode. The switch keeps the History
    /// panel active if it was active before, because the user reads the rows
    /// there. From any other panel, Agent mode starts on the Agents panel,
    /// because the selection there drives every panel to its right. Leaving
    /// Agent mode drops the agent filter.
    pub fn toggle_mode(&mut self, visible: usize, visible_agents: usize) {
        // The row under the cursor right now. Both directions keep the user
        // on it, so the toggle never moves them away from their work.
        let current = self.current_entry_id();
        let current_session = self
            .current_entry()
            .and_then(|e| e.session_id.clone());

        // The row to select after the switch.
        let target = match self.mode {
            Mode::File => {
                // The selected row names its agent, so select that agent. A
                // row without a session (a Neovim edit, an unattributed
                // Cursor prompt) names none: take the agent of the nearest
                // row that has one, so the Agents panel still follows the
                // cursor. `file_row` brings the user back either way.
                match current_session.or_else(|| self.nearest_session()) {
                    Some(sid) => {
                        self.selected_agent = Some(sid);
                        self.pending_agent_restore = None;
                        current.clone()
                    }
                    None => None,
                }
            }
            Mode::Agent => {
                // Return to the remembered File-mode row, unless the user
                // moved the History cursor while in Agent mode.
                if current == self.mode_return.agent_row {
                    self.mode_return.file_row.clone()
                } else {
                    current.clone()
                }
            }
        };

        let leaving_file = self.mode == Mode::File;
        let stay_on_history = self.active_panel == Panel::History;
        self.mode = self.mode.other();
        self.active_panel = match self.mode {
            Mode::Agent if stay_on_history => Panel::History,
            Mode::Agent => Panel::Agents,
            Mode::File => Panel::History,
        };
        // The filtered set changes, so any search result now points at rows
        // the panel no longer lists.
        self.search_clear();
        self.refilter_preserving(target, visible);
        // The new filter list has a different length, so the old scroll
        // offset points at the wrong place. Put the selected row near the
        // middle, which shows the rows before it and after it.
        self.center_history_selection(visible);
        self.ensure_agent_visible(visible_agents);

        self.mode_return = if leaving_file {
            ModeReturn {
                file_row: current,
                agent_row: self.current_entry_id(),
            }
        } else {
            ModeReturn::default()
        };
    }

    /// The session of the History row closest to the cursor that has one.
    /// The search starts at the selected group and moves outward, newer row
    /// first. Returns None if no visible row carries a session.
    fn nearest_session(&self) -> Option<String> {
        let n = self.groups.len();
        for d in 0..n {
            for gi in [self.sel_group.checked_sub(d), Some(self.sel_group + d)]
                .into_iter()
                .flatten()
                .take(if d == 0 { 1 } else { 2 })
            {
                let Some(group) = self.groups.get(gi) else {
                    continue;
                };
                let found = group
                    .entry_indices
                    .iter()
                    .find_map(|&ei| self.store.entries[ei].session_id.clone());
                if found.is_some() {
                    return found;
                }
            }
        }
        None
    }

    /// Identity of the selected History row. See [`EntryId`].
    pub fn current_entry_id(&self) -> Option<EntryId> {
        self.current_entry()
            .map(|e| (e.file_path.clone(), e.timestamp))
    }

    /// Scroll the Agents panel so the selected agent is on screen.
    fn ensure_agent_visible(&mut self, visible: usize) {
        let Some(i) = self.sel_agent_idx() else {
            return;
        };
        if i < self.agent_scroll {
            self.agent_scroll = i;
        } else if visible > 0 && i >= self.agent_scroll + visible {
            self.agent_scroll = i + 1 - visible;
        }
    }

    // ── Agents panel navigation ───────────────────────────────────────────

    /// Index of the selected agent in `agents`.
    pub fn sel_agent_idx(&self) -> Option<usize> {
        let id = self.selected_agent.as_deref()?;
        self.agents.iter().position(|a| a.session_id == id)
    }

    /// Select the agent at row `target`, then rebuild History for it.
    pub fn select_agent_row(&mut self, target: usize, visible: usize) {
        let Some(agent) = self.agents.get(target) else {
            return;
        };
        let id = agent.session_id.clone();
        if self.selected_agent.as_deref() == Some(id.as_str()) {
            return;
        }
        self.selected_agent = Some(id);
        if target < self.agent_scroll {
            self.agent_scroll = target;
        } else if visible > 0 && target >= self.agent_scroll + visible {
            self.agent_scroll = target + 1 - visible;
        }
        self.refilter();
        self.adjust_scroll(visible);
    }

    pub fn agent_down(&mut self, visible: usize) {
        let Some(i) = self.sel_agent_idx() else {
            return;
        };
        if i + 1 < self.agents.len() {
            self.select_agent_row(i + 1, visible);
        }
    }

    pub fn agent_up(&mut self, visible: usize) {
        let Some(i) = self.sel_agent_idx() else {
            return;
        };
        if i > 0 {
            self.select_agent_row(i - 1, visible);
        }
    }

    pub fn agent_page_down(&mut self, visible: usize) {
        let Some(i) = self.sel_agent_idx() else {
            return;
        };
        let target = (i + visible.max(1)).min(self.agents.len().saturating_sub(1));
        self.select_agent_row(target, visible);
    }

    pub fn agent_page_up(&mut self, visible: usize) {
        let Some(i) = self.sel_agent_idx() else {
            return;
        };
        self.select_agent_row(i.saturating_sub(visible.max(1)), visible);
    }

    pub fn agent_top(&mut self, visible: usize) {
        self.select_agent_row(0, visible);
        self.agent_scroll = 0;
    }

    pub fn agent_bottom(&mut self, visible: usize) {
        if self.agents.is_empty() {
            return;
        }
        self.select_agent_row(self.agents.len() - 1, visible);
    }

    /// Move to the next entry-kind filter. This keeps the selection and the
    /// active search, so the user can narrow a result list of many file
    /// changes down to the one prompt inside it.
    pub fn cycle_kind_filter(&mut self, visible: usize) {
        self.kind_filter = self.kind_filter.next();
        let selection = self
            .current_entry()
            .map(|e| (e.file_path.clone(), e.timestamp));
        self.refilter_preserving(selection, visible);
        // The matches describe the set from before the change. A semantic
        // search embeds its query again here, which costs one HTTP round trip.
        if self.search.phase != SearchPhase::Inactive {
            self.recompute_matches(visible);
        }
    }

    pub fn toggle_time(&mut self) {
        self.absolute_time = !self.absolute_time;
    }

    /// Open or close the settings menu (the `S` key).
    pub fn toggle_settings_menu(&mut self) {
        self.settings_menu.open = !self.settings_menu.open;
    }

    /// Turn the selected setting on or off. The column set and the date row
    /// change the height of a History row, so refresh the heights and pull
    /// the selected row back into view.
    pub fn toggle_selected_setting(&mut self, visible: usize) {
        let item = self.settings_menu.item();
        self.settings.toggle(item);
        self.adjust_scroll(visible);
    }

    /// Resolved auto-expand: true if either the user has toggled it on, or
    /// the file filter is active. Filtering to a single file collapses the
    /// History into one group containing every edit, so we force expand to
    /// keep the per-edit rows reachable.
    pub fn effective_auto_expand(&self) -> bool {
        self.auto_expand || self.selected_file_path.is_some()
    }

    /// Whether the `x` keybinding has an effect right now. Disabled while
    /// the file filter is active (auto-expand is forced on regardless).
    pub fn can_toggle_auto_expand(&self) -> bool {
        self.selected_file_path.is_none()
    }

    /// Flip auto-expand mode. When turning it off, fold every group so the
    /// display shows one row per group. When turning it on, expand whichever
    /// group currently holds the cursor (matching navigation invariants).
    /// No-op while a file is selected — expansion is forced on in that mode.
    pub fn toggle_auto_expand(&mut self, visible: usize) {
        if !self.can_toggle_auto_expand() {
            return;
        }
        self.auto_expand = !self.auto_expand;
        if self.auto_expand {
            for g in &mut self.groups {
                g.expanded = false;
            }
            if let Some(g) = self.groups.get_mut(self.sel_group) {
                g.expanded = true;
            }
            // sel_child was clamped to 0 while collapsed; leave it there.
        } else {
            for g in &mut self.groups {
                g.expanded = false;
            }
            self.sel_child = 0;
        }
        self.rebuild_display();
        self.diff_scroll = 0;
        self.adjust_scroll(visible);
        self.refresh_diff();
    }

    pub fn project_label(path: &PathBuf) -> String {
        let parts: Vec<_> = path.components().collect();
        let start = parts.len().saturating_sub(3);
        parts[start..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }

    // ── Search ────────────────────────────────────────────────────────────────

    pub fn search_open(&mut self) {
        self.search.phase = SearchPhase::Typing;
        self.search.kind = SearchKind::Path;
        self.search.restore = Some((self.sel_group, self.sel_child));
        self.search_history.get_mut(self.history_search_mode()).reset_walk();
        self.search.query.clear();
        self.search.matches.clear();
        self.search.current = 0;
        self.search.highlights = true;
        self.search.content_regex = None;
        self.search.pending_since = None;
    }

    /// Like `search_open` but starts the prompt in content-search mode:
    /// the query is treated as a case-insensitive regex and applied to each
    /// entry's diff body / prompt text rather than its display path.
    pub fn search_open_content(&mut self) {
        self.search.phase = SearchPhase::Typing;
        self.search.kind = SearchKind::Content;
        self.search.restore = Some((self.sel_group, self.sel_child));
        self.search_history.get_mut(self.history_search_mode()).reset_walk();
        self.search.query.clear();
        self.search.matches.clear();
        self.search.current = 0;
        self.search.highlights = true;
        self.search.content_regex = None;
        self.search.pending_since = None;
    }

    pub fn search_push_char(&mut self, c: char, visible: usize) {
        self.search.query.push(c);
        self.search_history.get_mut(self.history_search_mode()).reset_walk();
        self.search_query_changed(visible);
    }

    pub fn search_pop_char(&mut self, visible: usize) {
        self.search.query.pop();
        self.search_history.get_mut(self.history_search_mode()).reset_walk();
        self.search_query_changed(visible);
    }

    /// The query list that the History prompt walks. Each corpus keeps its
    /// own list, because a path query and a regex are not interchangeable.
    pub fn history_search_mode(&self) -> SearchMode {
        match self.search.kind {
            SearchKind::Path => SearchMode::Path,
            SearchKind::Content => SearchMode::Content,
            SearchKind::Semantic => SearchMode::Semantic,
        }
    }

    /// Put the next older remembered query on the History prompt (Up).
    pub fn search_history_older(&mut self, visible: usize) {
        let mode = self.history_search_mode();
        let current = self.search.query.clone();
        let Some(query) = self.search_history.get_mut(mode).older(&current) else {
            return;
        };
        self.search.query = query;
        self.search_query_changed(visible);
    }

    /// Put the next newer remembered query on the History prompt (Down). At
    /// the end of the walk the prompt shows the text the user typed first.
    pub fn search_history_newer(&mut self, visible: usize) {
        let mode = self.history_search_mode();
        let Some(query) = self.search_history.get_mut(mode).newer() else {
            return;
        };
        self.search.query = query;
        self.search_query_changed(visible);
    }

    /// Route a query edit to the matcher. Path search is a scan of strings
    /// already in memory, so it runs on the keystroke. Content search reads and
    /// diffs snapshots off disk, so it's deferred until typing pauses — the
    /// previous query's matches stay on screen (at most `SEARCH_DEBOUNCE` stale)
    /// with the prompt marking the scan as in flight. Queries too short to
    /// search run immediately: `recompute_matches` only clears, so there's
    /// nothing to defer.
    fn search_query_changed(&mut self, visible: usize) {
        // A semantic search needs an HTTP round trip and a full rank, so it
        // waits for Enter. `matches` stays empty while the user types.
        if self.search.kind == SearchKind::Semantic {
            return;
        }
        if self.search.kind == SearchKind::Content
            && self.search.query.chars().count() >= CONTENT_MIN_CHARS
        {
            self.search.pending_since = Some(Instant::now());
            return;
        }
        self.recompute_matches(visible);
    }

    /// Run a deferred content search once typing has paused for
    /// `SEARCH_DEBOUNCE`. Called from the event loop each frame.
    pub fn maybe_run_pending_search(&mut self, visible: usize) {
        let Some(since) = self.search.pending_since else {
            return;
        };
        if since.elapsed() >= SEARCH_DEBOUNCE {
            self.recompute_matches(visible);
        }
    }

    pub fn search_commit(&mut self, visible: usize) {
        let query = self.search.query.clone();
        self.search_history.get_mut(self.history_search_mode()).push(&query);
        // Enter shouldn't wait out the debounce: scan now so the committed
        // matches reflect the query on screen rather than the previous one.
        // A semantic search runs here and nowhere else.
        if self.search.pending_since.is_some() || self.search.kind == SearchKind::Semantic {
            self.recompute_matches(visible);
        }
        if self.search.matches.is_empty() {
            // Nothing to keep highlighted — fall back to clear.
            self.search_clear();
            return;
        }
        self.search.phase = SearchPhase::Active;
        self.search.highlights = true;
        self.search.restore = None;
        self.search.last_query = self.search.query.clone();
        self.search.last_kind = self.search.kind;
    }

    /// Vim's `:noh` — hide highlights but keep the query/matches so n/N can
    /// resume. Idempotent: calling on an empty search is a no-op.
    pub fn search_hide(&mut self) {
        if self.search.phase == SearchPhase::Active {
            self.search.highlights = false;
        }
    }

    /// Cancel an in-progress search: discard query, clear matches, restore
    /// the selection that was current when search began.
    pub fn search_cancel(&mut self, visible: usize) {
        // The user ran this query, so remember it even though Esc drops the
        // matches. The list holds the query once, whatever ends the prompt.
        let query = self.search.query.clone();
        self.search_history.get_mut(self.history_search_mode()).push(&query);
        if let Some((g, c)) = self.search.restore.take() {
            self.sel_group = g.min(self.groups.len().saturating_sub(1));
            self.sel_child = c;
            if let Some(group) = self.groups.get(self.sel_group) {
                if self.sel_child >= group.entry_indices.len() {
                    self.sel_child = group.entry_indices.len().saturating_sub(1);
                }
            }
            self.adjust_scroll(visible);
            self.refresh_diff();
        }
        self.search.phase = SearchPhase::Inactive;
        self.search.kind = SearchKind::Path;
        self.search.query.clear();
        self.search.matches.clear();
        self.search.current = 0;
        self.search.content_regex = None;
        self.search.pending_since = None;
    }

    /// Drop highlights but keep the selection where it is.
    pub fn search_clear(&mut self) {
        self.search.phase = SearchPhase::Inactive;
        self.search.kind = SearchKind::Path;
        self.search.query.clear();
        self.search.matches.clear();
        self.search.current = 0;
        self.search.highlights = true;
        self.search.restore = None;
        self.search.content_regex = None;
        self.search.pending_since = None;
    }

    pub fn search_next(&mut self, visible: usize) {
        self.search_step(visible, RevealDir::Down);
    }

    pub fn search_prev(&mut self, visible: usize) {
        self.search_step(visible, RevealDir::Up);
    }

    /// Move to the match that follows (or precedes) the row the user is on.
    /// The step runs the last search again first when the matches are gone,
    /// so `n` works after Esc cancelled the prompt, after Esc hid the
    /// highlights, and after a panel switch or a filter change.
    fn search_step(&mut self, visible: usize, dir: RevealDir) {
        self.search_rearm(visible);
        // The matches of a path or a semantic search come in score order, so
        // sort them into the order the panel lists them. The user reads `n`
        // as "the next one below", not "the next best one".
        let mut order: Vec<((usize, usize), usize)> = self
            .search
            .matches
            .iter()
            .enumerate()
            .filter_map(|(pos, m)| self.locate_entry(m.entry_idx).map(|at| (at, pos)))
            .collect();
        order.sort();
        if order.is_empty() {
            return;
        }
        let here = self
            .locate_entry(self.current_entry_idx().unwrap_or(usize::MAX))
            .unwrap_or((self.sel_group, 0));
        // The walk wraps around, as it does in vim.
        let pick = match dir {
            RevealDir::Down => order.iter().find(|(at, _)| *at > here).or(order.first()),
            RevealDir::Up => order.iter().rev().find(|(at, _)| *at < here).or(order.last()),
        };
        let Some(&(_, pos)) = pick else {
            return;
        };
        self.search.current = pos;
        self.search.highlights = true;
        self.jump_to_current_match_dir(visible, dir);
    }

    /// Put the last search back in place when nothing holds its matches any
    /// more. The selection does not move here: `search_step` decides where
    /// the user lands.
    fn search_rearm(&mut self, visible: usize) {
        if self.search.phase == SearchPhase::Active && !self.search.matches.is_empty() {
            return;
        }
        if self.search.last_query.is_empty() {
            return;
        }
        let here = (self.sel_group, self.sel_child);
        self.search.query = self.search.last_query.clone();
        self.search.kind = self.search.last_kind;
        self.search.restore = None;
        self.recompute_matches(visible);
        self.sel_group = here.0.min(self.groups.len().saturating_sub(1));
        self.sel_child = here.1;
        if let Some(group) = self.groups.get(self.sel_group)
            && self.sel_child >= group.entry_indices.len()
        {
            self.sel_child = group.entry_indices.len().saturating_sub(1);
        }
        if self.search.matches.is_empty() {
            self.search.phase = SearchPhase::Inactive;
        } else {
            self.search.phase = SearchPhase::Active;
        }
    }

    /// Where an entry sits in the History list: its group, and its place in
    /// that group. A collapsed group draws one row, so every entry of it
    /// reports place 0 and `n` steps over the whole group.
    fn locate_entry(&self, entry_idx: usize) -> Option<(usize, usize)> {
        self.groups.iter().enumerate().find_map(|(gi, g)| {
            g.entry_indices
                .iter()
                .position(|&e| e == entry_idx)
                .map(|ci| (gi, if g.expanded { ci } else { 0 }))
        })
    }

    fn recompute_matches(&mut self, visible: usize) {
        // Snapshot selection so we can keep parking on the same entry while typing.
        let current_entry_idx = self
            .groups
            .get(self.sel_group)
            .and_then(|g| g.entry_indices.get(self.sel_child).copied());

        self.search.matches.clear();
        self.search.current = 0;
        self.search.content_regex = None;
        // Whatever the query was waiting for, it's being computed now.
        self.search.pending_since = None;

        if self.search.query.is_empty() || self.search.needs_more_chars() {
            return;
        }

        let results = match self.search.kind {
            SearchKind::Path => self.compute_path_matches(),
            SearchKind::Content => self.compute_content_matches(),
            SearchKind::Semantic => self.compute_semantic_matches(),
        };
        self.search.matches = results;

        // Park on the previously-selected entry if it still matches; else jump to the top hit.
        if let Some(idx) = current_entry_idx {
            if let Some(pos) = self.search.match_pos_for(idx) {
                self.search.current = pos;
                self.adjust_scroll(visible);
                // We're staying on the same entry, so refresh_diff doesn't
                // run — scroll the Diff panel to the new first match here
                // instead, so each keystroke moves the panel onto the hit.
                self.reveal_diff_content_match();
                return;
            }
        }
        if !self.search.matches.is_empty() {
            self.jump_to_current_match(visible);
        }
    }

    fn compute_path_matches(&mut self) -> Vec<SearchMatch> {
        // Lowercased for the same reason as `PanelSearch::recompute`: nucleo's
        // raw `Matcher` asserts on uppercase needles when `ignore_case` is on.
        let query = self.search.query.to_lowercase();
        let mut needle_buf: Vec<char> = Vec::new();
        let needle = Utf32Str::new(&query, &mut needle_buf);

        let mut hay_buf: Vec<char> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        let mut results: Vec<SearchMatch> = Vec::new();

        for group in &self.groups {
            for &entry_idx in &group.entry_indices {
                let entry = &self.store.entries[entry_idx];
                // Path search targets the *path*. For prompts, `display_path`
                // appends the whole (single-lined) prompt body — fuzzy-matching
                // a short query against that long text scatter-matches almost
                // every prompt, so match only the project chain here. Prompt
                // bodies are searchable via content search (`?`).
                let s = if entry.is_prompt() {
                    let (chain, _) = entry.display_path_segments(&self.store.project_names);
                    chain.join(" → ")
                } else {
                    entry.display_path(&self.store.project_names)
                };
                hay_buf.clear();
                let haystack = Utf32Str::new(&s, &mut hay_buf);
                indices.clear();
                if let Some(score) =
                    self.search.matcher.fuzzy_indices(haystack, needle, &mut indices)
                {
                    results.push(SearchMatch {
                        entry_idx,
                        indices: indices.clone(),
                        score,
                    });
                }
            }
        }

        // Highest score first; ties keep recency order (groups already iterate newest → oldest).
        results.sort_by(|a, b| b.score.cmp(&a.score));
        results
    }

    fn compute_content_matches(&mut self) -> Vec<SearchMatch> {
        // Build a case-insensitive regex. Invalid patterns fall through to an
        // empty result set so the UI reads as "no matches" instead of crashing.
        let regex = match RegexBuilder::new(&self.search.query)
            .case_insensitive(true)
            .build()
        {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        // First pass: materialize searchable text for every entry that the
        // current filter exposes, and cache it across keystrokes.
        let entry_indices: Vec<usize> = self
            .groups
            .iter()
            .flat_map(|g| g.entry_indices.iter().copied())
            .collect();
        self.fill_content_cache(&entry_indices);

        let mut results: Vec<SearchMatch> = Vec::new();
        for &entry_idx in &entry_indices {
            let key = content_cache_key(&self.store.entries[entry_idx]);
            let Some(text) = self.content_cache.get(&key) else {
                continue;
            };
            if regex.is_match(text) {
                results.push(SearchMatch {
                    entry_idx,
                    indices: Vec::new(),
                    score: 0,
                });
            }
        }
        // Groups iterate newest-first, so `entry_indices` is newest-first
        // already; no sort needed. Stash the regex so the Diff panel can
        // reuse it for in-line highlighting of hits.
        self.search.content_regex = Some(regex);
        results
    }

    /// Materialize the searchable text of every entry in `entry_indices` that
    /// the cache does not hold yet. A Prompt entry stores its text and its
    /// response; a FileChange entry stores its `+`/`-` diff lines.
    ///
    /// Two things keep this off the multi-second path. First a **prefilter**:
    /// an added line is part of the new snapshot and a removed line is part of
    /// the old one, so an entry whose two snapshots contain no match cannot
    /// produce a matching diff line. Such an entry needs no diff, and a query
    /// normally rejects almost every entry this way. `multi_line` makes `^`
    /// and `$` match at line breaks, so a line-anchored query behaves on the
    /// snapshot text as it does on a diff line. Second, the scan runs on all
    /// available cores, because it reads gigabytes of snapshots.
    fn fill_content_cache(&mut self, entry_indices: &[usize]) {
        let prefilter = match RegexBuilder::new(&self.search.query)
            .case_insensitive(true)
            .multi_line(true)
            .build()
        {
            Ok(r) => r,
            Err(_) => return,
        };

        let todo: Vec<usize> = entry_indices
            .iter()
            .copied()
            .filter(|&i| {
                !self
                    .content_cache
                    .contains_key(&content_cache_key(&self.store.entries[i]))
            })
            .collect();
        if todo.is_empty() {
            return;
        }

        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(todo.len());
        let chunk = todo.len().div_ceil(threads);
        let store = &self.store;
        let prefilter = &prefilter;

        let found: Vec<(PathBuf, String)> = std::thread::scope(|scope| {
            let handles: Vec<_> = todo
                .chunks(chunk)
                .map(|part| {
                    scope.spawn(move || {
                        let mut out: Vec<(PathBuf, String)> = Vec::new();
                        for &idx in part {
                            let entry = &store.entries[idx];
                            let key = content_cache_key(entry);
                            match &entry.kind {
                                HistoryEntryKind::Prompt { .. } => {
                                    out.push((key, turn_text(entry)));
                                }
                                HistoryEntryKind::FileChange => {
                                    let (old_content, new_content) =
                                        entry_snapshots(store, idx);
                                    if !prefilter.is_match(&new_content)
                                        && !prefilter.is_match(&old_content)
                                    {
                                        continue;
                                    }
                                    out.push((key, diff_text(&old_content, &new_content)));
                                }
                            }
                        }
                        out
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().ok())
                .flatten()
                .collect()
        });

        // An entry the prefilter rejected stays out of the cache. It holds no
        // match for this query, and the next query prefilters it again.
        self.content_cache.extend(found);
    }


    // ── Semantic search ──────────────────────────────────────────────────────

    /// The sqlite key of an entry. It reuses [`content_cache_key`], which is
    /// already the identity of an entry's searchable text, so the vector store
    /// and the content cache never disagree about what one entry is.
    fn embed_key(entry: &HistoryEntry) -> String {
        content_cache_key(entry).to_string_lossy().into_owned()
    }

    /// Build one job per entry that the in-memory index does not hold.
    /// Phase 1 embeds prompts. `include_diffs` turns on phase 2.
    fn embed_jobs(&self, include_diffs: bool) -> Vec<EmbedJob> {
        let mut jobs = Vec::new();
        for (idx, entry) in self.store.entries.iter().enumerate() {
            let key = Self::embed_key(entry);
            if self.embed.index.contains(&key) {
                continue;
            }
            match &entry.kind {
                HistoryEntryKind::Prompt { .. } => jobs.push(EmbedJob {
                    key,
                    kind: EmbedKind::Prompt,
                    text: Some(turn_text(entry)),
                    new_path: entry.file_path.clone(),
                    old_path: None,
                }),
                HistoryEntryKind::FileChange if include_diffs => jobs.push(EmbedJob {
                    key,
                    kind: EmbedKind::Diff,
                    text: None,
                    new_path: entry.snapshot_path.clone(),
                    old_path: self.store.prev_snapshot(idx).map(|p| p.to_path_buf()),
                }),
                HistoryEntryKind::FileChange => {}
            }
        }
        // Prompts first. They are short, so they cost little time, and they
        // carry the user's own words, which is what a semantic query most
        // often looks for. A long diff round then follows.
        jobs.sort_by_key(|j| match j.kind {
            EmbedKind::Prompt => 0,
            EmbedKind::Diff => 1,
        });
        jobs
    }

    /// Start the model server and the indexer, or start one more indexer round
    /// for entries that loaded after the last round. Returns at once; the work
    /// runs on a background thread.
    pub fn start_embedding(&mut self, include_diffs: bool) {
        match self.embed.status {
            // A round is already in flight.
            EmbedStatus::Starting | EmbedStatus::Indexing => return,
            EmbedStatus::Failed(_) | EmbedStatus::Off | EmbedStatus::Ready => {}
        }
        if let Some(msg) = self.embed.cfg.missing_parts() {
            self.embed.status = EmbedStatus::Failed(msg);
            return;
        }

        let first_round = self.embed.server.is_none();
        if first_round {
            match EmbedServer::spawn(&self.embed.cfg) {
                Ok(server) => {
                    self.embed.server = Some(server);
                    self.embed.client = Some(EmbedClient::new(&self.embed.cfg));
                }
                Err(e) => {
                    self.embed.status = EmbedStatus::Failed(e.to_string());
                    return;
                }
            }
        }

        let jobs = self.embed_jobs(include_diffs);
        if !first_round && jobs.is_empty() {
            self.embed.status = EmbedStatus::Ready;
            return;
        }

        let (tx, rx) = mpsc::channel();
        let cfg = self.embed.cfg.clone();
        let Some(client) = self.embed.client.clone() else {
            return;
        };
        self.embed.rx = Some(rx);
        self.embed.done = 0;
        self.embed.total = jobs.len();
        self.embed.status = if first_round {
            EmbedStatus::Starting
        } else {
            EmbedStatus::Indexing
        };
        std::thread::spawn(move || run_indexer(cfg, client, jobs, first_round, tx));
    }

    /// Merge whatever the indexer thread produced. The main loop calls this
    /// each frame, so a search can already use a batch that just landed.
    pub fn drain_embed_events(&mut self) {
        let Some(rx) = &self.embed.rx else {
            return;
        };
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(EmbedEvent::Index(index)) => {
                    // Jobs whose key the store already held need no work.
                    self.embed.total = self
                        .embed
                        .total
                        .saturating_sub(count_known(&index, &self.store.entries));
                    self.embed.index = index;
                    self.embed.status = EmbedStatus::Indexing;
                }
                Ok(EmbedEvent::Batch(rows)) => {
                    self.embed.done += rows.len();
                    for (key, kind, vec) in rows {
                        self.embed.index.insert(key, kind, &vec);
                    }
                    self.embed.status = EmbedStatus::Indexing;
                }
                Ok(EmbedEvent::Done) => {
                    self.embed.status = EmbedStatus::Ready;
                    finished = true;
                }
                Ok(EmbedEvent::Failed(e)) => {
                    self.embed.status = EmbedStatus::Failed(e);
                    // Drop the server so the next `F` starts a clean round
                    // instead of trusting a process that did not answer.
                    self.embed.server = None;
                    self.embed.client = None;
                    finished = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.embed.rx = None;
        }
    }

    /// Open the search prompt in semantic mode. This also starts the server
    /// and the indexer on the first use, so the user pays for the model only
    /// when the user asks for this search.
    pub fn search_open_semantic(&mut self) {
        self.search.phase = SearchPhase::Typing;
        self.search.kind = SearchKind::Semantic;
        self.search.restore = Some((self.sel_group, self.sel_child));
        self.search_history.get_mut(self.history_search_mode()).reset_walk();
        self.search.query.clear();
        self.search.matches.clear();
        self.search.current = 0;
        self.search.highlights = true;
        self.search.content_regex = None;
        self.search.pending_since = None;
        self.start_embedding(true);
    }

    /// Rank the entries that the current filter exposes against the query.
    /// Runs on Enter only: one HTTP round trip per keystroke would be waste.
    fn compute_semantic_matches(&mut self) -> Vec<SearchMatch> {
        let Some(client) = self.embed.client.clone() else {
            return Vec::new();
        };
        // The server needs a few seconds to load the model. A query that
        // arrives before the load completes gets a refused connection, so wait
        // here. The call costs one /health round trip when the server is ready.
        if let Err(e) = client.wait_ready(Duration::from_secs(120)) {
            self.embed.status = EmbedStatus::Failed(e.to_string());
            return Vec::new();
        }
        let query = match client.embed_query(&self.search.query) {
            Ok(q) => q,
            Err(e) => {
                self.embed.status = EmbedStatus::Failed(e.to_string());
                return Vec::new();
            }
        };

        let mut scored: Vec<(f32, usize)> = Vec::new();
        for group in &self.groups {
            for &entry_idx in &group.entry_indices {
                let key = Self::embed_key(&self.store.entries[entry_idx]);
                let Some(vec) = self.embed.index.get(&key) else {
                    continue;
                };
                scored.push((cosine(&query, vec), entry_idx));
            }
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));

        let Some(&(best, _)) = scored.first() else {
            return Vec::new();
        };
        scored
            .into_iter()
            .take(SEMANTIC_TOP_K)
            .filter(|(score, _)| *score >= best - SEMANTIC_REL_FLOOR)
            .map(|(score, entry_idx)| SearchMatch {
                entry_idx,
                indices: Vec::new(),
                score: (score.clamp(0.0, 1.0) * SEMANTIC_SCORE_SCALE) as u16,
            })
            .collect()
    }

    /// Run one semantic search without the prompt, for the `--index` command
    /// line and for tests.
    pub fn semantic_search(&mut self, query: &str, visible: usize) {
        self.search.kind = SearchKind::Semantic;
        self.search.query = query.to_string();
        self.search.phase = SearchPhase::Active;
        self.recompute_matches(visible);
    }

    /// The cosine score of the match the cursor sits on, for the prompt line.
    pub fn semantic_current_score(&self) -> Option<f32> {
        if self.search.kind != SearchKind::Semantic {
            return None;
        }
        let m = self.search.matches.get(self.search.current)?;
        Some(m.score as f32 / SEMANTIC_SCORE_SCALE)
    }

    /// How many of the current matches are prompts, and how many are edits.
    pub fn semantic_split(&self) -> (usize, usize) {
        let prompts = self
            .search
            .matches
            .iter()
            .filter(|m| self.store.entries[m.entry_idx].is_prompt())
            .count();
        (prompts, self.search.matches.len() - prompts)
    }

    // ── Projects-panel search ────────────────────────────────────────────────

    /// Display label used by the fuzzy matcher for project row `i`. Mirrors
    /// the rendered chain so a search for "foo → bar" can match the chain
    /// glyphs the user sees on screen.
    fn project_search_label(&self, i: usize) -> String {
        let proj = &self.store.projects[i];
        self.store
            .project_chains
            .get(proj)
            .map(|chain| {
                chain
                    .iter()
                    .map(|p| match self.store.project_names.get(p) {
                        Some(s) => s.clone(),
                        None => p
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("?")
                            .to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(" → ")
            })
            .unwrap_or_else(|| Self::project_label(proj))
    }

    fn project_search_items(&self) -> Vec<String> {
        (0..self.store.projects.len())
            .map(|i| self.project_search_label(i))
            .collect()
    }

    pub fn project_search_open(&mut self) {
        let current = self
            .selected_project_idx
            .and_then(|i| self.store.projects.get(i).cloned());
        self.project_search.phase = SearchPhase::Typing;
        self.project_search.restore = current;
        self.search_history.get_mut(SearchMode::Project).reset_walk();
        self.project_search.query.clear();
        self.project_search.matches.clear();
        self.project_search.current = 0;
        self.project_search.highlights = true;
    }

    pub fn project_search_push_char(&mut self, c: char, visible: usize) {
        self.project_search.query.push(c);
        self.search_history.get_mut(SearchMode::Project).reset_walk();
        self.recompute_project_matches(visible);
    }

    pub fn project_search_pop_char(&mut self, visible: usize) {
        self.project_search.query.pop();
        self.search_history.get_mut(SearchMode::Project).reset_walk();
        self.recompute_project_matches(visible);
    }

    /// Put the next older remembered query on the Projects prompt (Up).
    pub fn project_search_history_older(&mut self, visible: usize) {
        let current = self.project_search.query.clone();
        let Some(query) = self.search_history.get_mut(SearchMode::Project).older(&current) else {
            return;
        };
        self.project_search.query = query;
        self.recompute_project_matches(visible);
    }

    /// Put the next newer remembered query on the Projects prompt (Down).
    pub fn project_search_history_newer(&mut self, visible: usize) {
        let Some(query) = self.search_history.get_mut(SearchMode::Project).newer() else {
            return;
        };
        self.project_search.query = query;
        self.recompute_project_matches(visible);
    }

    pub fn project_search_commit(&mut self) {
        let query = self.project_search.query.clone();
        self.search_history.get_mut(SearchMode::Project).push(&query);
        if self.project_search.matches.is_empty() {
            self.project_search.clear();
            return;
        }
        self.project_search.phase = SearchPhase::Active;
        self.project_search.highlights = true;
        self.project_search.restore = None;
    }

    pub fn project_search_hide(&mut self) {
        self.project_search.hide();
    }

    pub fn project_search_cancel(&mut self, visible: usize) {
        // Captured selection might have been a specific project (Some(p)) or
        // the "All projects" row (None). `restore` carries that, with the
        // outer field cleared by `clear()` so a future `cancel` outside
        // Typing is a no-op.
        let query = self.project_search.query.clone();
        self.search_history.get_mut(SearchMode::Project).push(&query);
        let target = self.project_search.restore.take();
        let was_typing = self.project_search.phase == SearchPhase::Typing;
        self.project_search.clear();
        if was_typing {
            self.select_project_by_path(target, visible);
        }
    }

    pub fn project_search_next(&mut self, visible: usize) {
        if self.project_search.matches.is_empty() {
            return;
        }
        self.project_search.highlights = true;
        self.project_search.current =
            (self.project_search.current + 1) % self.project_search.matches.len();
        self.jump_to_current_project_match(visible);
    }

    pub fn project_search_prev(&mut self, visible: usize) {
        if self.project_search.matches.is_empty() {
            return;
        }
        self.project_search.highlights = true;
        let len = self.project_search.matches.len();
        self.project_search.current = (self.project_search.current + len - 1) % len;
        self.jump_to_current_project_match(visible);
    }

    fn recompute_project_matches(&mut self, visible: usize) {
        let current_item_idx = self.selected_project_idx;
        let items = self.project_search_items();
        self.project_search.recompute(&items);

        // Park on the currently-selected project if it still matches; else
        // jump to the top hit. Matches existing history-search behavior.
        if let Some(i) = current_item_idx {
            if let Some(pos) = self.project_search.match_pos_for(i) {
                self.project_search.current = pos;
                return;
            }
        }
        if !self.project_search.matches.is_empty() {
            self.jump_to_current_project_match(visible);
        }
    }

    fn jump_to_current_project_match(&mut self, visible: usize) {
        let Some(m) = self.project_search.matches.get(self.project_search.current) else {
            return;
        };
        let target_path = self.store.projects.get(m.item_idx).cloned();
        self.select_project_by_path(target_path, visible);
    }

    /// Move the Projects selection to the given project (or "All" for `None`),
    /// adjusting scroll to keep the row in view. Triggers a `refilter` since
    /// changing the project filter ripples through Files/History/Diff.
    fn select_project_by_path(&mut self, target: Option<PathBuf>, visible: usize) {
        let new_idx = target
            .as_ref()
            .and_then(|p| self.store.projects.iter().position(|x| x == p));
        if self.selected_project_idx == new_idx {
            return;
        }
        self.selected_project_idx = new_idx;
        let display_row = new_idx.map(|i| i + 1).unwrap_or(0);
        if display_row < self.project_scroll {
            self.project_scroll = display_row;
        } else if visible > 0 && display_row >= self.project_scroll + visible {
            self.project_scroll = display_row + 1 - visible;
        }
        // Different project = different files list — old file_search matches
        // would index into a stale Vec.
        self.file_search.clear();
        self.refilter();
    }

    // ── Files-panel search ───────────────────────────────────────────────────

    fn file_search_label(&self, i: usize) -> String {
        let file = &self.files[i];
        let chain = self.file_chains.get(file).cloned().unwrap_or_default();
        let project_selected = self.selected_project_idx.is_some();
        // Reuse the same label the panel renders so the user can search the
        // text they see (chain + rel path, or just rel path when a project
        // filter hides the chain).
        crate::ui::file_display_for_panel(
            file,
            &chain,
            &self.store.project_names,
            project_selected,
        )
    }

    fn file_search_items(&self) -> Vec<String> {
        (0..self.files.len())
            .map(|i| self.file_search_label(i))
            .collect()
    }

    pub fn file_search_open(&mut self) {
        let current = self.selected_file_path.clone();
        self.file_search.phase = SearchPhase::Typing;
        self.file_search.restore = current;
        self.search_history.get_mut(SearchMode::File).reset_walk();
        self.file_search.query.clear();
        self.file_search.matches.clear();
        self.file_search.current = 0;
        self.file_search.highlights = true;
    }

    pub fn file_search_push_char(&mut self, c: char, visible: usize) {
        self.file_search.query.push(c);
        self.search_history.get_mut(SearchMode::File).reset_walk();
        self.recompute_file_matches(visible);
    }

    pub fn file_search_pop_char(&mut self, visible: usize) {
        self.file_search.query.pop();
        self.search_history.get_mut(SearchMode::File).reset_walk();
        self.recompute_file_matches(visible);
    }

    /// Put the next older remembered query on the Files prompt (Up).
    pub fn file_search_history_older(&mut self, visible: usize) {
        let current = self.file_search.query.clone();
        let Some(query) = self.search_history.get_mut(SearchMode::File).older(&current) else {
            return;
        };
        self.file_search.query = query;
        self.recompute_file_matches(visible);
    }

    /// Put the next newer remembered query on the Files prompt (Down).
    pub fn file_search_history_newer(&mut self, visible: usize) {
        let Some(query) = self.search_history.get_mut(SearchMode::File).newer() else {
            return;
        };
        self.file_search.query = query;
        self.recompute_file_matches(visible);
    }

    pub fn file_search_commit(&mut self) {
        let query = self.file_search.query.clone();
        self.search_history.get_mut(SearchMode::File).push(&query);
        if self.file_search.matches.is_empty() {
            self.file_search.clear();
            return;
        }
        self.file_search.phase = SearchPhase::Active;
        self.file_search.highlights = true;
        self.file_search.restore = None;
    }

    pub fn file_search_hide(&mut self) {
        self.file_search.hide();
    }

    pub fn file_search_cancel(&mut self, visible: usize) {
        let query = self.file_search.query.clone();
        self.search_history.get_mut(SearchMode::File).push(&query);
        let target = self.file_search.restore.take();
        let was_typing = self.file_search.phase == SearchPhase::Typing;
        self.file_search.clear();
        if was_typing {
            self.select_file_by_path(target, visible);
        }
    }

    pub fn file_search_next(&mut self, visible: usize) {
        if self.file_search.matches.is_empty() {
            return;
        }
        self.file_search.highlights = true;
        self.file_search.current =
            (self.file_search.current + 1) % self.file_search.matches.len();
        self.jump_to_current_file_match(visible);
    }

    pub fn file_search_prev(&mut self, visible: usize) {
        if self.file_search.matches.is_empty() {
            return;
        }
        self.file_search.highlights = true;
        let len = self.file_search.matches.len();
        self.file_search.current = (self.file_search.current + len - 1) % len;
        self.jump_to_current_file_match(visible);
    }

    fn recompute_file_matches(&mut self, visible: usize) {
        let current_item_idx = self.sel_file_idx();
        let items = self.file_search_items();
        self.file_search.recompute(&items);

        if let Some(i) = current_item_idx {
            if let Some(pos) = self.file_search.match_pos_for(i) {
                self.file_search.current = pos;
                return;
            }
        }
        if !self.file_search.matches.is_empty() {
            self.jump_to_current_file_match(visible);
        }
    }

    fn jump_to_current_file_match(&mut self, visible: usize) {
        let Some(m) = self.file_search.matches.get(self.file_search.current) else {
            return;
        };
        let target = self.files.get(m.item_idx).cloned();
        self.select_file_by_path(target, visible);
    }

    fn select_file_by_path(&mut self, target: Option<PathBuf>, visible: usize) {
        if self.selected_file_path == target {
            return;
        }
        self.selected_file_path = target.clone();
        let new_idx = target
            .as_ref()
            .and_then(|p| self.files.iter().position(|x| x == p));
        let display_row = new_idx.map(|i| i + 1).unwrap_or(0);
        if display_row < self.file_scroll {
            self.file_scroll = display_row;
        } else if visible > 0 && display_row >= self.file_scroll + visible {
            self.file_scroll = display_row + 1 - visible;
        }
        self.refilter();
    }

    // ── Diff-panel content search ────────────────────────────────────────────

    pub fn diff_search_open(&mut self) {
        self.diff_search.phase = SearchPhase::Typing;
        self.diff_search.restore_scroll = Some(self.diff_scroll);
        self.search_history.get_mut(SearchMode::Diff).reset_walk();
        self.diff_search.query.clear();
        self.diff_search.matches.clear();
        self.diff_search.current = 0;
        self.diff_search.highlights = true;
        self.diff_search.regex = None;
    }

    pub fn diff_search_push_char(&mut self, c: char) {
        self.diff_search.query.push(c);
        self.search_history.get_mut(SearchMode::Diff).reset_walk();
        self.recompute_diff_matches();
        self.jump_to_current_diff_match();
    }

    pub fn diff_search_pop_char(&mut self) {
        self.diff_search.query.pop();
        self.search_history.get_mut(SearchMode::Diff).reset_walk();
        self.recompute_diff_matches();
        self.jump_to_current_diff_match();
    }

    /// Put the next older remembered query on the Diff prompt (Up).
    pub fn diff_search_history_older(&mut self) {
        let current = self.diff_search.query.clone();
        let Some(query) = self.search_history.get_mut(SearchMode::Diff).older(&current) else {
            return;
        };
        self.diff_search.query = query;
        self.recompute_diff_matches();
        self.jump_to_current_diff_match();
    }

    /// Put the next newer remembered query on the Diff prompt (Down).
    pub fn diff_search_history_newer(&mut self) {
        let Some(query) = self.search_history.get_mut(SearchMode::Diff).newer() else {
            return;
        };
        self.diff_search.query = query;
        self.recompute_diff_matches();
        self.jump_to_current_diff_match();
    }

    pub fn diff_search_commit(&mut self) {
        let query = self.diff_search.query.clone();
        self.search_history.get_mut(SearchMode::Diff).push(&query);
        if self.diff_search.matches.is_empty() {
            self.diff_search_clear();
            return;
        }
        self.diff_search.phase = SearchPhase::Active;
        self.diff_search.highlights = true;
        self.diff_search.restore_scroll = None;
    }

    pub fn diff_search_cancel(&mut self) {
        let query = self.diff_search.query.clone();
        self.search_history.get_mut(SearchMode::Diff).push(&query);
        if let Some(scroll) = self.diff_search.restore_scroll.take() {
            self.diff_scroll = scroll;
        }
        self.diff_search_clear();
    }

    pub fn diff_search_clear(&mut self) {
        self.diff_search = DiffSearchState::default();
    }

    pub fn diff_search_hide(&mut self) {
        if self.diff_search.phase == SearchPhase::Active {
            self.diff_search.highlights = false;
        }
    }

    pub fn diff_search_next(&mut self) {
        if self.diff_search.matches.is_empty() {
            return;
        }
        self.diff_search.highlights = true;
        self.diff_search.current =
            (self.diff_search.current + 1) % self.diff_search.matches.len();
        self.jump_to_current_diff_match();
    }

    pub fn diff_search_prev(&mut self) {
        if self.diff_search.matches.is_empty() {
            return;
        }
        self.diff_search.highlights = true;
        let len = self.diff_search.matches.len();
        self.diff_search.current = (self.diff_search.current + len - 1) % len;
        self.jump_to_current_diff_match();
    }

    /// Recompute `diff_search.matches` against the current `diff_lines`.
    /// Called after a query edit and after every `refresh_diff` so a search
    /// active across entry navigation tracks whichever diff is showing.
    /// Preserves the user's `current` index when possible so n/N doesn't snap
    /// back to the first match on every refresh.
    fn recompute_diff_matches(&mut self) {
        let prev_current_line = self
            .diff_search
            .matches
            .get(self.diff_search.current)
            .copied();
        self.diff_search.matches.clear();
        self.diff_search.current = 0;
        self.diff_search.regex = None;
        if self.diff_search.query.is_empty() {
            return;
        }
        let regex = match RegexBuilder::new(&self.diff_search.query)
            .case_insensitive(true)
            .build()
        {
            Ok(r) => r,
            Err(_) => return,
        };
        for (i, dl) in self.diff_lines.iter().enumerate() {
            if let Some(text) = diff_line_text(dl) {
                if regex.is_match(text) {
                    self.diff_search.matches.push(i);
                }
            }
        }
        if let Some(line) = prev_current_line {
            if let Some(pos) = self.diff_search.matches.iter().position(|&l| l == line) {
                self.diff_search.current = pos;
            }
        }
        self.diff_search.regex = Some(regex);
    }

    /// Scroll the Diff panel so `diff_search.matches[current]` is at the top.
    /// No-op when there are no matches.
    fn jump_to_current_diff_match(&mut self) {
        let Some(&line) = self
            .diff_search
            .matches
            .get(self.diff_search.current)
        else {
            return;
        };
        self.diff_scroll = line;
    }

    /// First diff-line index whose searchable text matches `regex`, or `None`.
    /// Used to follow `f`/`n`/`N` from the History panel into the Diff.
    fn first_diff_line_matching(&self, regex: &Regex) -> Option<usize> {
        self.diff_lines.iter().position(|dl| {
            diff_line_text(dl).map_or(false, |t| regex.is_match(t))
        })
    }

    /// Move selection to the entry in `search.matches[search.current]`,
    /// auto-expanding its parent group if needed. Reveals the row as a
    /// forward step does.
    fn jump_to_current_match(&mut self, visible: usize) {
        self.jump_to_current_match_dir(visible, RevealDir::Down);
    }

    /// `jump_to_current_match`, but `dir` tells `reveal_selection` which side
    /// of the row to give the extra context lines to.
    fn jump_to_current_match_dir(&mut self, visible: usize, dir: RevealDir) {
        let Some(m) = self.search.matches.get(self.search.current) else {
            return;
        };
        let target = m.entry_idx;

        let Some((group_idx, child_local)) =
            self.groups.iter().enumerate().find_map(|(gi, g)| {
                g.entry_indices
                    .iter()
                    .position(|&e| e == target)
                    .map(|ci| (gi, ci))
            })
        else {
            return;
        };

        // Collapse whatever group we're leaving, expand the destination group.
        let auto_expand = self.effective_auto_expand();
        if auto_expand {
            if self.sel_group != group_idx {
                self.groups[self.sel_group].expanded = false;
            }
            self.groups[group_idx].expanded = true;
        }
        self.sel_group = group_idx;
        self.sel_child = if auto_expand { child_local } else { 0 };
        self.rebuild_display();
        self.diff_scroll = 0;
        self.reveal_selection(visible, dir);
        self.refresh_diff();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::history::EditSource;

    fn prompt_entry(text: &str, session: &str, minutes_ago: i64) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local::now() - chrono::Duration::minutes(minutes_ago),
            file_path: PathBuf::from(format!("claude:{session}:{text}")),
            snapshot_path: PathBuf::new(),
            base_path: None,
            project_roots: vec![PathBuf::from("/dev/proj")],
            source: EditSource::ClaudeCode,
            kind: HistoryEntryKind::Prompt {
                text: text.to_string(),
                response: None,
            },
            session_id: Some(session.to_string()),
            session_cwd: None,
        }
    }

    #[test]
    fn a_prompt_that_arrives_again_with_a_response_replaces_the_old_entry() {
        let store = HistoryStore::from_entries(vec![prompt_entry("fix it", "s1", 5)]);
        let mut app = App::new(store, Sources::default());
        app.content_cache
            .insert(content_cache_key(&app.store.entries[0]), "fix it".to_string());

        let mut again = prompt_entry("fix it", "s1", 5);
        again.kind = HistoryEntryKind::Prompt {
            text: "fix it".to_string(),
            response: Some("Fixed.".to_string()),
        };
        app.merge_batch(vec![again, nvim_entry("/dev/proj/a.rs", 1)], 10);

        let prompts: Vec<&HistoryEntry> =
            app.store.entries.iter().filter(|e| e.is_prompt()).collect();
        assert_eq!(prompts.len(), 1, "the prompt is replaced, not duplicated");
        assert_eq!(prompts[0].response_text(), Some("Fixed."));
        assert_eq!(app.store.entries.len(), 2);
        assert!(app.content_cache.is_empty(), "the stale search text is dropped");
    }

    /// A Cursor prompt carries the composer that wrote it. The fuzzy join
    /// runs on Local-History edits only, so it must not clear that composer.
    #[test]
    fn cursor_attribution_keeps_the_composer_of_a_prompt() {
        let mut prompt = prompt_entry("fix it", "composer-1", 5);
        prompt.source = EditSource::Cursor;
        prompt.file_path = PathBuf::from("cursor:composer-1:bubble-1");
        let edit = {
            let mut e = nvim_entry("/dev/proj/a.rs", 1);
            e.source = EditSource::Cursor;
            e.session_id = Some("stale".to_string());
            e
        };

        let mut app = App::new(HistoryStore::from_entries(vec![prompt, edit]), Sources::default());
        app.sources.cursor_sessions =
            Some(std::sync::Arc::new(crate::cursor_sessions::CursorSessions::default()));
        app.apply_cursor_attribution();

        let prompt = app.store.entries.iter().find(|e| e.is_prompt()).unwrap();
        assert_eq!(prompt.session_id.as_deref(), Some("composer-1"));
        let edit = app.store.entries.iter().find(|e| !e.is_prompt()).unwrap();
        assert_eq!(edit.session_id, None, "an unattributed edit loses its composer");
    }

    /// A committed query comes back on Up, and each corpus keeps its own list.
    #[test]
    fn each_history_search_mode_walks_its_own_query_list() {
        let store = HistoryStore::from_entries(vec![prompt_entry("fix it", "s1", 5)]);
        let mut app = App::new(store, Sources::default());

        app.search_open();
        for c in "fix".chars() {
            app.search_push_char(c, 10);
        }
        app.search_commit(10);

        app.search_open_content();
        for c in "worker".chars() {
            app.search_push_char(c, 10);
        }
        app.search_commit(10);

        app.search_open();
        app.search_history_older(10);
        assert_eq!(app.search.query, "fix", "the path prompt walks the path list");
        app.search_history_older(10);
        assert_eq!(app.search.query, "fix", "the walk stops at the oldest query");
        app.search_history_newer(10);
        assert_eq!(app.search.query, "", "Down restores the empty draft");
        app.search_cancel(10);

        app.search_open_content();
        app.search_history_older(10);
        assert_eq!(app.search.query, "worker", "the content list is separate");
    }

    /// Esc still ends a search that the user ran, so the query is remembered.
    #[test]
    fn a_canceled_search_stays_in_the_query_list() {
        let store = HistoryStore::from_entries(vec![prompt_entry("fix it", "s1", 5)]);
        let mut app = App::new(store, Sources::default());

        app.search_open();
        for c in "abc".chars() {
            app.search_push_char(c, 10);
        }
        app.search_cancel(10);

        app.search_open();
        app.search_history_older(10);
        assert_eq!(app.search.query, "abc");
    }

    /// `n` runs the last search again and moves to the match below the row
    /// the user is on, even after Esc dropped the matches.
    #[test]
    fn n_repeats_the_last_search_after_esc() {
        let store = HistoryStore::from_entries(vec![
            prompt_entry("alpha one", "s1", 5),
            prompt_entry("beta", "s2", 4),
            prompt_entry("alpha two", "s3", 3),
        ]);
        let mut app = App::new(store, Sources::default());

        // Content search reads the prompt text; path search reads the chain.
        app.search_open_content();
        for c in "alpha".chars() {
            app.search_push_char(c, 10);
        }
        app.search_commit(10);
        assert_eq!(app.search.matches.len(), 2, "both alpha prompts match");

        // Esc at the prompt drops the matches, as it always has.
        app.search_open_content();
        app.search_cancel(10);
        assert!(app.search.matches.is_empty());
        assert_eq!(app.search.last_query, "alpha", "the last run is remembered");

        // The user walks to the top row, then repeats the search.
        app.sel_group = 0;
        app.sel_child = 0;
        let top = app.current_entry_idx().unwrap();
        app.search_next(10);
        assert_eq!(app.search.phase, SearchPhase::Active, "the search is armed again");
        assert_eq!(app.search.matches.len(), 2);
        let landed = app.current_entry_idx().unwrap();
        assert_ne!(landed, top, "n moves off the row the user is on");
        assert!(
            app.search.match_pos_for(landed).is_some(),
            "n lands on a match"
        );

        // A second `n` wraps back to the other match.
        app.search_next(10);
        assert_eq!(app.current_entry_idx(), Some(top), "the walk wraps around");
    }

    /// `n` walks the list in row order, not in score order.
    #[test]
    fn n_walks_the_rows_in_order() {
        let store = HistoryStore::from_entries(vec![
            prompt_entry("alpha", "s1", 5),
            prompt_entry("alpha alpha alpha", "s2", 4),
            prompt_entry("alpha", "s3", 3),
        ]);
        let mut app = App::new(store, Sources::default());
        app.search_open_content();
        for c in "alpha".chars() {
            app.search_push_char(c, 10);
        }
        app.search_commit(10);

        app.sel_group = 0;
        app.sel_child = 0;
        let mut seen = vec![app.sel_group];
        for _ in 0..2 {
            app.search_next(10);
            seen.push(app.sel_group);
        }
        assert_eq!(seen, vec![0, 1, 2], "n steps down one row at a time");
        app.search_prev(10);
        assert_eq!(app.sel_group, 1, "N steps back up");
    }

    /// The typed text filters the walk, as vim's command line filters it.
    #[test]
    fn the_typed_prefix_filters_the_walk() {
        let store = HistoryStore::from_entries(vec![prompt_entry("fix it", "s1", 5)]);
        let mut app = App::new(store, Sources::default());
        for query in ["secret worker", "cursor prompt", "secret rotation"] {
            app.search_open();
            for c in query.chars() {
                app.search_push_char(c, 10);
            }
            app.search_commit(10);
        }

        app.search_open();
        for c in "sec".chars() {
            app.search_push_char(c, 10);
        }
        app.search_history_older(10);
        assert_eq!(app.search.query, "secret rotation");
        app.search_history_older(10);
        assert_eq!(app.search.query, "secret worker");
        app.search_history_newer(10);
        assert_eq!(app.search.query, "secret rotation");
        app.search_history_newer(10);
        assert_eq!(app.search.query, "sec", "the prefix comes back");
    }

    /// The query lists survive a restart through the persisted state.
    #[test]
    fn the_query_lists_survive_a_restart() {
        let store = HistoryStore::from_entries(vec![prompt_entry("fix it", "s1", 5)]);
        let mut app = App::new(store, Sources::default());
        app.diff_search_open();
        for c in "fn main".chars() {
            app.diff_search_push_char(c);
        }
        app.diff_search_commit();

        let saved = app.persisted_state();
        let store = HistoryStore::from_entries(vec![prompt_entry("fix it", "s1", 5)]);
        let mut next = App::new(store, Sources::default());
        next.apply_persisted(&saved);

        next.diff_search_open();
        next.diff_search_history_older();
        assert_eq!(next.diff_search.query, "fn main");
    }

    fn nvim_entry(path: &str, minutes_ago: i64) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local::now() - chrono::Duration::minutes(minutes_ago),
            file_path: PathBuf::from(path),
            snapshot_path: PathBuf::from("/dev/null"),
            base_path: None,
            project_roots: vec![PathBuf::from("/dev/proj")],
            source: EditSource::Neovim,
            kind: HistoryEntryKind::FileChange,
            session_id: None,
            session_cwd: None,
        }
    }

    /// Two agents whose prompts interleave, so a File-mode row is never row 0
    /// of its own agent.
    fn interleaved_app() -> App {
        let store = HistoryStore::from_entries(vec![
            prompt_entry("b2", "sessionB", 1),
            prompt_entry("a2", "sessionA", 2),
            prompt_entry("b1", "sessionB", 3),
            prompt_entry("a1", "sessionA", 4),
        ]);
        App::new(store, Sources::default())
    }

    /// Text of the selected prompt, or the selected file's name.
    fn selected(app: &App) -> String {
        let e = app.current_entry().expect("a selection");
        match e.prompt_text() {
            Some(t) => t.to_string(),
            None => e.file_name().to_string(),
        }
    }

    /// Move the History cursor onto the row whose prompt reads `text`.
    fn select_prompt(app: &mut App, text: &str) {
        let gi = app
            .groups
            .iter()
            .position(|g| {
                app.store.entries[g.entry_indices[0]].prompt_text() == Some(text)
            })
            .unwrap_or_else(|| panic!("no row for {text:?}"));
        app.sel_group = gi;
        app.sel_child = 0;
        app.rebuild_display_after_external_mutation(20);
    }

    /// A store with one prompt and three file changes, all in one minute, so
    /// the kind filter is the only thing that changes the row count.
    fn mixed_app() -> App {
        let store = HistoryStore::from_entries(vec![
            prompt_entry("fix the parser", "sessionA", 1),
            nvim_entry("/dev/proj/parser.rs", 2),
            nvim_entry("/dev/proj/lexer.rs", 3),
            nvim_entry("/dev/proj/main.rs", 4),
        ]);
        App::new(store, Sources::default())
    }

    fn entry_count(app: &App) -> usize {
        app.groups.iter().map(|g| g.entry_indices.len()).sum()
    }

    #[test]
    fn the_kind_filter_cycles_through_three_sets() {
        let mut app = mixed_app();
        assert_eq!(app.kind_filter, KindFilter::All);
        assert_eq!(entry_count(&app), 4);

        app.cycle_kind_filter(20);
        assert_eq!(app.kind_filter, KindFilter::Prompts);
        assert_eq!(entry_count(&app), 1);

        app.cycle_kind_filter(20);
        assert_eq!(app.kind_filter, KindFilter::Changes);
        assert_eq!(entry_count(&app), 3);

        app.cycle_kind_filter(20);
        assert_eq!(app.kind_filter, KindFilter::All);
        assert_eq!(entry_count(&app), 4);
    }

    #[test]
    fn the_kind_filter_narrows_an_active_search() {
        let mut app = mixed_app();
        app.search_open();
        for c in "proj".chars() {
            app.search_push_char(c, 20);
        }
        app.search_commit(20);
        // The path search reaches the prompt and the three file changes.
        assert_eq!(app.search.matches.len(), 4);

        app.cycle_kind_filter(20); // Prompts.
        assert_eq!(app.search.matches.len(), 1);
        assert_eq!(selected(&app), "fix the parser");

        app.cycle_kind_filter(20); // Changes.
        assert_eq!(app.search.matches.len(), 3);
        assert!(app.current_entry().is_some_and(|e| !e.is_prompt()));
    }

    #[test]
    fn agent_mode_selects_the_agent_of_the_selected_row() {
        let mut app = interleaved_app();
        select_prompt(&mut app, "b1");

        app.toggle_mode(20, 20);

        assert_eq!(app.mode, Mode::Agent);
        assert_eq!(app.selected_agent.as_deref(), Some("sessionB"));
        // History keeps the same row, now inside that one agent.
        assert_eq!(selected(&app), "b1");
        assert_eq!(app.groups.len(), 2, "History holds only sessionB's prompts");
    }

    #[test]
    fn file_mode_returns_to_the_row_it_left() {
        let mut app = interleaved_app();
        select_prompt(&mut app, "b1");

        app.toggle_mode(20, 20);
        app.toggle_mode(20, 20);

        assert_eq!(app.mode, Mode::File);
        assert_eq!(selected(&app), "b1", "the round trip must not move the cursor");
        assert_eq!(app.groups.len(), 4, "File mode lists every row again");
    }

    #[test]
    fn file_mode_follows_a_cursor_the_user_moved_in_agent_mode() {
        let mut app = interleaved_app();
        select_prompt(&mut app, "b1");

        app.toggle_mode(20, 20);
        // The user reads the agent's newer prompt, then switches back.
        app.history_up(20);
        assert_eq!(selected(&app), "b2");
        app.toggle_mode(20, 20);

        assert_eq!(selected(&app), "b2", "File mode follows the newer choice");
    }

    #[test]
    fn a_row_without_an_agent_survives_the_round_trip() {
        let store = HistoryStore::from_entries(vec![
            prompt_entry("b2", "sessionB", 1),
            nvim_entry("/dev/proj/src/main.rs", 2),
            prompt_entry("a1", "sessionA", 3),
        ]);
        let mut app = App::new(store, Sources::default());
        let gi = app
            .groups
            .iter()
            .position(|g| !app.store.entries[g.entry_indices[0]].is_prompt())
            .unwrap();
        app.sel_group = gi;
        app.sel_child = 0;
        app.rebuild_display_after_external_mutation(20);
        assert_eq!(selected(&app), "main.rs");

        // A Neovim edit has no agent, so Agent mode cannot show it.
        app.toggle_mode(20, 20);
        assert_ne!(selected(&app), "main.rs");
        app.toggle_mode(20, 20);

        assert_eq!(selected(&app), "main.rs", "the round trip must come back");
    }

    #[test]
    fn switching_agents_then_returning_lands_on_that_agent() {
        let mut app = interleaved_app();
        select_prompt(&mut app, "b1");
        app.toggle_mode(20, 20);

        // The user picks the other agent in the Agents panel.
        let other = app
            .agents
            .iter()
            .position(|a| a.session_id == "sessionA")
            .unwrap();
        app.select_agent_row(other, 20);
        assert_eq!(selected(&app), "a2");

        app.toggle_mode(20, 20);
        assert_eq!(selected(&app), "a2", "File mode follows the new agent's row");
    }

    #[test]
    fn a_row_without_an_agent_takes_the_agent_of_the_nearest_row() {
        let store = HistoryStore::from_entries(vec![
            prompt_entry("b2", "sessionB", 1),
            nvim_entry("/dev/proj/src/main.rs", 2),
            prompt_entry("a1", "sessionA", 3),
        ]);
        let mut app = App::new(store, Sources::default());
        let gi = app
            .groups
            .iter()
            .position(|g| !app.store.entries[g.entry_indices[0]].is_prompt())
            .unwrap();
        app.sel_group = gi;
        app.sel_child = 0;
        app.rebuild_display_after_external_mutation(20);

        app.toggle_mode(20, 20);

        // The Neovim edit names no agent. The row above it names sessionB.
        assert_eq!(app.selected_agent.as_deref(), Some("sessionB"));
        assert!(app.sel_agent_idx().is_some(), "the Agents panel has a selection");
    }

    /// The `f` search matches the text of a prompt and its response. The Diff
    /// panel must scroll onto the matched line, not stay at the top.
    #[test]
    fn the_content_search_scrolls_the_diff_onto_the_match_in_a_response() {
        let mut response: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
        response[40] = "the needle is here".to_string();
        let mut with_response = prompt_entry("second prompt", "sessionB", 5);
        with_response.kind = HistoryEntryKind::Prompt {
            text: "second prompt".to_string(),
            response: Some(response.join("\n")),
        };
        let store = HistoryStore::from_entries(vec![
            prompt_entry("first prompt", "sessionA", 1),
            with_response,
        ]);
        let mut app = App::new(store, Sources::default());
        let visible = 10;
        app.set_diff_height(visible);

        app.search_open_content();
        app.search.query = "needle".to_string();
        app.recompute_matches(20);

        // The prompt takes diff line 0, a blank line takes line 1, so response
        // line 40 takes diff line 42.
        let want = 42;
        assert_eq!(app.search.matches.len(), 1, "one entry holds the needle");
        assert!(
            matches!(&app.diff_lines[want], DiffLine::Response(t) if t.contains("needle")),
            "the diff line {want} holds the needle"
        );
        assert!(
            app.diff_scroll <= want && want < app.diff_scroll + visible,
            "the diff shows line {want}, but the scroll is {}",
            app.diff_scroll
        );
        // The move is longer than one page, so the line lands in the middle.
        assert_eq!(app.diff_scroll, want - visible / 2);
    }

    #[test]
    fn shift_y_copies_the_response_only_in_the_diff_panel() {
        let mut entry = prompt_entry("fix the parser", "sessionA", 1);
        entry.kind = HistoryEntryKind::Prompt {
            text: "fix the parser".to_string(),
            response: Some("I fixed it.".to_string()),
        };
        let store = HistoryStore::from_entries(vec![entry]);
        let mut app = App::new(store, Sources::default());

        // The History panel shows no response, so `Y` does nothing there.
        app.active_panel = Panel::History;
        assert_eq!(app.copy_response_target(), None);

        app.active_panel = Panel::Diff;
        assert_eq!(app.copy_response_target(), Some("I fixed it.".to_string()));
    }

    #[test]
    fn shift_y_copies_nothing_when_the_prompt_has_no_response() {
        let store =
            HistoryStore::from_entries(vec![prompt_entry("fix the parser", "sessionA", 1)]);
        let mut app = App::new(store, Sources::default());
        app.active_panel = Panel::Diff;
        assert_eq!(app.copy_response_target(), None);
    }

    #[test]
    fn the_status_bar_drops_open_file_on_a_prompt_row() {
        let store = HistoryStore::from_entries(vec![
            prompt_entry("fix the parser", "sessionA", 1),
            nvim_entry("/dev/proj/src/main.rs", 2),
        ]);
        let mut app = App::new(store, Sources::default());

        assert!(!app.can_open_file(), "a prompt row has no file to open");
        assert_eq!(
            app.copy_target(),
            Some(("fix the parser".to_string(), "prompt"))
        );

        app.history_down(20);
        assert!(app.can_open_file());
        assert_eq!(
            app.copy_target(),
            Some(("/dev/proj/src/main.rs".to_string(), "path"))
        );
    }

    #[test]
    fn the_mode_switch_brings_the_selected_row_onto_the_screen() {
        // Tall prompt rows: File mode holds twice as many rows as Agent mode,
        // so the scroll math must measure the new list, not the old one.
        let text: String = "wrap this prompt over several lines. ".repeat(8);
        let mut entries = Vec::new();
        for i in 0..30 {
            let session = if i % 2 == 0 { "sessionA" } else { "sessionB" };
            entries.push(prompt_entry(&format!("{text} {i}"), session, i as i64 + 1));
        }
        let mut app = App::new(HistoryStore::from_entries(entries), Sources::default());
        let visible = 20;
        let width = 60;
        app.recompute_row_heights(width, visible);

        // Agent mode, on the oldest row of one agent.
        app.toggle_mode(visible, visible);
        app.recompute_row_heights(width, visible);
        app.history_bottom(visible);
        app.recompute_row_heights(width, visible);
        let want = app.current_entry_id();

        app.toggle_mode(visible, visible);
        // What the next frame measures before it draws.
        app.recompute_row_heights(width, visible);

        assert_eq!(app.current_entry_id(), want, "the row survives the switch");
        let row = app.sel_display_row();
        assert!(row >= app.history_scroll, "the row is not above the window");
        let used: usize = (app.history_scroll..=row).map(|i| app.row_height(i)).sum();
        assert!(
            used <= visible,
            "the row must fit on the screen: rows {}..={row} need {used} of {visible} lines",
            app.history_scroll
        );
    }

    #[test]
    fn the_mode_switch_keeps_the_history_panel_active() {
        let entries = vec![
            prompt_entry("first", "sessionA", 1),
            prompt_entry("second", "sessionB", 2),
        ];
        let mut app = App::new(HistoryStore::from_entries(entries), Sources::default());

        app.active_panel = Panel::History;
        app.toggle_mode(20, 20);
        assert_eq!(app.mode, Mode::Agent);
        assert_eq!(app.active_panel, Panel::History);

        app.toggle_mode(20, 20);
        assert_eq!(app.mode, Mode::File);
        assert_eq!(app.active_panel, Panel::History);

        // From any other panel, Agent mode still starts on the Agents panel.
        app.active_panel = Panel::Files;
        app.toggle_mode(20, 20);
        assert_eq!(app.active_panel, Panel::Agents);
    }

    #[test]
    fn the_mode_switch_puts_the_selected_row_near_the_middle() {
        // One agent owns every second row, so Agent mode lists 15 of the 30.
        let mut entries = Vec::new();
        for i in 0..30 {
            let session = if i % 2 == 0 { "sessionA" } else { "sessionB" };
            entries.push(prompt_entry(&format!("prompt {i}"), session, i as i64 + 1));
        }
        let mut app = App::new(HistoryStore::from_entries(entries), Sources::default());
        let visible = 10;
        let width = 60;
        app.recompute_row_heights(width, visible);

        // Select a row in the middle of the File-mode list, then switch.
        for _ in 0..14 {
            app.history_down(visible);
        }
        app.recompute_row_heights(width, visible);
        app.toggle_mode(visible, visible);
        app.recompute_row_heights(width, visible);

        let row = app.sel_display_row();
        assert!(row > app.history_scroll, "rows before the selected row show");
        let above: usize = (app.history_scroll..row).map(|i| app.row_height(i)).sum();
        assert!(
            above >= visible / 2 - 1 && above <= visible / 2 + 1,
            "the selected row sits near the middle: {above} lines above it of {visible}"
        );
    }

    /// Non-hermetic timing probe for the content (`f`) search, ignored by
    /// default (`cargo test --release -- --ignored --nocapture app::tests`).
    /// Loads the real local history, then times the *cold* scan — the one that
    /// used to freeze the UI on the first keystroke — and a warm one.
    #[test]
    #[ignore]
    fn timing_cold_content_search() {
        let mut app = App::new(HistoryStore::from_entries(Vec::new()), Sources::detect());
        app.start_background_load();
        let load_start = Instant::now();
        while app.loading {
            app.drain_load_events(40);
            std::thread::sleep(Duration::from_millis(5));
        }
        eprintln!(
            "loaded {} entries in {:?}",
            app.store.entries.len(),
            load_start.elapsed()
        );

        app.search_open_content();
        app.search.query = "fn ".to_string();
        let cold = Instant::now();
        app.recompute_matches(40);
        let cold = cold.elapsed();
        let hits = app.search.matches.len();

        app.search.query = "fn m".to_string();
        let warm = Instant::now();
        app.recompute_matches(40);
        let warm = warm.elapsed();

        eprintln!("cold scan {cold:?} ({hits} hits), warm scan {warm:?}");
        assert!(hits > 0, "expected the local history to match \"fn \"");
    }
}


#[cfg(test)]
mod smoke {
    use super::*;
    use crate::history::EditSource;

    /// Non-hermetic: loads every real source on this machine, then runs the
    /// content (`f`) search. Ignored by default:
    /// `cargo test smoke_full_load_and_content_search -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn smoke_full_load_and_content_search() {
        let needle = std::env::var("RECAP_NEEDLE").unwrap_or_else(|_| "space_for".to_string());
        let visible = 40;
        let mut app = App::new(HistoryStore::from_entries(Vec::new()), Sources::detect());
        let t0 = Instant::now();
        app.start_background_load();
        while app.loading {
            app.drain_load_events(visible);
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("load took {:?}", t0.elapsed());

        let total = app.store.entries.len();
        let cursor_blob = app
            .store
            .entries
            .iter()
            .filter(|e| e.source == EditSource::Cursor && e.base_path.is_some())
            .count();
        let cursor_local = app
            .store
            .entries
            .iter()
            .filter(|e| {
                e.source == EditSource::Cursor && e.base_path.is_none() && !e.is_prompt()
            })
            .count();
        eprintln!(
            "entries={total} cursor blob edits={cursor_blob} cursor local-history edits={cursor_local}"
        );

        let t1 = Instant::now();
        app.search_open_content();
        for c in needle.chars() {
            app.search.query.push(c);
        }
        app.recompute_matches(visible);
        let elapsed = t1.elapsed();
        eprintln!(
            "content search {:?} for {needle:?}: {} matches, corpus {} entries, first scan {:?}",
            needle,
            app.search.matches.len(),
            app.content_cache.len(),
            elapsed
        );
        for m in app.search.matches.iter().take(8) {
            let e = &app.store.entries[m.entry_idx];
            eprintln!(
                "  {} {:?} {} sess={:?}",
                e.timestamp.format("%Y-%m-%d %H:%M"),
                e.source,
                e.file_path.display(),
                e.session_id.as_deref().map(|s| &s[..8.min(s.len())]),
            );
        }

        // A second search reuses the cache; report the warm cost too.
        let t2 = Instant::now();
        app.search.query = format!("{needle}");
        app.recompute_matches(visible);
        eprintln!("warm rescan {:?}", t2.elapsed());
    }
}

