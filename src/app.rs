use crate::cursor_workspace;
use crate::diff::{build_diff, compute_summary, DiffLine, DiffSummary};
use crate::history::{find_project_chain, HistoryEntry, HistoryEntryKind, HistoryStore};
use crate::{claude, nvim};
use chrono::{DateTime, Local};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use regex::{Regex, RegexBuilder};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Instant, SystemTime};

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
    SourceFinished {
        kind: SourceKind,
        mtimes: HashMap<PathBuf, SystemTime>,
        seen: HashSet<PathBuf>,
    },
}

/// Stable cache key for an entry's searchable text. FileChange entries key on
/// their snapshot file (one per edit); Prompt entries key on the synthetic
/// `file_path` they already use as their unique id. Both survive the
/// re-sort of `store.entries` that follows a load-batch merge.
fn content_cache_key(entry: &HistoryEntry) -> PathBuf {
    match &entry.kind {
        HistoryEntryKind::Prompt { .. } => entry.file_path.clone(),
        HistoryEntryKind::FileChange => entry.snapshot_path.clone(),
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


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Projects,
    Files,
    History,
    Diff,
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
    /// (sel_group, sel_child) snapshot taken when entering Typing — restored on cancel.
    pub restore: Option<(usize, usize)>,
    /// Compiled regex for the current content-search query. `None` while the
    /// query is empty or fails to parse. Used both for picking matches and
    /// for highlighting hits in the Diff panel.
    pub content_regex: Option<Regex>,
    matcher: Matcher,
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
            restore: None,
            content_regex: None,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }
}

impl SearchState {
    /// Lookup table from entry_idx → position in `matches`.
    pub fn match_pos_for(&self, entry_idx: usize) -> Option<usize> {
        self.matches.iter().position(|m| m.entry_idx == entry_idx)
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
        DiffLine::Added(s) | DiffLine::Removed(s) | DiffLine::Prompt(s) => Some(s),
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

    /// Max visual lines a prompt entry may span in the History list. Adjusted
    /// with `>`/`<`, persisted across restarts.
    pub prompt_lines: usize,

    /// Per-`display_rows` visual height in terminal lines. Recomputed each
    /// frame from the panel width (prompts wrap to up to `prompt_lines`
    /// lines; everything else is 1). Drives scroll/click math so a tall
    /// prompt row doesn't confuse the single-line-per-row assumption.
    pub hist_row_heights: Vec<usize>,

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
    pub project_search: PanelSearchState,
    pub file_search: PanelSearchState,
    pub diff_search: DiffSearchState,

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

    /// Persisted project selection (by root path) awaiting resolution to an
    /// index. The projects list is empty at startup and fills in async, so the
    /// path is held here and resolved against `store.projects` as batches
    /// arrive. Only honored while `loading`; cleared once loading finishes.
    pending_project_restore: Option<PathBuf>,
    /// Persisted file selection awaiting its entries to load. Keeps
    /// `selected_file_path` from being dropped by `recompute_files` before the
    /// file's first entry has streamed in. Cleared once loading finishes.
    pending_file_restore: Option<PathBuf>,
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
            groups: Vec::new(),
            display_rows: Vec::new(),
            sel_group: 0,
            sel_child: 0,
            history_scroll: 0,
            diff_scroll: 0,
            absolute_time: false,
            diff_lines: Vec::new(),
            prompt_lines: DEFAULT_PROMPT_LINES,
            hist_row_heights: Vec::new(),
            diff_summaries: HashMap::new(),
            content_cache: HashMap::new(),
            search: SearchState::default(),
            project_search: PanelSearchState::default(),
            file_search: PanelSearchState::default(),
            diff_search: DiffSearchState::default(),
            auto_expand: false,
            sources,
            running: crate::session::RunningSessions::scan(),
            running_scanned_at: Some(Instant::now()),
            loading: false,
            rx: None,
            loading_sources: 0,
            pending_project_restore: None,
            pending_file_restore: None,
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
        crate::persist::PersistedState {
            selected_project,
            selected_file,
            absolute_time: self.absolute_time,
            auto_expand: self.auto_expand,
            prompt_lines: self.prompt_lines,
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
        self.loading_sources = 0;

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
                    HistoryStore::load_cursor(&dir, &mut mtimes, &mut seen, &mut on_batch)
                {
                    eprintln!("recap: cursor history failed: {e}");
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
                let mut on_batch = |batch: Vec<HistoryEntry>| -> bool {
                    tx.send(LoadEvent::Batch(batch)).is_ok()
                };
                if let Err(e) = claude::load(&dir, &mut mtimes, &mut seen, &mut on_batch) {
                    eprintln!("recap: claude history failed: {e}");
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
                if let Err(e) = nvim::load(&dir, &mut mtimes, &mut seen, &mut on_batch) {
                    eprintln!("recap: neovim history failed: {e}");
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
                Ok(LoadEvent::CursorSessions { sessions, mtime }) => {
                    self.sources.cursor_sessions_mtime = mtime;
                    self.store.cursor_session_names = sessions.names_map().clone();
                    // Merge the composer's user prompts like Claude's, deduped
                    // by their unique synthetic key (`cursor:<composer>:<bubble>`)
                    // so a rebuild on mtime change doesn't re-add old prompts.
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
                        .filter(|p| !have.contains(&p.file_path))
                        .cloned()
                        .collect();
                    drop(have);
                    accumulated.extend(fresh);
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
            .map(|(i, _)| i)
            .collect()
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
    }

    pub fn refilter(&mut self) {
        self.recompute_files();
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
        if self.groups.is_empty() {
            self.diff_lines = vec![DiffLine::Header("(no history)".to_string())];
            return;
        }
        let Some(group) = self.groups.get(self.sel_group) else {
            self.diff_lines = vec![DiffLine::Header("(no selection)".to_string())];
            return;
        };

        // Collapsed multi-entry group → concatenate every child's diff,
        // separated by per-child header rows.
        if !self.effective_auto_expand() && group.entry_indices.len() > 1 {
            let mut lines: Vec<DiffLine> = Vec::new();
            for (i, &entry_idx) in group.entry_indices.iter().enumerate() {
                let entry = &self.store.entries[entry_idx];
                let prev = self.store.entries[entry_idx + 1..]
                    .iter()
                    .find(|e| e.file_path == entry.file_path)
                    .map(|e| e.snapshot_path.as_path());
                if i > 0 {
                    lines.push(DiffLine::Header(String::new()));
                }
                lines.push(DiffLine::ChildSeparator { entry_idx });
                lines.extend(build_diff(prev, &entry.snapshot_path));
            }
            self.diff_lines = lines;
            return;
        }

        let Some(&entry_idx) = group.entry_indices.get(self.sel_child) else {
            self.diff_lines = vec![DiffLine::Header("(no selection)".to_string())];
            return;
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
            self.diff_lines = lines;
            return;
        }
        // Previous snapshot = next entry in the global list with the same file path (older).
        let prev = self.store.entries[entry_idx + 1..]
            .iter()
            .find(|e| e.file_path == entry.file_path)
            .map(|e| e.snapshot_path.as_path());
        self.diff_lines = build_diff(prev, &entry.snapshot_path);
        self.after_diff_lines_changed();
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
        if self.search.kind == SearchKind::Content
            && self.search.phase != SearchPhase::Inactive
            && self.search.highlights
        {
            if let Some(re) = self.search.content_regex.clone() {
                if let Some(line) = self.first_diff_line_matching(&re) {
                    self.diff_scroll = line;
                }
            }
        }
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
        let row = self.sel_display_row();
        if row < self.history_scroll {
            self.history_scroll = row;
            return;
        }
        if visible == 0 {
            return;
        }
        // Advance the top of the window until the selected row fits in
        // `visible` lines. Bounded by `row` so we never scroll past it.
        while self.history_scroll < row {
            let used: usize = (self.history_scroll..=row).map(|i| self.row_height(i)).sum();
            if used <= visible {
                break;
            }
            self.history_scroll += 1;
        }
    }

    /// Visual height (terminal lines) of display row `idx`. Defaults to 1
    /// when heights haven't been computed yet.
    pub fn row_height(&self, idx: usize) -> usize {
        self.hist_row_heights.get(idx).copied().unwrap_or(1).max(1)
    }

    /// Entry index behind display row `idx` (the group's first entry for a
    /// Group row, or the specific child for a Child row).
    fn row_entry_idx(&self, idx: usize) -> usize {
        match &self.display_rows[idx] {
            DisplayRow::Group(gi) => self.groups[*gi].entry_indices[0],
            DisplayRow::Child { group_idx, child_local } => {
                self.groups[*group_idx].entry_indices[*child_local]
            }
        }
    }

    /// Map a 0-based line offset below `history_scroll` to the display-row
    /// index that occupies it (accounting for multi-line prompt rows). Used
    /// to translate a mouse click into a selection.
    pub fn history_item_at_line(&self, line_offset: usize) -> Option<usize> {
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
    /// History list's inner width and height. Prompt rows wrap to up to
    /// `prompt_lines` lines; every other row is a single line.
    pub fn recompute_row_heights(&mut self, list_width: usize, list_height: usize) {
        let ts_width = self.estimate_ts_width(list_height);
        let mut heights = Vec::with_capacity(self.display_rows.len());
        for row in &self.display_rows {
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
            heights.push(h);
        }
        self.hist_row_heights = heights;
    }

    /// Number of wrapped lines a prompt row occupies at the given panel
    /// geometry — mirrors the wrapping the renderer performs so scroll math
    /// and rendering agree.
    fn prompt_row_height(&self, entry: &HistoryEntry, list_width: usize, ts_width: usize) -> usize {
        let (chain, text) = entry.display_path_segments(&self.store.project_names);
        let chain_chars = if chain.is_empty() {
            0
        } else {
            chain.iter().map(|s| s.chars().count()).sum::<usize>() + 3 * chain.len()
        };
        let text_start = crate::ui::history_prefix_cols(ts_width) + chain_chars;
        let avail = list_width
            .saturating_sub(text_start + crate::ui::RUNNING_RESERVE)
            .max(1);
        crate::history::wrap_text(&text, avail, self.prompt_lines)
            .len()
            .max(1)
    }

    /// Estimate the timestamp-column width for the currently-visible window.
    /// Used only to size prompt wrapping; the renderer recomputes its own
    /// (near-identical) width, and any small drift is absorbed by the
    /// pad/truncate step when rows are drawn.
    fn estimate_ts_width(&self, list_height: usize) -> usize {
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
            let prev = self.store.entries[entry_idx + 1..]
                .iter()
                .find(|e| e.file_path == entry.file_path)
                .map(|e| e.snapshot_path.clone());
            let summary = compute_summary(prev.as_deref(), &entry.snapshot_path);
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
            Panel::History => self.current_entry().map(|e| e.file_path.clone()),
            Panel::Diff => None,
        }
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

    pub fn toggle_panel(&mut self) {
        self.active_panel = match self.active_panel {
            Panel::Projects => Panel::Files,
            Panel::Files => Panel::History,
            Panel::History => Panel::Diff,
            Panel::Diff => Panel::Projects,
        };
    }

    /// Rotate focus the other direction (Shift+Tab).
    pub fn toggle_panel_backward(&mut self) {
        self.active_panel = match self.active_panel {
            Panel::Projects => Panel::Diff,
            Panel::Files => Panel::Projects,
            Panel::History => Panel::Files,
            Panel::Diff => Panel::History,
        };
    }

    /// Move focus one panel right. No wrap-around.
    pub fn focus_right(&mut self) {
        self.active_panel = match self.active_panel {
            Panel::Projects => Panel::Files,
            Panel::Files => Panel::History,
            Panel::History => Panel::Diff,
            Panel::Diff => Panel::Diff,
        };
    }

    /// Move focus one panel left. No wrap-around.
    pub fn focus_left(&mut self) {
        self.active_panel = match self.active_panel {
            Panel::Projects => Panel::Projects,
            Panel::Files => Panel::Projects,
            Panel::History => Panel::Files,
            Panel::Diff => Panel::History,
        };
    }

    pub fn toggle_time(&mut self) {
        self.absolute_time = !self.absolute_time;
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
        self.search.query.clear();
        self.search.matches.clear();
        self.search.current = 0;
        self.search.highlights = true;
        self.search.content_regex = None;
    }

    /// Like `search_open` but starts the prompt in content-search mode:
    /// the query is treated as a case-insensitive regex and applied to each
    /// entry's diff body / prompt text rather than its display path.
    pub fn search_open_content(&mut self) {
        self.search.phase = SearchPhase::Typing;
        self.search.kind = SearchKind::Content;
        self.search.restore = Some((self.sel_group, self.sel_child));
        self.search.query.clear();
        self.search.matches.clear();
        self.search.current = 0;
        self.search.highlights = true;
        self.search.content_regex = None;
    }

    pub fn search_push_char(&mut self, c: char, visible: usize) {
        self.search.query.push(c);
        self.recompute_matches(visible);
    }

    pub fn search_pop_char(&mut self, visible: usize) {
        self.search.query.pop();
        self.recompute_matches(visible);
    }

    pub fn search_commit(&mut self) {
        if self.search.matches.is_empty() {
            // Nothing to keep highlighted — fall back to clear.
            self.search_clear();
            return;
        }
        self.search.phase = SearchPhase::Active;
        self.search.highlights = true;
        self.search.restore = None;
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
    }

    pub fn search_next(&mut self, visible: usize) {
        if self.search.matches.is_empty() {
            return;
        }
        self.search.highlights = true;
        self.search.current = (self.search.current + 1) % self.search.matches.len();
        self.jump_to_current_match(visible);
    }

    pub fn search_prev(&mut self, visible: usize) {
        if self.search.matches.is_empty() {
            return;
        }
        self.search.highlights = true;
        let len = self.search.matches.len();
        self.search.current = (self.search.current + len - 1) % len;
        self.jump_to_current_match(visible);
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

        if self.search.query.is_empty() {
            return;
        }

        let results = match self.search.kind {
            SearchKind::Path => self.compute_path_matches(),
            SearchKind::Content => self.compute_content_matches(),
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
                if self.search.kind == SearchKind::Content {
                    if let Some(re) = self.search.content_regex.clone() {
                        if let Some(line) = self.first_diff_line_matching(&re) {
                            self.diff_scroll = line;
                        }
                    }
                }
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

        // First pass: lazily materialize searchable text for every entry that
        // the current filter exposes. Cached across keystrokes, so only the
        // first content search after each entry shows up pays the diff cost.
        let entry_indices: Vec<usize> = self
            .groups
            .iter()
            .flat_map(|g| g.entry_indices.iter().copied())
            .collect();
        for &entry_idx in &entry_indices {
            self.ensure_content_text(entry_idx);
        }

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

    /// Materialize the searchable text for one entry into `content_cache`.
    /// FileChange entries store the concatenated `+`/`-` diff lines; Prompt
    /// entries store the full prompt text. No-op if the entry is already
    /// cached.
    fn ensure_content_text(&mut self, entry_idx: usize) {
        let entry = &self.store.entries[entry_idx];
        let key = content_cache_key(entry);
        if self.content_cache.contains_key(&key) {
            return;
        }

        let text = match &entry.kind {
            HistoryEntryKind::Prompt { text } => text.clone(),
            HistoryEntryKind::FileChange => {
                let snapshot_path = entry.snapshot_path.clone();
                let file_path = entry.file_path.clone();
                let prev = self.store.entries[entry_idx + 1..]
                    .iter()
                    .find(|e| e.file_path == file_path)
                    .map(|e| e.snapshot_path.clone());
                let diff_lines = build_diff(prev.as_deref(), &snapshot_path);
                let mut out = String::new();
                for dl in &diff_lines {
                    if let DiffLine::Added(l) | DiffLine::Removed(l) = dl {
                        out.push_str(l);
                        out.push('\n');
                    }
                }
                out
            }
        };
        self.content_cache.insert(key, text);
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
        self.project_search.query.clear();
        self.project_search.matches.clear();
        self.project_search.current = 0;
        self.project_search.highlights = true;
    }

    pub fn project_search_push_char(&mut self, c: char, visible: usize) {
        self.project_search.query.push(c);
        self.recompute_project_matches(visible);
    }

    pub fn project_search_pop_char(&mut self, visible: usize) {
        self.project_search.query.pop();
        self.recompute_project_matches(visible);
    }

    pub fn project_search_commit(&mut self) {
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
        self.file_search.query.clear();
        self.file_search.matches.clear();
        self.file_search.current = 0;
        self.file_search.highlights = true;
    }

    pub fn file_search_push_char(&mut self, c: char, visible: usize) {
        self.file_search.query.push(c);
        self.recompute_file_matches(visible);
    }

    pub fn file_search_pop_char(&mut self, visible: usize) {
        self.file_search.query.pop();
        self.recompute_file_matches(visible);
    }

    pub fn file_search_commit(&mut self) {
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
        self.diff_search.query.clear();
        self.diff_search.matches.clear();
        self.diff_search.current = 0;
        self.diff_search.highlights = true;
        self.diff_search.regex = None;
    }

    pub fn diff_search_push_char(&mut self, c: char) {
        self.diff_search.query.push(c);
        self.recompute_diff_matches();
        self.jump_to_current_diff_match();
    }

    pub fn diff_search_pop_char(&mut self) {
        self.diff_search.query.pop();
        self.recompute_diff_matches();
        self.jump_to_current_diff_match();
    }

    pub fn diff_search_commit(&mut self) {
        if self.diff_search.matches.is_empty() {
            self.diff_search_clear();
            return;
        }
        self.diff_search.phase = SearchPhase::Active;
        self.diff_search.highlights = true;
        self.diff_search.restore_scroll = None;
    }

    pub fn diff_search_cancel(&mut self) {
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
    /// auto-expanding its parent group if needed.
    fn jump_to_current_match(&mut self, visible: usize) {
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
        self.adjust_scroll(visible);
        self.refresh_diff();
    }
}
