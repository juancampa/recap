//! The state of the TUI. The server owns the history data (see the crate
//! `recap-server`). The App holds a copy of the part of that data that the
//! panels show, and all the UI state: the selection, the scroll positions,
//! the search prompts and the settings.

use crate::search_history::{SearchHistories, SearchMode};
use chrono::{DateTime, Local, NaiveDate};
use nucleo_matcher::{Config, Matcher, Utf32Str};
pub use recap_api::{Agent, KindFilter, SearchKind};
use recap_api::{
    Api, ApiError, DiffLine, DiffSummary, EditSource, EmbedStatus, Entry, EntryFields, EntryId,
    Event, FileInfo, Filter, HistoryResponse, ListItem, Page, Project, RunningSession,
    SEMANTIC_SCORE_SCALE, TriageStatus,
};
use crate::triage_files::{AgentNotes, FollowUp, Todo, TodoList, TriageFiles};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The copy of the server data that the History panel and the Projects panel
/// show. The App replaces or merges it from the API responses. It calculates
/// no data of its own.
#[derive(Default)]
pub struct View {
    /// The store version of the last history response.
    pub version: u64,
    /// The entries that pass the current filter, newest first.
    pub entries: Vec<Entry>,
    /// The position of each entry in `entries`.
    index: HashMap<EntryId, usize>,
    /// All projects, newest first.
    pub projects: Vec<Project>,
    /// The display name of each project root.
    pub project_names: HashMap<PathBuf, String>,
}

impl View {
    pub fn index_of(&self, id: &EntryId) -> Option<usize> {
        self.index.get(id).copied()
    }

    pub fn get(&self, id: &EntryId) -> Option<&Entry> {
        self.index_of(id).map(|i| &self.entries[i])
    }

    fn project_position(&self, root: &Path) -> Option<usize> {
        self.projects.iter().position(|p| p.root == root)
    }

    fn set_projects(&mut self, projects: Vec<Project>) {
        self.project_names = projects
            .iter()
            .map(|p| (p.root.clone(), p.name.clone()))
            .collect();
        self.projects = projects;
    }

    fn reindex(&mut self) {
        self.index = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
    }

    /// Replace all entries with the entries of a full history response.
    fn replace(&mut self, response: HistoryResponse) {
        self.version = response.version;
        self.entries = response.groups.into_iter().flat_map(|g| g.entries).collect();
        self.reindex();
    }

    /// Merge the response to a history request with `since`. The response
    /// holds each changed entry that passes the filter, and the id of each
    /// changed entry that does not pass it any more. The function returns
    /// the ids of all changed entries.
    fn apply(&mut self, response: HistoryResponse) -> HashSet<EntryId> {
        let fresh: Vec<Entry> = response.groups.into_iter().flat_map(|g| g.entries).collect();
        let mut changed: HashSet<EntryId> = response.removed.into_iter().collect();
        changed.extend(fresh.iter().map(|e| e.id.clone()));
        self.entries.retain(|e| !changed.contains(&e.id));
        self.entries.extend(fresh);
        self.entries
            .sort_by(|a, b| recap_api::newest_first((&a.timestamp, &a.id), (&b.timestamp, &b.id)));
        self.version = response.version;
        self.reindex();
        changed
    }
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
/// selected agent, so the panel chain gains one step. `Queue` is Agent mode
/// with only the agents that the triage agent tracks, and the Diff panel
/// shows the notes of the triage agent (see [`crate::triage_files`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    File,
    Agent,
    Queue,
}

impl Mode {
    /// The mode that the `m` key switches to. The `Q` key opens Queue
    /// mode.
    pub fn other(self) -> Mode {
        match self {
            Mode::File => Mode::Agent,
            Mode::Agent | Mode::Queue => Mode::File,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::File => "file",
            Mode::Agent => "agent",
            Mode::Queue => "queue",
        }
    }

    /// True when the Agents panel shows and History shows one agent.
    pub fn has_agents(self) -> bool {
        self != Mode::File
    }
}

/// The order of the agents in the Queue panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueOrder {
    /// The oldest agent first. A new agent goes to the end, so the other
    /// rows do not move.
    #[default]
    Created,
    /// The agent with the newest prompt first.
    LastMessage,
    /// Rank 1, 2 and 3 first, then the others in the order of creation.
    Rank,
}

impl QueueOrder {
    pub fn next(self) -> QueueOrder {
        match self {
            QueueOrder::Created => QueueOrder::LastMessage,
            QueueOrder::LastMessage => QueueOrder::Rank,
            QueueOrder::Rank => QueueOrder::Created,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            QueueOrder::Created => "created",
            QueueOrder::LastMessage => "last message",
            QueueOrder::Rank => "rank",
        }
    }
}

/// One row of the Triage panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageItem<'a> {
    FollowUp(&'a FollowUp),
    Todo(TodoList, &'a Todo),
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
    /// The session does not run, and Claude Code deleted its files. The
    /// server reads its entries from the archive copy (see
    /// [`recap_api::Entry::session_archived`]).
    Archived,
    Unknown,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPhase {
    /// No search interaction.
    Inactive,
    /// User is editing the query at the prompt.
    Typing,
    /// Query committed; matches highlighted, n/N navigates.
    Active,
}

/// One History search result. The server finds the matches and sends the
/// entry ids. The App adds the position of each entry in `view.entries`.
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub id: EntryId,
    /// Index into `view.entries`. [`App::remap_search_matches`] updates it
    /// after a merge changes the order of the entries.
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

/// `/` search in the Agents panel. The query is a case-insensitive regex, as
/// in the `f` search and the Diff panel. An agent row holds long prompts, and
/// a fuzzy match on long text matches almost every row.
pub struct AgentSearchState {
    pub phase: SearchPhase,
    pub query: String,
    /// The chain ids of the agents that match, in the order of the panel.
    pub matches: Vec<String>,
    /// Index in `matches` of the agent that the last jump selected.
    pub current: usize,
    pub highlights: bool,
    /// `None` when the query is empty or is not a valid regex.
    pub regex: Option<Regex>,
    /// The agent that was selected when the prompt opened. Esc selects it
    /// again. The field is set only while `phase == Typing`.
    restore: Option<String>,
}

impl Default for AgentSearchState {
    fn default() -> Self {
        Self {
            phase: SearchPhase::Inactive,
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            highlights: true,
            regex: None,
            restore: None,
        }
    }
}

impl AgentSearchState {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The matches show on screen: the search runs, it has matches, and Esc
    /// did not hide them.
    pub fn shows_matches(&self) -> bool {
        self.phase != SearchPhase::Inactive && self.highlights && !self.matches.is_empty()
    }

    pub fn is_match(&self, chain_id: &str) -> bool {
        self.matches.iter().any(|id| id == chain_id)
    }
}

/// Per-line searchable text for diff content search. Matches the corpus the
/// History panel's `f` search builds (Added/Removed body lines, plus Prompt
/// lines for prompt entries), so `/` in the Diff panel feels like the same
/// search restricted to whatever the Diff currently shows.
fn diff_line_text(dl: &DiffLine) -> Option<&str> {
    dl.search_text()
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
    /// The server. The App reads all history data through it.
    api: Arc<dyn Api>,
    /// The copy of the server data that the panels show. See [`View`].
    pub view: View,
    pub active_panel: Panel,

    /// Index into `view.projects`. `None` is the "All projects" row.
    pub selected_project_idx: Option<usize>,
    pub project_scroll: usize,

    /// The files of the Files panel for the selected project, newest first.
    /// The server sends them. The list has no prompts, because the
    /// `file_path` of a prompt is not a real file.
    pub files: Vec<FileInfo>,
    /// The project filter and the store version of `files`.
    files_basis: Option<ListBasis>,
    /// `None` → "All files" row. Path-based (not index) so the selection
    /// survives a change of the list.
    pub selected_file_path: Option<PathBuf>,
    pub file_scroll: usize,

    /// Global pivot. See [`Mode`].
    pub mode: Mode,

    /// Which entry kinds History shows. See [`KindFilter`].
    pub kind_filter: KindFilter,

    /// The agents of the Agents panel for the selected project, newest
    /// first. The server sends them. Entries without a session id (Neovim
    /// edits, unattributed Cursor saves) have no agent, so Agent mode does
    /// not reach them.
    all_agents: Vec<Agent>,
    /// The project filter and the store version of `all_agents`.
    agents_basis: Option<ListBasis>,
    /// The rows of the Agents panel: `all_agents` in Agent mode, and the
    /// agents of the queue in Queue mode (see [`Self::apply_agent_view`]).
    pub agents: Vec<Agent>,
    /// The files of the triage agent. `None` when the server has no triage
    /// runner.
    pub triage_files: Option<TriageFiles>,
    pub queue_order: QueueOrder,
    /// The selected row of the Triage panel (see [`Self::triage_items`]).
    pub triage_sel: usize,
    /// Chain id of the selected agent (see `Entry::chain_id`). Stored by id
    /// (not index) so the selection survives a change of the list.
    pub selected_agent: Option<String>,
    pub agent_scroll: usize,
    /// The File-mode history while Agent mode is active. A return to File
    /// mode with the same filter asks the server only for the entries that
    /// changed, because the full history can hold many MB.
    file_mode_history: Option<KeptHistory>,
    /// Selection memory for the `m` toggle. See [`ModeReturn`].
    mode_return: ModeReturn,

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

    /// True when `refilter_history` put the selection on a new list and the
    /// History panel must scroll the selection into view. The function has
    /// no panel height, so the main loop does the scroll on the next frame
    /// (see [`Self::reveal_pending_selection`]). In Agent mode the selection
    /// is the newest row, at the bottom of the list.
    history_reveal_pending: bool,

    /// The number of text lines that the Diff panel showed on the last frame.
    /// `reveal_diff_line` needs it to scroll a search match into view, but
    /// `refresh_diff` runs outside the draw, so the main loop stores it here.
    last_diff_height: usize,

    /// Counter that increases each time `rebuild_display` writes
    /// `display_rows`. It is part of the row-height cache key.
    display_gen: u64,

    /// The +/- line counts of the entries that the History panel showed.
    /// The server calculates them. A merge removes the counts of the entries
    /// that changed.
    pub diff_summaries: HashMap<EntryId, DiffSummary>,

    pub search: SearchState,
    pub project_search: PanelSearchState,
    pub file_search: PanelSearchState,
    pub diff_search: DiffSearchState,
    pub agent_search: AgentSearchState,
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

    /// The running Claude Code sessions, by session id. The server scans the
    /// session registry and sends each change as an event. The sessions
    /// drive the "running" indicator column and the `o` (focus/resume) key.
    pub running: HashMap<String, RunningSession>,

    /// True while the server loads its sources. Drives the `[loading]`
    /// indicator in the help bar.
    pub loading: bool,

    /// The state of the triage runner of the server. `None` when the server
    /// runs without a triage directory.
    pub triage: Option<TriageStatus>,

    /// The state of the semantic search on the server.
    pub embed: EmbedStatus,

    /// The error of the last API call that failed. The help bar shows it.
    /// The next successful connection clears it.
    pub error: Option<String>,

    /// Persisted project selection (by root path) that the projects list
    /// does not hold yet. While the server loads, the project can arrive
    /// later. Cleared when the server finishes the load.
    pending_project_restore: Option<PathBuf>,
    /// Persisted file selection whose entries the server did not load yet.
    /// Keeps `selected_file_path` from being dropped by `recompute_files`.
    /// Cleared when the server finishes the load.
    pending_file_restore: Option<PathBuf>,
    /// Persisted agent selection whose entries the server did not load yet.
    /// [`Self::recompute_agents`] keeps the selection on it until the
    /// server finishes the load.
    pending_agent_restore: Option<String>,
}

/// The entries of a history view that the History panel does not show now.
#[derive(Debug)]
struct KeptHistory {
    filter: Filter,
    version: u64,
    entries: Vec<Entry>,
}

/// The project filter of a side list, and the store version of the list.
#[derive(Debug, Clone, PartialEq)]
struct ListBasis {
    project: Option<PathBuf>,
    version: u64,
}

/// Read a side list from the server into `list`. `basis` describes `list`.
/// When `basis` has the project `project`, the function sends its version,
/// and the server can send only the change to the list. `fetch` takes that
/// version and returns the version of the answer, the version that the
/// change applies to, the items, and the removed keys. On an error, `list`
/// is empty.
fn read_list<T: ListItem>(
    list: &mut Vec<T>,
    basis: &mut Option<ListBasis>,
    project: Option<PathBuf>,
    fetch: impl FnOnce(Option<u64>) -> Result<(u64, Option<u64>, Vec<T>, Vec<T::Key>), ApiError>,
) -> Result<(), ApiError> {
    let since = basis
        .as_ref()
        .filter(|b| b.project == project)
        .map(|b| b.version);
    match fetch(since) {
        Ok((version, change_since, items, removed)) => {
            match change_since {
                Some(_) => recap_api::apply_list_change(list, items, &removed),
                None => *list = items,
            }
            *basis = Some(ListBasis { project, version });
            Ok(())
        }
        Err(e) => {
            list.clear();
            *basis = None;
            Err(e)
        }
    }
}

impl App {
    pub fn new(api: Arc<dyn Api>) -> Self {
        let mut app = Self {
            api,
            view: View::default(),
            active_panel: Panel::History,
            selected_project_idx: None,
            project_scroll: 0,
            files: Vec::new(),
            files_basis: None,
            selected_file_path: None,
            file_scroll: 0,
            mode: Mode::File,
            kind_filter: KindFilter::All,
            all_agents: Vec::new(),
            agents_basis: None,
            agents: Vec::new(),
            triage_files: None,
            queue_order: QueueOrder::default(),
            triage_sel: 0,
            selected_agent: None,
            agent_scroll: 0,
            file_mode_history: None,
            mode_return: ModeReturn::default(),
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
            history_reveal_pending: false,
            last_diff_height: 0,
            display_gen: 0,
            diff_summaries: HashMap::new(),
            search: SearchState::default(),
            project_search: PanelSearchState::default(),
            file_search: PanelSearchState::default(),
            diff_search: DiffSearchState::default(),
            agent_search: AgentSearchState::default(),
            search_history: SearchHistories::default(),
            auto_expand: false,
            running: HashMap::new(),
            triage: None,
            loading: false,
            embed: EmbedStatus::Off,
            error: None,
            pending_project_restore: None,
            pending_file_restore: None,
            pending_agent_restore: None,
        };
        app.load_server_state();
        app.refresh_projects();
        app.refilter();
        app
    }

    /// Seed selection/toggles from persisted state, then load the views for
    /// that selection. While the server loads, a project, file or agent
    /// that it did not load yet waits in the `pending_*_restore` fields.
    /// `absolute_time` is left to the caller so a CLI override can take
    /// precedence.
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
        if self.mode.has_agents() {
            self.active_panel = Panel::Agents;
        }
        self.refresh_projects();
        if !self.loading {
            // The server holds all its data, so a selection that it does not
            // know is gone. The refilter below drops it.
            self.clear_pending_restores();
        }
        self.refilter();
    }

    /// Snapshot the bits of UI state worth surviving a restart.
    pub fn persisted_state(&self) -> crate::persist::PersistedState {
        let selected_project = self
            .selected_project_root()
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
            agent_mode: self.mode.has_agents(),
            selected_agent,
            search_history: self.search_history.snapshot(),
            settings: self.settings,
        }
    }

    // ── Server state ──────────────────────────────────────────────────────────

    /// Record the error of an API call. The help bar shows it.
    fn fail(&mut self, error: ApiError) {
        self.error = Some(error.0);
    }

    /// Read the server state that the event stream sends only when it
    /// changes: the load state, the semantic search state and the running
    /// sessions.
    fn load_server_state(&mut self) {
        match self.api.status() {
            Ok(status) => {
                self.loading = status.loading;
                self.embed = status.embed;
                self.set_triage(status.triage);
                self.error = None;
            }
            Err(e) => self.fail(e),
        }
        match self.api.running() {
            Ok(sessions) => self.set_running(sessions),
            Err(e) => self.fail(e),
        }
    }

    /// Keep the state of the triage runner, and read the triage files when
    /// the directory is new. A new run changes the row of the triage agent.
    fn set_triage(&mut self, status: Option<TriageStatus>) {
        let dir = status.as_ref().map(|s| s.dir.clone());
        let new_dir = self.triage_files.as_ref().map(|f| &f.dir) != dir.as_ref();
        if new_dir {
            self.triage_files = dir.map(|d| TriageFiles::read(&d));
        }
        let run_id = |s: &Option<TriageStatus>| {
            s.as_ref()
                .and_then(|s| s.run.as_ref())
                .map(|r| r.session_id.clone())
        };
        let new_run = run_id(&self.triage) != run_id(&status);
        self.triage = status;
        if new_dir || new_run {
            self.apply_agent_view();
        }
    }

    /// Ask the server for a review of the agents of the last 24 hours (the
    /// `R` key in Queue mode).
    pub fn request_triage_review(&mut self) {
        if let Err(e) = self.api.triage_review() {
            self.fail(e);
        }
    }

    fn set_running(&mut self, sessions: Vec<RunningSession>) {
        self.running = sessions
            .into_iter()
            .map(|s| (s.session_id.clone(), s))
            .collect();
    }

    /// Read the projects list again, and keep the selection on the same
    /// project. The selection stays on the "All projects" row until the
    /// selected project exists.
    fn refresh_projects(&mut self) {
        let desired = self
            .selected_project_root()
            .or_else(|| self.pending_project_restore.clone());
        match self.api.projects() {
            Ok(response) => self.view.set_projects(response.projects),
            Err(e) => self.fail(e),
        }
        self.selected_project_idx = desired.and_then(|p| self.view.project_position(&p));
    }

    fn clear_pending_restores(&mut self) -> bool {
        self.pending_project_restore.take().is_some()
            | self.pending_file_restore.take().is_some()
            | self.pending_agent_restore.take().is_some()
    }

    /// Apply one event from the server.
    pub fn on_event(&mut self, event: Event, visible: usize) {
        match event {
            Event::Changed { version } => {
                if version > self.view.version {
                    self.pull_changes(visible);
                }
            }
            Event::Loading { loading } => {
                self.loading = loading;
                // The load is complete, so the pending restores had their
                // chance. Drop a selection that never arrived, so it does not
                // filter the panels down to nothing.
                if !loading && self.clear_pending_restores() {
                    self.refresh_projects();
                    let selection = self.current_entry_id();
                    self.refilter_preserving(selection, visible);
                }
            }
            Event::Embed(status) => self.embed = status,
            Event::Running { sessions } => self.set_running(sessions),
            Event::Triage(status) => self.set_triage(Some(status)),
        }
    }

    /// Read all the views again after the connection to the server came
    /// back. The server can have restarted, so the store version can be
    /// lower than before.
    pub fn on_reconnect(&mut self, visible: usize) {
        // The versions of a new server process do not identify the same
        // lists, so the next reads get the full lists.
        self.files_basis = None;
        self.agents_basis = None;
        self.file_mode_history = None;
        self.load_server_state();
        self.refresh_projects();
        let selection = self.current_entry_id();
        self.refilter_preserving(selection, visible);
    }

    /// Record that the connection to the server stopped.
    pub fn on_disconnect(&mut self, error: String) {
        self.error = Some(error);
    }

    /// Ask the server to load all sources now (the `r` key).
    pub fn reload(&mut self, _visible: usize) {
        if let Err(e) = self.api.reload() {
            self.fail(e);
        }
    }

    /// Ask the server to write the files of the archived session of `entry`
    /// back to the Claude Code projects directory. Returns false and shows
    /// the error when the server cannot do it.
    pub fn restore_session(&mut self, entry: &Entry) -> bool {
        let Some(session_id) = entry.session_id.as_deref() else {
            return false;
        };
        match self.api.restore_session(session_id) {
            Ok(()) => true,
            Err(e) => {
                self.fail(e);
                false
            }
        }
    }

    /// Merge the entries that changed after `view.version`. The selection,
    /// the expanded group and the search matches stay on the same entries.
    fn pull_changes(&mut self, visible: usize) {
        let selection = self.current_entry_id();
        let before = self.filter();
        self.refresh_projects();
        self.recompute_files();
        self.recompute_agents();
        if self.filter() != before {
            // The selected project, file or agent is gone, so the History
            // panel shows another set of entries.
            self.search_clear();
            self.refilter_preserving(selection, visible);
            return;
        }

        let page = Page {
            since: Some(self.view.version),
            limit: None,
        };
        let changed = match self.api.history(&before, &page) {
            Ok(response) => self.view.apply(response),
            Err(e) => return self.fail(e),
        };
        for id in &changed {
            self.diff_summaries.remove(id);
        }
        self.remap_search_matches();
        // The side lists can grow or change their order, so the positions
        // in their matches can be wrong.
        self.project_search.clear();
        self.file_search.clear();

        // Agent mode reads like a chat: when the panel shows the last row,
        // it keeps the last row in view as rows arrive or grow. When the
        // cursor is on the last row, the cursor moves to the new last row.
        // The row heights are still the heights of the last frame here, so
        // they describe the rows that the user sees.
        let stick = self.mode.has_agents() && self.history_rows_below(visible) == 0;
        let follow = stick && self.sel_display_row() + 1 == self.display_rows.len();

        let expanded = self.current_group_ids();
        self.regroup(if follow { None } else { selection.clone() });
        let moved = self.current_entry_id() != selection;
        if moved {
            self.diff_scroll = 0;
        }
        self.adjust_scroll(visible);
        if stick {
            self.scroll_history_to_bottom(visible);
        }
        // Build the diff again only when the text can be different, so a
        // change in another entry does not drop the Diff scroll or the
        // mouse selection.
        if moved || self.current_group_ids() != expanded || expanded.iter().any(|id| changed.contains(id)) {
            self.refresh_diff();
            self.diff_scroll = self.diff_scroll.min(self.diff_lines.len().saturating_sub(1));
        }
    }

    /// The ids of the entries in the selected group.
    fn current_group_ids(&self) -> Vec<EntryId> {
        self.groups
            .get(self.sel_group)
            .map(|g| {
                g.entry_indices
                    .iter()
                    .map(|&i| self.view.entries[i].id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Put each search match on the new position of its entry, and drop the
    /// matches whose entry is gone.
    fn remap_search_matches(&mut self) {
        let current = self.search.matches.get(self.search.current).map(|m| m.id.clone());
        let view = &self.view;
        self.search.matches.retain_mut(|m| match view.index_of(&m.id) {
            Some(i) => {
                m.entry_idx = i;
                true
            }
            None => false,
        });
        self.search.current = current
            .and_then(|id| self.search.matches.iter().position(|m| m.id == id))
            .unwrap_or(0);
    }

    /// Like `refilter` but tries to keep selection on the entry `selection`.
    /// Falls back to row 0 if not found.
    fn refilter_preserving(&mut self, selection: Option<EntryId>, visible: usize) {
        self.recompute_files();
        self.recompute_agents();
        self.load_history();
        self.regroup(selection);
        self.diff_scroll = 0;
        self.adjust_scroll(visible);
        self.refresh_diff();
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// The root of the selected project.
    fn selected_project_root(&self) -> Option<PathBuf> {
        self.selected_project_idx
            .and_then(|i| self.view.projects.get(i))
            .map(|p| p.root.clone())
    }

    /// The filter of the History panel, as the server applies it.
    fn filter(&self) -> Filter {
        Filter {
            project: self.selected_project_root(),
            file: self.selected_file_path.clone(),
            // Agent mode narrows History to one agent; File mode ignores
            // this. With no agent selected, Agent mode reaches no entry at
            // all: the server matches the empty id to no entry.
            agent: match self.mode {
                Mode::Agent | Mode::Queue => Some(self.selected_agent.clone().unwrap_or_default()),
                Mode::File => None,
            },
            kind: self.kind_filter,
            after: None,
        }
    }

    /// Read all entries that pass the filter from the server. In File mode,
    /// when `file_mode_history` has the same filter, read only the entries
    /// that changed after it.
    fn load_history(&mut self) {
        let filter = self.filter();
        if self.mode == Mode::File
            && let Some(kept) = self.file_mode_history.take()
            && kept.filter == filter
        {
            self.view.entries = kept.entries;
            self.view.reindex();
            self.view.version = kept.version;
            let page = Page {
                since: Some(kept.version),
                limit: None,
            };
            match self.api.history(&filter, &page) {
                Ok(response) => {
                    for id in self.view.apply(response) {
                        self.diff_summaries.remove(&id);
                    }
                    return;
                }
                Err(e) => self.fail(e),
            }
        }
        match self.api.history(&filter, &Page::default()) {
            Ok(response) => self.view.replace(response),
            Err(e) => {
                self.fail(e);
                self.view.replace(HistoryResponse::default());
            }
        }
    }

    /// Build the groups from `view.entries` again, and put the selection on
    /// the entry `selection`. The selection falls back to the newest row.
    ///
    /// File mode lists the newest entry first. Agent mode lists the oldest
    /// entry first, so the turns of the agent read from top to bottom and
    /// the newest turn is at the bottom.
    fn regroup(&mut self, selection: Option<EntryId>) {
        self.groups = recap_api::group_runs(&self.view.entries)
            .into_iter()
            .map(|run| HistoryGroup {
                entry_indices: run.collect(),
                expanded: false,
            })
            .collect();
        if self.mode.has_agents() {
            self.groups.reverse();
            for group in &mut self.groups {
                group.entry_indices.reverse();
            }
        }
        let newest = match self.mode {
            Mode::File => (0, 0),
            Mode::Agent | Mode::Queue => {
                let last = self.groups.len().saturating_sub(1);
                let child = self.groups.get(last).map_or(0, |g| g.entry_indices.len() - 1);
                (last, child)
            }
        };
        let target = selection
            .and_then(|id| self.view.index_of(&id))
            .and_then(|idx| {
                self.groups.iter().enumerate().find_map(|(gi, g)| {
                    g.entry_indices.iter().position(|&ei| ei == idx).map(|ci| (gi, ci))
                })
            });
        let (sg, sc) = target.unwrap_or(newest);
        self.sel_group = sg;
        self.sel_child = sc;
        if !self.groups.is_empty() && self.effective_auto_expand() {
            self.groups[self.sel_group].expanded = true;
        }
        self.rebuild_display();
    }

    /// Read the Agents panel list from the server. The list honors the
    /// project filter, so the panel mirrors what History can reach.
    fn recompute_agents(&mut self) {
        let project = self.selected_project_root();
        let api = &self.api;
        let mut agents = std::mem::take(&mut self.all_agents);
        let read = read_list(&mut agents, &mut self.agents_basis, project.clone(), |since| {
            api.agents(project.as_deref(), since)
                .map(|r| (r.version, r.since, r.agents, r.removed))
        });
        if let Err(e) = read {
            self.fail(e);
        }
        self.all_agents = agents;
        self.apply_agent_view();
        let agents = std::mem::take(&mut self.agents);

        // Keep the selection on an agent that still exists, and give Agent
        // mode a selection as soon as the first agent loads. A pending
        // restore is exempt: its entries may not have loaded yet.
        let still_there = self
            .selected_agent
            .as_deref()
            .is_some_and(|id| agents.iter().any(|a| a.chain_id == id));
        let restoring = self.pending_agent_restore.is_some()
            && self.pending_agent_restore == self.selected_agent;
        if !still_there && !restoring {
            self.selected_agent = agents.first().map(|a| a.chain_id.clone());
            self.agent_scroll = 0;
        }
        self.agents = agents;
        // The list can have other agents or another order now.
        if self.agent_search.phase != SearchPhase::Inactive {
            self.refresh_agent_matches();
        }
    }

    /// Position of a prompt inside its own agent session, counted from 1.
    /// `None` for a file change, or for a prompt with no session. The server
    /// calculates it.
    pub fn prompt_ordinal(&self, entry: &Entry) -> Option<usize> {
        entry.ordinal
    }

    /// Read the Files panel list from the server. The list honors the
    /// project filter, so the Files list mirrors what History can reach.
    fn recompute_files(&mut self) {
        let project = self.selected_project_root();
        let api = &self.api;
        let mut files = std::mem::take(&mut self.files);
        let read = read_list(&mut files, &mut self.files_basis, project.clone(), |since| {
            api.files(project.as_deref(), since)
                .map(|r| (r.version, r.since, r.files, r.removed))
        });
        if let Err(e) = read {
            self.fail(e);
        }

        // Drop the file selection if it's no longer in the list (e.g.
        // project filter changed away from it). A pending restore is exempt:
        // its entries may simply not have loaded yet.
        if let Some(sel) = &self.selected_file_path {
            let restoring = self.pending_file_restore.as_deref() == Some(sel.as_path());
            if !files.iter().any(|f| &f.path == sel) && !restoring {
                self.selected_file_path = None;
                self.file_scroll = 0;
            }
        }
        self.files = files;
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
            let entry = &self.view.entries[self.row_entry_idx(idx)];
            if entry.project_roots.is_empty() {
                continue;
            }
            let chars: usize = entry
                .project_roots
                .iter()
                .map(|root| {
                    let name = match self.view.project_names.get(root) {
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
        if !self.history_columns().show_project {
            0
        } else if self.settings.align_project {
            self.project_col_chars
        } else {
            own_chars
        }
    }

    pub fn refilter(&mut self) {
        self.recompute_files();
        self.recompute_agents();
        self.refilter_history();
    }

    /// Read the History panel again for the current filter. The Files and
    /// Agents lists depend only on the project, so a change of the selected
    /// file or agent calls this function and does not read those lists again.
    fn refilter_history(&mut self) {
        self.load_history();
        self.regroup(None);
        self.history_scroll = 0;
        self.history_reveal_pending = true;
        self.diff_scroll = 0;
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

        // Collapsed multi-entry group → the server concatenates every
        // child's diff, separated by per-child header rows.
        let ids: Vec<EntryId> = if !self.effective_auto_expand() && group.entry_indices.len() > 1 {
            group
                .entry_indices
                .iter()
                .map(|&i| self.view.entries[i].id.clone())
                .collect()
        } else {
            let Some(&entry_idx) = group.entry_indices.get(self.sel_child) else {
                return vec![DiffLine::Header("(no selection)".to_string())];
            };
            vec![self.view.entries[entry_idx].id.clone()]
        };
        match self.api.diff(&ids) {
            Ok(lines) => lines,
            Err(e) => vec![DiffLine::Header(format!("(cannot read the diff: {e})"))],
        }
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

    /// Scroll the selection into view after `refilter_history`. The main
    /// loop calls this on each frame, after it computed the row heights.
    pub fn reveal_pending_selection(&mut self, visible: usize) {
        if std::mem::take(&mut self.history_reveal_pending) {
            self.adjust_scroll(visible);
        }
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
        Some(self.view.entries[self.row_entry_idx(top)].timestamp.date_naive())
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

    /// True if display row `idx` is a prompt in Agent mode. The History panel
    /// then draws one empty line above the prompt, so each turn of the agent
    /// stands apart.
    pub fn row_gap(&self, idx: usize) -> bool {
        self.mode.has_agents()
            && matches!(self.display_rows.get(idx), Some(DisplayRow::Group(_)))
            && self.view.entries[self.row_entry_idx(idx)].is_prompt()
    }

    /// The number of display rows that the History panel does not show
    /// completely: the rows below the last complete row in `visible` lines.
    pub fn history_rows_below(&self, visible: usize) -> usize {
        let lines = self.row_lines(visible);
        let mut used = 0usize;
        for idx in self.history_scroll..self.display_rows.len() {
            used += self.row_height(idx);
            if used > lines {
                return self.display_rows.len() - idx;
            }
        }
        0
    }

    /// Scroll the History panel so the last row is at the bottom. The
    /// selection stays in view.
    fn scroll_history_to_bottom(&mut self, visible: usize) {
        self.refresh_row_heights(visible);
        let lines = self.row_lines(visible);
        let last = self.display_rows.len().saturating_sub(1);
        self.history_scroll = self.max_history_scroll(lines).min(last);
        self.adjust_scroll(visible);
    }

    /// True if display row `idx` is the first row of its day, and the user
    /// turned the date row on. The History panel then draws one extra line
    /// above the row, and [`Self::recompute_row_heights`] adds that line to
    /// the height of the row.
    pub fn row_starts_day(&self, idx: usize) -> bool {
        if !self.settings.date_separator || idx >= self.display_rows.len() {
            return false;
        }
        let day = self.view.entries[self.row_entry_idx(idx)]
            .timestamp
            .date_naive();
        match idx.checked_sub(1) {
            None => true,
            Some(prev) => {
                self.view.entries[self.row_entry_idx(prev)]
                    .timestamp
                    .date_naive()
                    != day
            }
        }
    }

    /// The number of display rows, from `history_scroll` down, that the
    /// History panel draws in `lines` lines. The count includes the last
    /// row that fits only in part: the panel draws the lines of that row
    /// that fit.
    pub fn history_rows_fit(&self, lines: usize) -> usize {
        let mut count = 0usize;
        let mut used = 0usize;
        for idx in self.history_scroll..self.display_rows.len() {
            if used >= lines {
                break;
            }
            count += 1;
            used += self.row_height(idx);
        }
        count
    }

    /// Map a 0-based line offset below `history_scroll` to the display-row
    /// index that occupies it (accounting for multi-line prompt rows), of the
    /// rows that the panel draws in its `visible` lines. Used to translate a
    /// mouse click into a selection.
    pub fn history_item_at_line(&self, line_offset: usize, visible: usize) -> Option<usize> {
        // The sticky date row is above the rows. A click on it selects no row.
        let sticky = self.sticky_date().is_some() as usize;
        let line_offset = line_offset.checked_sub(sticky)?;
        let drawn = self.history_rows_fit(visible.saturating_sub(sticky));
        let mut acc = 0usize;
        for idx in self.history_scroll..self.history_scroll + drawn {
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

    /// The History column settings for the current mode. In Agent mode the
    /// Agents panel already shows the source, the title and the project of
    /// each agent, so the History panel does not draw these three columns.
    pub fn history_columns(&self) -> crate::settings::Settings {
        match self.mode {
            Mode::File => self.settings,
            Mode::Agent | Mode::Queue => crate::settings::Settings {
                show_source: false,
                show_title: false,
                show_project: false,
                ..self.settings
            },
        }
    }

    pub fn recompute_row_heights(&mut self, list_width: usize, list_height: usize) {
        self.last_list_width = list_width;
        let ts_width = self.estimate_ts_width(list_height);
        let key = (
            list_width,
            ts_width,
            self.prompt_lines,
            self.display_gen,
            self.history_columns().cache_bits(),
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
                    let entry = &self.view.entries[g.entry_indices[0]];
                    if g.entry_indices.len() == 1 && entry.is_prompt() {
                        self.prompt_row_height(entry, list_width, ts_width)
                    } else {
                        1
                    }
                }
                DisplayRow::Child { .. } => 1,
            };
            // The date row of a day sits on top of the first row of that day.
            heights.push(h + self.row_starts_day(idx) as usize + self.row_gap(idx) as usize);
        }
        self.hist_row_heights = heights;
        self.row_heights_key = Some(key);
    }

    /// Number of wrapped lines a prompt row occupies at the given panel
    /// geometry — mirrors the wrapping the renderer performs so scroll math
    /// and rendering agree.
    fn prompt_row_height(&self, entry: &Entry, list_width: usize, ts_width: usize) -> usize {
        let (chain, _) = entry.display_path_segments(&self.view.project_names);
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
        let text_start = crate::ui::history_prefix_cols(ts_width, list_width, &self.history_columns())
            + chain_chars
            + crate::ui::prompt_prefix_width(ordinal, entry.subagent);
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
            .map(|(idx, _)| self.view.entries[self.row_entry_idx(idx)].timestamp)
            .collect();
        crate::reltime::align_relative_times(&tss)
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
            // Agent mode lists the oldest entry first, so the gap is
            // negative there.
            let gap = self.view.entries[prev_oldest_idx]
                .timestamp
                .signed_duration_since(self.view.entries[cur_newest_idx].timestamp);
            if gap.num_minutes().abs() > 60 {
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
        let mut missing: Vec<EntryId> = Vec::new();
        for row_idx in self.history_scroll..end {
            let entry_idx = match self.display_rows[row_idx] {
                DisplayRow::Group(gi) => self.groups[gi].entry_indices[0],
                DisplayRow::Child {
                    group_idx,
                    child_local,
                } => self.groups[group_idx].entry_indices[child_local],
            };
            let id = &self.view.entries[entry_idx].id;
            if !self.diff_summaries.contains_key(id) {
                missing.push(id.clone());
            }
        }
        if missing.is_empty() {
            return;
        }
        match self.api.summaries(&missing) {
            Ok(summaries) => {
                // An id that the server does not know gets an empty count, so
                // the next frame does not ask for it again.
                for id in missing {
                    let summary = summaries.get(&id).copied().unwrap_or_default();
                    self.diff_summaries.insert(id, summary);
                }
            }
            Err(e) => self.fail(e),
        }
    }

    pub fn current_entry(&self) -> Option<&Entry> {
        let group = self.groups.get(self.sel_group)?;
        let &idx = group.entry_indices.get(self.sel_child)?;
        self.view.entries.get(idx)
    }

    /// Index into `view.entries` of the current selection (parallels
    /// [`Self::current_entry`]).
    pub fn current_entry_idx(&self) -> Option<usize> {
        let group = self.groups.get(self.sel_group)?;
        group.entry_indices.get(self.sel_child).copied()
    }

    /// Whether the agent session behind `entry_idx` still runs. Only Claude
    /// Code publishes a session registry, so every other source reports
    /// [`RunState::Unknown`] and the renderer leaves its colors bright.
    pub fn entry_run_state(&self, entry_idx: usize) -> RunState {
        let Some(entry) = self.view.entries.get(entry_idx) else {
            return RunState::Unknown;
        };
        self.run_state_of(entry.source, entry.session_id.as_deref(), entry.session_archived)
    }

    /// [`Self::entry_run_state`] for a source and session id that the caller
    /// already holds (the Agents panel has no entry index).
    pub fn run_state_of(&self, source: EditSource, session_id: Option<&str>, archived: bool) -> RunState {
        if source != EditSource::ClaudeCode {
            return RunState::Unknown;
        }
        match session_id {
            None => RunState::Unknown,
            Some(sid) if self.running.contains_key(sid) => RunState::Running,
            Some(_) if archived => RunState::Archived,
            Some(_) => RunState::Stopped,
        }
    }

    /// Whether the entry at `entry_idx` originated from a Claude Code session
    /// that is currently running.
    pub fn is_entry_running(&self, entry_idx: usize) -> bool {
        self.view
            .entries
            .get(entry_idx)
            .and_then(|e| e.session_id.as_deref())
            .map(|sid| self.running.contains_key(sid))
            .unwrap_or(false)
    }

    /// Absolute path for the currently selected row in the active panel.
    /// Returns None when nothing is selected, or the active panel is Diff.
    pub fn selected_path(&self) -> Option<PathBuf> {
        match self.active_panel {
            Panel::Projects => self
                .selected_project_idx
                .and_then(|i| self.view.projects.get(i).map(|p| p.root.clone())),
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

    /// True if `O` has a file to open. A prompt row and a compaction row
    /// have none.
    pub fn can_open_file(&self) -> bool {
        self.current_entry().is_some_and(|e| e.is_file_change())
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    pub fn history_down(&mut self, visible: usize) {
        if self.select_next_history_row() {
            self.after_history_move(visible);
        }
    }

    pub fn history_up(&mut self, visible: usize) {
        if self.select_prev_history_row() {
            self.after_history_move(visible);
        }
    }

    pub fn history_page_down(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        if (0..steps).take_while(|_| self.select_next_history_row()).count() > 0 {
            self.after_history_move(visible);
        }
    }

    pub fn history_page_up(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        if (0..steps).take_while(|_| self.select_prev_history_row()).count() > 0 {
            self.after_history_move(visible);
        }
    }

    /// Select the next History row. The function returns false if the
    /// selection is on the last row. It does not scroll and does not fetch
    /// the diff, so a page move does these two steps one time only.
    fn select_next_history_row(&mut self) -> bool {
        if self.groups.is_empty() {
            return false;
        }
        let group_len = self.groups[self.sel_group].entry_indices.len();
        let auto_expand = self.effective_auto_expand();
        if auto_expand && self.sel_child + 1 < group_len {
            self.sel_child += 1;
        } else if self.sel_group + 1 < self.groups.len() {
            let leaving = self.sel_group;
            self.sel_group += 1;
            self.sel_child = 0;
            // Only auto-expand changes the display rows. A rebuild makes the
            // row-height cache invalid, and the next `adjust_scroll` then
            // wraps the text of every row again.
            if auto_expand {
                self.groups[leaving].expanded = false;
                self.groups[self.sel_group].expanded = true;
                self.rebuild_display();
            }
        } else {
            return false;
        }
        true
    }

    /// Select the previous History row. The function returns false if the
    /// selection is on the first row. See `select_next_history_row`.
    fn select_prev_history_row(&mut self) -> bool {
        if self.groups.is_empty() {
            return false;
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
                self.rebuild_display();
            } else {
                self.sel_child = 0;
            }
        } else {
            return false;
        }
        true
    }

    /// Scroll the History panel to the selected row and show its diff.
    fn after_history_move(&mut self, visible: usize) {
        self.diff_scroll = 0;
        self.adjust_scroll(visible);
        self.refresh_diff();
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
        let max = self.view.projects.len();
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
        let n = self.view.projects.len();
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
        self.files.iter().position(|p| &p.path == f)
    }

    pub fn file_down(&mut self, visible: usize) {
        let max = self.files.len();
        let current = self.sel_file_idx().map(|i| i + 1).unwrap_or(0);
        if current < max {
            self.selected_file_path = Some(self.files[current].path.clone());
            let display_idx = current + 1; // +1 for "All files" row
            if display_idx >= self.file_scroll + visible {
                self.file_scroll += 1;
            }
            self.refilter_history();
        }
    }

    pub fn file_up(&mut self, _visible: usize) {
        match self.sel_file_idx() {
            None => {}
            Some(0) => {
                self.selected_file_path = None;
                self.file_scroll = self.file_scroll.saturating_sub(1);
                self.refilter_history();
            }
            Some(i) => {
                self.selected_file_path = Some(self.files[i - 1].path.clone());
                let display_idx = i; // i-1 file = display row i (0 = "All files")
                if display_idx < self.file_scroll {
                    self.file_scroll = display_idx;
                }
                self.refilter_history();
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
        self.refilter_history();
    }

    pub fn file_bottom(&mut self, visible: usize) {
        let n = self.files.len();
        if n == 0 {
            return;
        }
        self.selected_file_path = Some(self.files[n - 1].path.clone());
        let total_rows = n + 1;
        self.file_scroll = total_rows.saturating_sub(visible);
        self.refilter_history();
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
            Mode::Agent | Mode::Queue => &[
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

    /// Switch between File mode and Agent mode. Queue mode switches to File
    /// mode. The switch keeps the History panel active if it was active
    /// before, because the user reads the rows there. From any other panel,
    /// Agent mode starts on the Agents panel, because the selection there
    /// drives every panel to its right. Leaving Agent mode drops the agent
    /// filter.
    pub fn toggle_mode(&mut self, visible: usize, visible_agents: usize) {
        // The row under the cursor right now. Both directions keep the user
        // on it, so the toggle never moves them away from their work.
        let current = self.current_entry_id();
        let current_session = self
            .current_entry()
            .and_then(|e| e.chain_id.clone());

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
            Mode::Agent | Mode::Queue => {
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
        if leaving_file {
            self.file_mode_history = Some(KeptHistory {
                filter: self.filter(),
                version: self.view.version,
                entries: std::mem::take(&mut self.view.entries),
            });
        }
        self.mode = if leaving_file { Mode::Agent } else { Mode::File };
        self.apply_agent_view();
        self.active_panel = match self.mode {
            Mode::Agent | Mode::Queue if stay_on_history => Panel::History,
            Mode::Agent | Mode::Queue => Panel::Agents,
            Mode::File => Panel::History,
        };
        // The filtered set changes, so any search result now points at rows
        // the panel no longer lists.
        self.search_clear();
        self.agent_search.clear();
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

    /// Switch to Queue mode, on the Agents panel. The `--queue` option and
    /// the `Q` key call this function.
    pub fn enter_queue(&mut self, visible: usize, visible_agents: usize) {
        if self.mode == Mode::File {
            self.file_mode_history = Some(KeptHistory {
                filter: self.filter(),
                version: self.view.version,
                entries: std::mem::take(&mut self.view.entries),
            });
        }
        self.mode = Mode::Queue;
        self.active_panel = Panel::Agents;
        self.agent_search.clear();
        self.search_clear();
        self.reapply_agent_view(visible);
        self.refilter_history();
        self.center_history_selection(visible);
        self.ensure_agent_visible(visible_agents);
    }

    /// Fill `agents` from `all_agents` for the current mode. Queue mode
    /// keeps the agents that have a triage file and that the user did not
    /// hide for their current turn. The first row of Queue mode is the
    /// current or last run of the triage agent.
    fn apply_agent_view(&mut self) {
        if self.mode != Mode::Queue {
            self.agents = self.all_agents.clone();
            return;
        }
        let Some(files) = &self.triage_files else {
            self.agents = Vec::new();
            return;
        };
        let mut agents: Vec<Agent> = self
            .all_agents
            .iter()
            .filter(|a| {
                files
                    .agents
                    .get(&a.chain_id)
                    .is_some_and(|n| n.hidden.is_none() || n.hidden != a.last_ordinal)
            })
            .cloned()
            .collect();
        let created = |a: &Agent| a.first_ts.unwrap_or(a.last_ts);
        match self.queue_order {
            QueueOrder::Created => agents.sort_by_key(|a| (created(a), a.chain_id.clone())),
            QueueOrder::LastMessage => agents.sort_by(|a, b| b.last_ts.cmp(&a.last_ts)),
            QueueOrder::Rank => agents.sort_by_key(|a| {
                (files.rank(&a.chain_id).unwrap_or(usize::MAX), created(a), a.chain_id.clone())
            }),
        }
        // The transcript of a new run shows after the next load of the
        // server.
        if let Some(run) = self.all_agents.iter().find(|a| self.is_triage_run(a)) {
            agents.insert(0, run.clone());
        }
        self.agents = agents;
    }

    /// True when `agent` is the current or last run of the triage agent.
    pub fn is_triage_run(&self, agent: &Agent) -> bool {
        self.triage
            .as_ref()
            .and_then(|t| t.run.as_ref())
            .is_some_and(|r| r.session_id == agent.chain_id)
    }

    /// Select the item `index` of the Triage panel. A click on the check box
    /// of a TODO item also toggles the item.
    pub fn click_triage_item(&mut self, index: usize, on_box: bool, visible: usize) {
        self.triage_sel = index;
        self.clamp_triage_sel();
        if on_box {
            self.toggle_selected_todo(visible);
        }
    }

    /// Fill `agents` again, and keep a selection that is in the list. When
    /// the selected agent changes, History shows the new agent.
    fn reapply_agent_view(&mut self, visible: usize) {
        let before = self.selected_agent.clone();
        self.apply_agent_view();
        let still_there = self
            .selected_agent
            .as_deref()
            .is_some_and(|id| self.agents.iter().any(|a| a.chain_id == id));
        if !still_there {
            self.selected_agent = self.agents.first().map(|a| a.chain_id.clone());
            self.agent_scroll = 0;
        }
        self.clamp_triage_sel();
        if self.selected_agent != before {
            self.refilter_history();
            self.adjust_scroll(visible);
        }
    }

    /// Read the triage files again when one of them changed. The main loop
    /// calls this function about once a second.
    pub fn refresh_triage_files(&mut self, visible: usize) {
        let Some(files) = &self.triage_files else {
            return;
        };
        if !files.changed() {
            return;
        }
        self.triage_files = Some(TriageFiles::read(&files.dir.clone()));
        self.reapply_agent_view(visible);
    }

    /// The notes of the triage agent for `agent`.
    pub fn notes_of(&self, agent: &Agent) -> Option<&AgentNotes> {
        self.triage_files.as_ref()?.agents.get(&agent.chain_id)
    }

    /// The rank of `agent` in the queue: 1, 2 or 3.
    pub fn rank_of(&self, agent: &Agent) -> Option<usize> {
        self.triage_files.as_ref()?.rank(&agent.chain_id)
    }

    /// True when the triage agent or the user resolved the current turn of
    /// `agent`.
    pub fn is_resolved(&self, agent: &Agent) -> bool {
        self.notes_of(agent)
            .is_some_and(|n| n.resolved.is_some() && n.resolved == agent.last_ordinal)
    }

    /// The selected agent.
    pub fn selected_agent_row(&self) -> Option<&Agent> {
        self.sel_agent_idx().map(|i| &self.agents[i])
    }

    /// The rows of the Triage panel for the selected agent: the follow-ups,
    /// the agent TODO items, then the TODO items of the user.
    pub fn triage_items(&self) -> Vec<TriageItem<'_>> {
        let Some(notes) = self.selected_agent_row().and_then(|a| self.notes_of(a)) else {
            return Vec::new();
        };
        let mut items: Vec<TriageItem> = notes.follow_ups.iter().map(TriageItem::FollowUp).collect();
        for list in [TodoList::Agent, TodoList::Mine] {
            items.extend(notes.todo(list).iter().map(|t| TriageItem::Todo(list, t)));
        }
        items
    }

    fn clamp_triage_sel(&mut self) {
        let n = self.triage_items().len();
        self.triage_sel = self.triage_sel.min(n.saturating_sub(1));
    }

    pub fn triage_down(&mut self) {
        if self.triage_sel + 1 < self.triage_items().len() {
            self.triage_sel += 1;
        }
    }

    pub fn triage_up(&mut self) {
        self.triage_sel = self.triage_sel.saturating_sub(1);
    }

    /// The prompt of the selected follow-up.
    pub fn selected_follow_up(&self) -> Option<String> {
        match self.triage_items().get(self.triage_sel)? {
            TriageItem::FollowUp(f) => Some(f.prompt.clone()),
            TriageItem::Todo(..) => None,
        }
    }

    /// Toggle the check box of the selected TODO item.
    pub fn toggle_selected_todo(&mut self, visible: usize) {
        let Some(chain) = self.selected_agent.clone() else {
            return;
        };
        let Some(TriageItem::Todo(list, todo)) = self.triage_items().get(self.triage_sel).copied() else {
            return;
        };
        let number = todo.number;
        if let Some(files) = self.triage_files.as_mut()
            && let Err(e) = files.toggle_todo(&chain, list, number)
        {
            self.error = Some(format!("cannot change the triage file: {e}"));
        }
        self.reapply_agent_view(visible);
    }

    /// Set `key` (`resolved` or `hidden`) of each agent in `chains` to the
    /// current turn of the agent, or clear it when `clear` is true.
    fn set_turn_fields(&mut self, chains: &[(String, Option<usize>)], key: &str, clear: bool, visible: usize) {
        if let Some(files) = self.triage_files.as_mut() {
            for (chain, turn) in chains {
                let value = if clear { None } else { *turn };
                if let Err(e) = files.set_turn_field(chain, key, value) {
                    self.error = Some(format!("cannot change the triage file: {e}"));
                }
            }
        }
        self.reapply_agent_view(visible);
    }

    /// Mark the current turn of the selected agent as resolved, or clear the
    /// mark.
    pub fn toggle_resolved(&mut self, visible: usize) {
        let Some(agent) = self.selected_agent_row().filter(|a| !self.is_triage_run(a)) else {
            return;
        };
        let target = (agent.chain_id.clone(), agent.last_ordinal);
        let resolved = self.is_resolved(agent);
        self.set_turn_fields(&[target], "resolved", resolved, visible);
    }

    /// Hide the selected agent until its next turn. The selection moves to
    /// the next agent.
    pub fn hide_selected(&mut self, visible: usize) {
        let Some(i) = self
            .sel_agent_idx()
            .filter(|&i| !self.is_triage_run(&self.agents[i]))
        else {
            return;
        };
        let agent = &self.agents[i];
        let target = (agent.chain_id.clone(), agent.last_ordinal);
        let next = self
            .agents
            .get(i + 1)
            .or_else(|| i.checked_sub(1).and_then(|p| self.agents.get(p)))
            .map(|a| a.chain_id.clone());
        self.selected_agent = next;
        self.set_turn_fields(&[target], "hidden", false, visible);
        self.refilter_history();
    }

    /// Hide all the resolved agents until their next turn.
    pub fn hide_resolved(&mut self, visible: usize) {
        let targets: Vec<(String, Option<usize>)> = self
            .agents
            .iter()
            .filter(|a| self.is_resolved(a))
            .map(|a| (a.chain_id.clone(), a.last_ordinal))
            .collect();
        self.set_turn_fields(&targets, "hidden", false, visible);
    }

    /// Select the agent of rank `rank`.
    pub fn select_rank(&mut self, rank: usize, visible: usize, visible_agents: usize) {
        let Some(chain) = self.triage_files.as_ref().and_then(|f| f.top.get(rank - 1)).cloned() else {
            return;
        };
        if let Some(i) = self.agents.iter().position(|a| a.chain_id == chain) {
            self.select_agent_row(i, visible);
            self.ensure_agent_visible(visible_agents);
        }
    }

    /// Change the order of the Queue panel.
    pub fn cycle_queue_order(&mut self, visible: usize, visible_agents: usize) {
        self.queue_order = self.queue_order.next();
        self.reapply_agent_view(visible);
        self.ensure_agent_visible(visible_agents);
    }

    /// The newest entry of the session of the selected agent, for the
    /// focus or the resume of the session.
    pub fn selected_agent_entry(&self) -> Option<&Entry> {
        let agent = self.selected_agent_row()?;
        self.view
            .entries
            .iter()
            .filter(|e| e.session_id.as_deref() == Some(agent.session_id.as_str()))
            .max_by_key(|e| e.timestamp)
    }

    /// The time that orders the agents of the panel and gives the date rows:
    /// the creation time in the default order of Queue mode, else the time
    /// of the newest prompt.
    pub fn agent_time(&self, agent: &Agent) -> DateTime<Local> {
        if self.mode == Mode::Queue && self.queue_order != QueueOrder::LastMessage {
            agent.first_ts.unwrap_or(agent.last_ts)
        } else {
            agent.last_ts
        }
    }

    /// The session chain of the History row closest to the cursor that has
    /// one.
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
                    .find_map(|&ei| self.view.entries[ei].chain_id.clone());
                if found.is_some() {
                    return found;
                }
            }
        }
        None
    }

    /// Identity of the selected History row. See [`EntryId`].
    pub fn current_entry_id(&self) -> Option<EntryId> {
        self.current_entry().map(|e| e.id.clone())
    }

    /// Scroll the Agents panel so the selected agent is on screen.
    /// `visible` is the height of the panel in lines. The main loop calls
    /// this function before each frame, because a new agent can move the
    /// selected agent down the list.
    pub fn ensure_agent_visible(&mut self, visible: usize) {
        let Some(i) = self.sel_agent_idx() else {
            return;
        };
        if visible == 0 {
            return;
        }
        if i < self.agent_scroll {
            self.agent_scroll = i;
        }
        while self.agent_scroll < i && i >= self.agent_scroll + self.agents_fit(self.agent_scroll, visible) {
            self.agent_scroll += 1;
        }
    }

    /// True if agent `i` is the first agent of its day, and the user turned
    /// the date row on. The Agents panel then draws a date row above the
    /// agent, as the History panel does.
    pub fn agent_starts_day(&self, i: usize) -> bool {
        if !self.settings.date_separator || i >= self.agents.len() {
            return false;
        }
        let day = self.agent_time(&self.agents[i]).date_naive();
        match i.checked_sub(1) {
            None => true,
            Some(prev) => self.agent_time(&self.agents[prev]).date_naive() != day,
        }
    }

    /// The height of agent `i` in lines, with its date row and the empty
    /// line after it.
    pub fn agent_height(&self, i: usize) -> usize {
        crate::ui::AGENT_ROW_HEIGHT + self.agent_starts_day(i) as usize
    }

    /// The number of agents, from agent `top`, that fit in a panel of
    /// `visible` lines. The last agent needs no empty line after it. When
    /// the user turns the date row on, the panel keeps the top line for the
    /// sticky date row (see [`Self::agent_sticky_date`]), as History does.
    /// The value is at least 1 when an agent follows `top`.
    pub fn agents_fit(&self, top: usize, visible: usize) -> usize {
        let lines = visible.saturating_sub(self.settings.date_separator as usize);
        let mut used = 0usize;
        let mut count = 0usize;
        for i in top..self.agents.len() {
            let h = self.agent_height(i);
            if used + h - 1 > lines {
                break;
            }
            used += h;
            count += 1;
        }
        count.max(1).min(self.agents.len().saturating_sub(top))
    }

    /// The day that the sticky date row shows at the top of the Agents
    /// panel. It is the day of the top agent, when the top agent has no date
    /// row of its own. The value is `None` when the user turned the date row off.
    pub fn agent_sticky_date(&self) -> Option<NaiveDate> {
        let top = self.agent_scroll;
        if !self.settings.date_separator || top >= self.agents.len() || self.agent_starts_day(top) {
            return None;
        }
        Some(self.agent_time(&self.agents[top]).date_naive())
    }

    /// The agent on line `line` of the Agents panel, counted from the top
    /// of the panel. A click on the sticky date row selects no agent.
    pub fn agent_at_line(&self, line: usize) -> Option<usize> {
        let line = line.checked_sub(self.agent_sticky_date().is_some() as usize)?;
        let mut acc = 0usize;
        for i in self.agent_scroll..self.agents.len() {
            let h = self.agent_height(i);
            if line < acc + h {
                return Some(i);
            }
            acc += h;
        }
        None
    }

    // ── Agents panel navigation ───────────────────────────────────────────

    /// Index of the selected agent in `agents`.
    pub fn sel_agent_idx(&self) -> Option<usize> {
        let id = self.selected_agent.as_deref()?;
        self.agents.iter().position(|a| a.chain_id == id)
    }

    /// Select the agent at row `target`, then rebuild History for it.
    pub fn select_agent_row(&mut self, target: usize, visible: usize) {
        let Some(agent) = self.agents.get(target) else {
            return;
        };
        let id = agent.chain_id.clone();
        if self.selected_agent.as_deref() == Some(id.as_str()) {
            return;
        }
        self.selected_agent = Some(id);
        self.ensure_agent_visible(visible);
        self.refilter_history();
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
        let page = self.agents_fit(self.agent_scroll, visible);
        let target = (i + page).min(self.agents.len().saturating_sub(1));
        self.select_agent_row(target, visible);
    }

    pub fn agent_page_up(&mut self, visible: usize) {
        let Some(i) = self.sel_agent_idx() else {
            return;
        };
        let page = self.agents_fit(self.agent_scroll, visible);
        self.select_agent_row(i.saturating_sub(page), visible);
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

    // ── Agents-panel search ──────────────────────────────────────────────

    pub fn agent_search_open(&mut self) {
        self.agent_search.clear();
        self.agent_search.phase = SearchPhase::Typing;
        self.agent_search.restore = self.selected_agent.clone();
        self.search_history.get_mut(SearchMode::Agent).reset_walk();
    }

    pub fn agent_search_push_char(&mut self, c: char, visible: usize) {
        self.agent_search.query.push(c);
        self.search_history.get_mut(SearchMode::Agent).reset_walk();
        self.agent_search_query_changed(visible);
    }

    pub fn agent_search_pop_char(&mut self, visible: usize) {
        self.agent_search.query.pop();
        self.search_history.get_mut(SearchMode::Agent).reset_walk();
        self.agent_search_query_changed(visible);
    }

    /// Put the next older remembered query on the Agents prompt (Up).
    pub fn agent_search_history_older(&mut self, visible: usize) {
        let current = self.agent_search.query.clone();
        let Some(query) = self.search_history.get_mut(SearchMode::Agent).older(&current) else {
            return;
        };
        self.agent_search.query = query;
        self.agent_search_query_changed(visible);
    }

    /// Put the next newer remembered query on the Agents prompt (Down).
    pub fn agent_search_history_newer(&mut self, visible: usize) {
        let Some(query) = self.search_history.get_mut(SearchMode::Agent).newer() else {
            return;
        };
        self.agent_search.query = query;
        self.agent_search_query_changed(visible);
    }

    pub fn agent_search_commit(&mut self) {
        let query = self.agent_search.query.clone();
        self.search_history.get_mut(SearchMode::Agent).push(&query);
        if self.agent_search.matches.is_empty() {
            self.agent_search.clear();
            return;
        }
        self.agent_search.phase = SearchPhase::Active;
        self.agent_search.highlights = true;
        self.agent_search.restore = None;
    }

    /// Vim's `:noh`: hide the highlights, but keep the matches for n/N.
    pub fn agent_search_hide(&mut self) {
        if self.agent_search.phase == SearchPhase::Active {
            self.agent_search.highlights = false;
        }
    }

    /// Esc on the prompt: drop the search and select the agent that was
    /// selected when the prompt opened.
    pub fn agent_search_cancel(&mut self, visible: usize) {
        let query = self.agent_search.query.clone();
        self.search_history.get_mut(SearchMode::Agent).push(&query);
        let target = self.agent_search.restore.take();
        let was_typing = self.agent_search.phase == SearchPhase::Typing;
        self.agent_search.clear();
        if was_typing
            && let Some(i) = target.and_then(|id| self.agents.iter().position(|a| a.chain_id == id))
        {
            self.select_agent_row(i, visible);
        }
    }

    pub fn agent_search_next(&mut self, visible: usize) {
        self.agent_search_step(visible, RevealDir::Down, false);
    }

    pub fn agent_search_prev(&mut self, visible: usize) {
        self.agent_search_step(visible, RevealDir::Up, false);
    }

    /// Calculate the matches for the new query. The selection stays on the
    /// selected agent if it matches, else it goes to the first match below
    /// it, as vim's incremental search does.
    fn agent_search_query_changed(&mut self, visible: usize) {
        self.refresh_agent_matches();
        let selected_matches = self
            .selected_agent
            .as_deref()
            .is_some_and(|id| self.agent_search.is_match(id));
        if !selected_matches {
            self.agent_search_step(visible, RevealDir::Down, true);
        }
    }

    /// Match the query against the text of each agent row. `current` stays on
    /// the selected agent when it matches.
    fn refresh_agent_matches(&mut self) {
        let regex = if self.agent_search.query.is_empty() {
            None
        } else {
            recap_api::content_regex(&self.agent_search.query).ok()
        };
        self.agent_search.matches = match &regex {
            Some(re) => self
                .agents
                .iter()
                .filter(|a| self.agent_search_texts(a).iter().any(|t| re.is_match(t)))
                .map(|a| a.chain_id.clone())
                .collect(),
            None => Vec::new(),
        };
        self.agent_search.regex = regex;
        self.agent_search.current = self
            .selected_agent
            .as_deref()
            .and_then(|id| self.agent_search.matches.iter().position(|m| m == id))
            .unwrap_or(0);
    }

    /// The text of an agent row that the search reads: the project chain and
    /// the four content lines, as the Agents panel shows them.
    fn agent_search_texts(&self, agent: &Agent) -> [String; 5] {
        let [title, first, last, tail] = crate::ui::agent_content(agent);
        [
            crate::ui::agent_chain(agent, &self.view.project_names),
            title.0.to_string(),
            first.0.to_string(),
            last.0.to_string(),
            tail.0.to_string(),
        ]
    }

    /// Select the match below (`Down`) or above (`Up`) the selected agent.
    /// The walk wraps around, as it does in vim. With `include_selected`, the
    /// selected agent itself counts as the match below it.
    fn agent_search_step(&mut self, visible: usize, dir: RevealDir, include_selected: bool) {
        if self.agent_search.matches.is_empty() {
            return;
        }
        // `matches` holds the agents in the order of the panel.
        let rows: Vec<usize> = self
            .agent_search
            .matches
            .iter()
            .filter_map(|id| self.agents.iter().position(|a| &a.chain_id == id))
            .collect();
        let here = self.sel_agent_idx();
        let pick = match (dir, here) {
            (_, None) => rows.first(),
            (RevealDir::Down, Some(h)) => rows
                .iter()
                .find(|&&r| r > h || (include_selected && r == h))
                .or(rows.first()),
            (RevealDir::Up, Some(h)) => rows.iter().rev().find(|&&r| r < h).or(rows.last()),
        };
        let Some(&row) = pick else {
            return;
        };
        self.agent_search.highlights = true;
        self.agent_search.current = rows.iter().position(|&r| r == row).unwrap_or(0);
        self.select_agent_row(row, visible);
    }

    /// Move to the next entry-kind filter. This keeps the selection and the
    /// active search, so the user can narrow a result list of many file
    /// changes down to the one prompt inside it.
    pub fn cycle_kind_filter(&mut self, visible: usize) {
        self.kind_filter = self.kind_filter.next();
        let selection = self.current_entry_id();
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

        if self.search.kind == SearchKind::Content {
            // An invalid pattern gives no matches, so the UI reads as "no
            // matches" instead of an error. The Diff panel reuses the regex
            // to highlight the hits in the line.
            match recap_api::content_regex(&self.search.query) {
                Ok(regex) => self.search.content_regex = Some(regex),
                Err(_) => return,
            }
        }

        let response = self.api.search(self.search.kind, &self.search.query, &self.filter());
        let matches = match response {
            Ok(response) => response.matches,
            Err(e) => return self.fail(e),
        };
        // The server searched the same filter, so each match is in the view.
        // A match that a newer store version added is not, until the next
        // merge.
        self.search.matches = matches
            .into_iter()
            .filter_map(|m| {
                let entry_idx = self.view.index_of(&m.id)?;
                Some(SearchMatch {
                    id: m.id,
                    entry_idx,
                    indices: m.indices,
                    score: m.score,
                })
            })
            .collect();

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

    // ── Semantic search ──────────────────────────────────────────────────────

    /// Open the search prompt in semantic mode. This also asks the server to
    /// start the model and the indexer on the first use, so the user pays
    /// for the model only when the user asks for this search.
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
        if let Err(e) = self.api.start_embedding() {
            self.fail(e);
        }
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
            .filter(|m| self.view.entries[m.entry_idx].is_prompt())
            .count();
        (prompts, self.search.matches.len() - prompts)
    }

    // ── Projects-panel search ────────────────────────────────────────────────

    /// Display label used by the fuzzy matcher for project row `i`. Mirrors
    /// the rendered chain so a search for "foo → bar" can match the chain
    /// glyphs the user sees on screen.
    fn project_search_label(&self, i: usize) -> String {
        let proj = &self.view.projects[i];
        Some(&proj.chain)
            .filter(|chain| !chain.is_empty())
            .map(|chain| {
                chain
                    .iter()
                    .map(|p| recap_api::project_name(p, &self.view.project_names))
                    .collect::<Vec<_>>()
                    .join(" → ")
            })
            .unwrap_or_else(|| Self::project_label(&proj.root))
    }

    fn project_search_items(&self) -> Vec<String> {
        (0..self.view.projects.len())
            .map(|i| self.project_search_label(i))
            .collect()
    }

    pub fn project_search_open(&mut self) {
        let current = self
            .selected_project_idx
            .and_then(|i| self.view.projects.get(i).map(|p| p.root.clone()));
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
        let target_path = self.view.projects.get(m.item_idx).map(|p| p.root.clone());
        self.select_project_by_path(target_path, visible);
    }

    /// Move the Projects selection to the given project (or "All" for `None`),
    /// adjusting scroll to keep the row in view. Triggers a `refilter` since
    /// changing the project filter ripples through Files/History/Diff.
    fn select_project_by_path(&mut self, target: Option<PathBuf>, visible: usize) {
        let new_idx = target
            .as_ref()
            .and_then(|p| self.view.project_position(p));
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
        let file = &self.files[i].path;
        let chain = &self.files[i].chain;
        let project_selected = self.selected_project_idx.is_some();
        // Reuse the same label the panel renders so the user can search the
        // text they see (chain + rel path, or just rel path when a project
        // filter hides the chain).
        crate::ui::file_display_for_panel(
            file,
            chain,
            &self.view.project_names,
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
        let target = self.files.get(m.item_idx).map(|f| f.path.clone());
        self.select_file_by_path(target, visible);
    }

    fn select_file_by_path(&mut self, target: Option<PathBuf>, visible: usize) {
        if self.selected_file_path == target {
            return;
        }
        self.selected_file_path = target.clone();
        let new_idx = target
            .as_ref()
            .and_then(|p| self.files.iter().position(|x| &x.path == p));
        let display_row = new_idx.map(|i| i + 1).unwrap_or(0);
        if display_row < self.file_scroll {
            self.file_scroll = display_row;
        } else if visible > 0 && display_row >= self.file_scroll + visible {
            self.file_scroll = display_row + 1 - visible;
        }
        self.refilter_history();
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
        let regex = match recap_api::content_regex(&self.diff_search.query) {
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
    use recap_api::EntryKind;
    use recap_server::history::HistoryEntry;

    /// An App whose server holds `entries` and runs no loader.
    fn test_app(entries: Vec<HistoryEntry>) -> App {
        App::new(Arc::new(recap_server::Server::from_entries(entries)))
    }

    /// An App whose server loads the real sources of this machine. The
    /// function returns when the first load is complete.
    pub(super) fn real_app() -> App {
        let server = recap_server::Server::start(Duration::from_secs(3600), None);
        let started = Instant::now();
        loop {
            let status = server.status().unwrap();
            if status.version > 0 && !status.loading {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("the server loaded the sources in {:?}", started.elapsed());
        App::new(Arc::new(server))
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

    /// A committed query comes back on Up, and each corpus keeps its own list.
    #[test]
    fn each_history_search_mode_walks_its_own_query_list() {
        let store = (vec![prompt_entry("fix it", "s1", 5)]);
        let mut app = test_app(store);

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
        let store = (vec![prompt_entry("fix it", "s1", 5)]);
        let mut app = test_app(store);

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
        let store = (vec![
            prompt_entry("alpha one", "s1", 5),
            prompt_entry("beta", "s2", 4),
            prompt_entry("alpha two", "s3", 3),
        ]);
        let mut app = test_app(store);

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
        let store = (vec![
            prompt_entry("alpha", "s1", 5),
            prompt_entry("alpha alpha alpha", "s2", 4),
            prompt_entry("alpha", "s3", 3),
        ]);
        let mut app = test_app(store);
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
        let store = (vec![prompt_entry("fix it", "s1", 5)]);
        let mut app = test_app(store);
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
        let store = (vec![prompt_entry("fix it", "s1", 5)]);
        let mut app = test_app(store);
        app.diff_search_open();
        for c in "fn main".chars() {
            app.diff_search_push_char(c);
        }
        app.diff_search_commit();

        let saved = app.persisted_state();
        let store = (vec![prompt_entry("fix it", "s1", 5)]);
        let mut next = test_app(store);
        next.apply_persisted(&saved);

        next.diff_search_open();
        next.diff_search_history_older();
        assert_eq!(next.diff_search.query, "fn main");
    }

    fn nvim_entry(path: &str, minutes_ago: i64) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local::now() - chrono::Duration::minutes(minutes_ago),
            file_path: PathBuf::from(path),
            snapshot_path: PathBuf::from(format!("/snapshots{path}/{minutes_ago}")),
            base_path: None,
            project_roots: vec![PathBuf::from("/dev/proj")],
            source: EditSource::Neovim,
            kind: EntryKind::FileChange,
            session_id: None,
            session_cwd: None,
        }
    }

    /// Two agents whose prompts interleave, so a File-mode row is never row 0
    /// of its own agent.
    fn interleaved_app() -> App {
        let store = (vec![
            prompt_entry("b2", "sessionB", 1),
            prompt_entry("a2", "sessionA", 2),
            prompt_entry("b1", "sessionB", 3),
            prompt_entry("a1", "sessionA", 4),
        ]);
        test_app(store)
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
                app.view.entries[g.entry_indices[0]].prompt_text() == Some(text)
            })
            .unwrap_or_else(|| panic!("no row for {text:?}"));
        app.sel_group = gi;
        app.sel_child = 0;
        app.rebuild_display_after_external_mutation(20);
    }

    /// A store with one prompt and three file changes, all in one minute, so
    /// the kind filter is the only thing that changes the row count.
    fn mixed_app() -> App {
        let store = (vec![
            prompt_entry("fix the parser", "sessionA", 1),
            nvim_entry("/dev/proj/parser.rs", 2),
            nvim_entry("/dev/proj/lexer.rs", 3),
            nvim_entry("/dev/proj/main.rs", 4),
        ]);
        test_app(store)
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

    /// Agent mode on a store with three agents, each with one prompt. The
    /// Agents panel lists them newest first: C, B, A.
    fn three_agents_app() -> App {
        let mut app = test_app(vec![
            prompt_entry("deploy the server", "sessionC", 1),
            prompt_entry("fix the parser", "sessionB", 2),
            prompt_entry("Parse the logs", "sessionA", 3),
        ]);
        app.toggle_mode(20, 20);
        app.active_panel = Panel::Agents;
        app.agent_top(20);
        app
    }

    /// The selected agent can move down the list, for example when a newer
    /// agent appears. The Agents panel then scrolls to it.
    #[test]
    fn the_agents_panel_scrolls_to_the_selected_agent() {
        let mut app = three_agents_app();
        // One agent fits in 4 lines.
        assert_eq!(app.agents_fit(0, 4), 1);
        app.selected_agent = Some("sessionA".to_string());
        app.ensure_agent_visible(4);
        assert_eq!(app.agent_scroll, 2);
        app.selected_agent = Some("sessionC".to_string());
        app.ensure_agent_visible(4);
        assert_eq!(app.agent_scroll, 0);
    }

    #[test]
    fn a_date_row_makes_the_first_agent_of_a_day_one_line_taller() {
        let mut app = test_app(vec![
            prompt_entry("deploy the server", "sessionC", 1),
            prompt_entry("fix the parser", "sessionB", 2),
            prompt_entry("Parse the logs", "sessionA", 3 * 24 * 60),
        ]);
        app.settings.date_separator = true;
        app.toggle_mode(20, 20);

        let heights: Vec<usize> = (0..3).map(|i| app.agent_height(i)).collect();
        assert_eq!(heights, [6, 5, 6]);
        // The sticky date row keeps one line, so 12 lines fit two agents.
        assert_eq!(app.agents_fit(0, 12), 2);
        assert_eq!(app.agent_sticky_date(), None, "the top agent has its own date row");
        assert_eq!(app.agent_at_line(0), Some(0));
        assert_eq!(app.agent_at_line(6), Some(1));

        app.agent_scroll = 1;
        assert!(app.agent_sticky_date().is_some());
        assert_eq!(app.agent_at_line(0), None, "a click on the sticky date row selects no agent");
        assert_eq!(app.agent_at_line(1), Some(1));
        assert_eq!(app.agent_at_line(6), Some(2));
    }

    fn type_agent_query(app: &mut App, query: &str) {
        app.agent_search_open();
        for c in query.chars() {
            app.agent_search_push_char(c, 20);
        }
    }

    #[test]
    fn the_agent_search_selects_the_agents_that_match() {
        let mut app = three_agents_app();
        assert_eq!(app.selected_agent.as_deref(), Some("sessionC"));

        // The regex ignores the case, so it matches "parser" and "Parse".
        type_agent_query(&mut app, "pars");
        assert_eq!(app.agent_search.matches, ["sessionB", "sessionA"]);
        assert_eq!(app.selected_agent.as_deref(), Some("sessionB"));
        assert_eq!(selected(&app), "fix the parser", "History follows the agent");

        app.agent_search_commit();
        assert_eq!(app.agent_search.phase, SearchPhase::Active);
        app.agent_search_next(20);
        assert_eq!(app.selected_agent.as_deref(), Some("sessionA"));
        app.agent_search_next(20);
        assert_eq!(app.selected_agent.as_deref(), Some("sessionB"), "n wraps around");
        app.agent_search_prev(20);
        assert_eq!(app.selected_agent.as_deref(), Some("sessionA"), "N wraps around");
        assert_eq!(app.agent_search.current, 1);
    }

    #[test]
    fn esc_on_the_agent_prompt_selects_the_first_agent_again() {
        let mut app = three_agents_app();
        type_agent_query(&mut app, "logs");
        assert_eq!(app.selected_agent.as_deref(), Some("sessionA"));

        app.agent_search_cancel(20);
        assert_eq!(app.selected_agent.as_deref(), Some("sessionC"));
        assert_eq!(app.agent_search.phase, SearchPhase::Inactive);
    }

    #[test]
    fn an_agent_query_without_a_match_ends_the_search() {
        let mut app = three_agents_app();
        // An invalid regex matches no agent, and the selection stays.
        type_agent_query(&mut app, "(");
        assert!(app.agent_search.matches.is_empty());
        assert_eq!(app.selected_agent.as_deref(), Some("sessionC"));

        app.agent_search_commit();
        assert_eq!(app.agent_search.phase, SearchPhase::Inactive);
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

    /// Agent mode lists the oldest prompt first. A new agent selection puts
    /// the cursor on the newest prompt, and the first frame scrolls it into
    /// view at the bottom of the panel.
    #[test]
    fn agent_mode_lists_the_newest_prompt_at_the_bottom() {
        let entries = (0..30).map(|i| prompt_entry(&format!("ask {i}"), "s1", 30 - i)).collect();
        let mut app = test_app(entries);
        app.mode = Mode::Agent;
        app.selected_agent = Some("s1".to_string());
        app.refilter();
        let texts: Vec<&str> = app
            .groups
            .iter()
            .map(|g| app.view.entries[g.entry_indices[0]].prompt_text().unwrap())
            .collect();
        assert_eq!(texts.first().copied(), Some("ask 0"), "the oldest prompt is first");
        assert_eq!(texts.last().copied(), Some("ask 29"), "the newest prompt is last");
        assert_eq!(selected(&app), "ask 29");

        let visible = 10;
        app.recompute_row_heights(80, visible);
        app.reveal_pending_selection(visible);
        let drawn = app.history_rows_fit(visible);
        assert_eq!(
            app.history_scroll + drawn,
            app.display_rows.len(),
            "the panel shows the last row"
        );
        // Each prompt row has an empty line above the prompt.
        assert_eq!(app.row_height(0), 2);
        assert_eq!(drawn, visible / 2, "the panel is full");
        assert_eq!(app.sel_display_row(), app.display_rows.len() - 1);
        assert_eq!(app.history_rows_below(visible), 0);

        // At the top, all the rows after the first five are below the window.
        app.history_top(visible);
        assert_eq!(app.history_rows_below(visible), 25);
        app.history_bottom(visible);
        assert_eq!(app.history_rows_below(visible), 0);
    }

    #[test]
    fn file_mode_follows_a_cursor_the_user_moved_in_agent_mode() {
        let mut app = interleaved_app();
        select_prompt(&mut app, "b1");

        app.toggle_mode(20, 20);
        // The user reads the agent's newer prompt, then switches back. Agent
        // mode lists the newer prompt below the older one.
        app.history_down(20);
        assert_eq!(selected(&app), "b2");
        app.toggle_mode(20, 20);

        assert_eq!(selected(&app), "b2", "File mode follows the newer choice");
    }

    #[test]
    fn a_row_without_an_agent_survives_the_round_trip() {
        let store = (vec![
            prompt_entry("b2", "sessionB", 1),
            nvim_entry("/dev/proj/src/main.rs", 2),
            prompt_entry("a1", "sessionA", 3),
        ]);
        let mut app = test_app(store);
        let gi = app
            .groups
            .iter()
            .position(|g| !app.view.entries[g.entry_indices[0]].is_prompt())
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
        let store = (vec![
            prompt_entry("b2", "sessionB", 1),
            nvim_entry("/dev/proj/src/main.rs", 2),
            prompt_entry("a1", "sessionA", 3),
        ]);
        let mut app = test_app(store);
        let gi = app
            .groups
            .iter()
            .position(|g| !app.view.entries[g.entry_indices[0]].is_prompt())
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
        with_response.kind = EntryKind::Prompt {
            text: "second prompt".to_string(),
            response: Some(response.join("\n")),
        };
        let store = (vec![
            prompt_entry("first prompt", "sessionA", 1),
            with_response,
        ]);
        let mut app = test_app(store);
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
        entry.kind = EntryKind::Prompt {
            text: "fix the parser".to_string(),
            response: Some("I fixed it.".to_string()),
        };
        let store = (vec![entry]);
        let mut app = test_app(store);

        // The History panel shows no response, so `Y` does nothing there.
        app.active_panel = Panel::History;
        assert_eq!(app.copy_response_target(), None);

        app.active_panel = Panel::Diff;
        assert_eq!(app.copy_response_target(), Some("I fixed it.".to_string()));
    }

    #[test]
    fn shift_y_copies_nothing_when_the_prompt_has_no_response() {
        let store =
            (vec![prompt_entry("fix the parser", "sessionA", 1)]);
        let mut app = test_app(store);
        app.active_panel = Panel::Diff;
        assert_eq!(app.copy_response_target(), None);
    }

    #[test]
    fn the_status_bar_drops_open_file_on_a_prompt_row() {
        let store = (vec![
            prompt_entry("fix the parser", "sessionA", 1),
            nvim_entry("/dev/proj/src/main.rs", 2),
        ]);
        let mut app = test_app(store);

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
        let mut app = test_app((entries));
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
        let mut app = test_app((entries));

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
        let mut app = test_app((entries));
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
        let mut app = tests::real_app();
        eprintln!("loaded {} entries", app.view.entries.len());

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

    /// Non-hermetic: loads every real source on this machine, then runs the
    /// content (`f`) search. Ignored by default:
    /// `cargo test smoke_full_load_and_content_search -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn smoke_full_load_and_content_search() {
        let needle = std::env::var("RECAP_NEEDLE").unwrap_or_else(|_| "space_for".to_string());
        let visible = 40;
        let mut app = tests::real_app();

        let total = app.view.entries.len();
        let cursor_blob = app
            .view
            .entries
            .iter()
            .filter(|e| e.source == EditSource::Cursor && e.base_path.is_some())
            .count();
        let cursor_local = app
            .view
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
            app.view.entries.len(),
            elapsed
        );
        for m in app.search.matches.iter().take(8) {
            let e = &app.view.entries[m.entry_idx];
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

