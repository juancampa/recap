//! The in-process server: the store, the caches, the semantic search, and
//! the manager thread that keeps the store current.
//!
//! [`Server`] implements [`recap_api::Api`]. The HTTP handlers (see
//! [`crate::http`]) and the tests call it directly.
//!
//! The manager thread starts a load of all sources, merges the batches of
//! the loader threads, and starts the next load `poll` after the last one
//! ends. The mtime caches of the loaders make a load without changes cheap.
//! The thread also scans the running Claude Code sessions every
//! [`RUNNING_SCAN`], and merges the output of the indexer thread. Each
//! change goes out as an [`Event`] on a broadcast channel.
//!
//! Lock order: a function that needs the store and one of the other locks
//! takes the store lock first. No function holds two of the other locks.

use crate::diff::build_diff;
use crate::embedder::Embedder;
use crate::history::HistoryEntry;
use crate::search::{self, ContentCache};
use crate::session::RunningSessions;
use crate::sources::{Load, LoadEvent, Sources};
use crate::store::Store;
use crate::triage::{Stop, Triage, TriageConfig};
use recap_api::{
    Agent, AgentsResponse, Api, ApiError, ApiResult, BlameRequest, DiffLine, DiffSummary, Entry,
    EntryId, Event, FileInfo, FilesResponse, Filter, HistoryResponse, HunkBlame, ListItem, Page,
    ProjectsResponse, RunningSession, SearchKind, SearchResponse, Status, TriageStatus,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// The interval of the manager loop. A batch from a loader thread waits at
/// most this long before the merge.
const TICK: Duration = Duration::from_millis(100);

/// The interval between two scans of the running Claude Code sessions.
const RUNNING_SCAN: Duration = Duration::from_secs(2);

/// The longest time that a semantic search waits for the model server to
/// load the model.
const MODEL_WAIT: Duration = Duration::from_secs(120);

/// The number of project filters that a [`ListCache`] keeps. The cache
/// forgets all lists when it reaches this number.
const LIST_CACHE_PROJECTS: usize = 16;

/// The last list that the server sent for each project filter, at the store
/// version of that list. A client that holds this version gets only the
/// change to the list (see [`ListItem`]).
struct ListCache<T> {
    lists: Mutex<HashMap<Option<PathBuf>, (u64, Arc<Vec<T>>)>>,
}

impl<T> Default for ListCache<T> {
    fn default() -> Self {
        ListCache {
            lists: Mutex::new(HashMap::new()),
        }
    }
}

/// The answer of [`ListCache::get`]: the items, the removed keys, and the
/// version that the change applies to (`None` for the full list).
struct ListAnswer<T: ListItem> {
    items: Vec<T>,
    removed: Vec<T::Key>,
    since: Option<u64>,
}

impl<T: ListItem> ListCache<T> {
    /// The list for `project` at the store version `version`, or only its
    /// change after `since` when the cache holds the list at `since`. The
    /// function calls `compute` when the cache does not hold the list at
    /// `version`.
    fn get(
        &self,
        project: Option<&Path>,
        version: u64,
        since: Option<u64>,
        compute: impl FnOnce() -> Vec<T>,
    ) -> ListAnswer<T> {
        let key = project.map(Path::to_path_buf);
        let old = lock(&self.lists).get(&key).cloned();
        let current = match &old {
            Some((v, list)) if *v == version => list.clone(),
            _ => Arc::new(compute()),
        };
        let answer = match old {
            Some((v, list)) if since == Some(v) => {
                let (items, removed) = recap_api::list_change(&list, &current);
                ListAnswer {
                    items,
                    removed,
                    since,
                }
            }
            _ => ListAnswer {
                items: current.as_ref().clone(),
                removed: Vec::new(),
                since: None,
            },
        };
        let mut lists = lock(&self.lists);
        if lists.len() >= LIST_CACHE_PROJECTS && !lists.contains_key(&key) {
            lists.clear();
        }
        lists.insert(key, (version, current));
        answer
    }
}

/// A request from an API call to the manager thread.
enum Command {
    Reload,
}

struct Inner {
    store: RwLock<Store>,
    content: Mutex<ContentCache>,
    summaries: Mutex<HashMap<EntryId, DiffSummary>>,
    embed: Mutex<Embedder>,
    running: RwLock<RunningSessions>,
    /// The last state of the triage runner, for `GET /v1/status`. The
    /// manager thread owns the runner.
    triage: RwLock<Option<TriageStatus>>,
    loading: AtomicBool,
    events: broadcast::Sender<Event>,
    files: ListCache<FileInfo>,
    agents: ListCache<Agent>,
    /// `None` for a server without a manager thread (the tests).
    commands: Option<Mutex<mpsc::Sender<Command>>>,
}

/// The recap server. A clone refers to the same server.
#[derive(Clone)]
pub struct Server {
    inner: Arc<Inner>,
}

fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// What one load read and changed, for the log.
#[derive(Default)]
struct LoadStats {
    started: Option<Instant>,
    /// The entries that the loader threads sent.
    received: usize,
    /// The entries that the merges changed. One entry can change in
    /// several merges of one load.
    changed: HashSet<EntryId>,
    /// The time from the start of the load to the end of each source.
    sources: Vec<(crate::sources::SourceKind, Duration)>,
}

impl Server {
    fn with_store(store: Store, commands: Option<mpsc::Sender<Command>>) -> Server {
        let (events, _) = broadcast::channel(256);
        Server {
            inner: Arc::new(Inner {
                store: RwLock::new(store),
                content: Mutex::new(ContentCache::new()),
                summaries: Mutex::new(HashMap::new()),
                embed: Mutex::new(Embedder::default()),
                running: RwLock::new(RunningSessions::default()),
                triage: RwLock::new(None),
                // A server with a manager thread starts with a load. Until
                // the first load starts, the store is empty but not complete.
                loading: AtomicBool::new(commands.is_some()),
                events,
                files: ListCache::default(),
                agents: ListCache::default(),
                commands: commands.map(Mutex::new),
            }),
        }
    }

    /// A server that holds `entries` and has no loaders and no manager
    /// thread. For tests.
    pub fn from_entries(entries: Vec<HistoryEntry>) -> Server {
        Server::from_store(Store::from_entries(entries))
    }

    /// A server that holds `store` and has no loaders and no manager
    /// thread. For tests.
    pub fn from_store(store: Store) -> Server {
        Server::with_store(store, None)
    }

    /// A server that loads all sources and keeps the store current. It
    /// starts a new load `poll` after the last load ends. With `triage`, it
    /// also starts a triage run when a Claude Code session stops.
    pub fn start(poll: Duration, triage: Option<TriageConfig>) -> Server {
        let (tx, rx) = mpsc::channel();
        let server = Server::with_store(Store::default(), Some(tx));
        let triage = triage.and_then(|config| match Triage::new(config) {
            Ok(t) => Some(t),
            Err(e) => {
                log::error!("triage: the runner is off: {e}");
                None
            }
        });
        if let Some(t) = &triage {
            log::info!("triage: the runner uses {}", t.status().dir.display());
            *write(&server.inner.triage) = Some(t.status());
        }
        let manager = server.clone();
        std::thread::Builder::new()
            .name("recap-manager".into())
            .spawn(move || manager.manage(Sources::detect(), rx, poll, triage))
            .expect("cannot start the manager thread");
        server
    }

    /// A receiver of the change events.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// The version of the store.
    pub fn version(&self) -> u64 {
        read(&self.inner.store).version()
    }

    pub(crate) fn send(&self, event: Event) {
        // An error only means that no client listens now.
        let _ = self.inner.events.send(event);
    }

    // ── The manager thread ──────────────────────────────────────────────────

    fn manage(
        &self,
        mut sources: Sources,
        commands: mpsc::Receiver<Command>,
        poll: Duration,
        mut triage: Option<Triage>,
    ) {
        let mut load: Option<Load> = None;
        let mut next_load = Instant::now();
        let mut next_scan = Instant::now();
        // The store version when the last indexer round started. A newer
        // version can hold entries that the index does not have.
        let mut indexed_version = 0u64;
        let mut stats = LoadStats::default();
        let mut first_load = true;
        loop {
            match commands.recv_timeout(TICK) {
                Ok(Command::Reload) => next_load = Instant::now(),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }

            if load.is_none() && Instant::now() >= next_load {
                load = sources.start_load();
                if load.is_some() {
                    stats = LoadStats {
                        started: Some(Instant::now()),
                        ..LoadStats::default()
                    };
                    self.set_loading(true);
                } else if self.inner.loading.load(Ordering::Relaxed) {
                    // No source exists, so the first load cannot start.
                    self.set_loading(false);
                }
            }
            if let Some(current) = load.as_mut()
                && self.drain_load(current, &mut sources, &mut stats)
            {
                load = None;
                next_load = Instant::now() + poll;
                self.set_loading(false);
                self.log_load(&stats, first_load);
                first_load = false;
            }

            self.drain_embed();
            let version = read(&self.inner.store).version();
            if version > indexed_version && lock(&self.inner.embed).is_ready() {
                indexed_version = version;
                self.start_embedding_round();
            }

            if Instant::now() >= next_scan {
                next_scan = Instant::now() + RUNNING_SCAN;
                let stops = self.rescan_running();
                if let Some(t) = triage.as_mut() {
                    t.add(stops);
                }
            }
            if let Some(t) = triage.as_mut() {
                let changed = t.tick(&read(&self.inner.running));
                if changed {
                    let status = t.status();
                    *write(&self.inner.triage) = Some(status.clone());
                    self.send(Event::Triage(status));
                    // The run wrote a new transcript. Load it now, so that
                    // the clients show what the triage agent did.
                    if !t.status().running {
                        next_load = next_load.min(Instant::now());
                    }
                }
            }
        }
    }

    /// Log a complete load. A load that changed nothing is a `debug` line,
    /// because the manager starts one load every poll interval.
    fn log_load(&self, stats: &LoadStats, first_load: bool) {
        let elapsed = stats
            .started
            .map(|s| crate::logger::ms(s.elapsed()))
            .unwrap_or_default();
        let (version, total) = {
            let store = read(&self.inner.store);
            (store.version(), store.len())
        };
        let what = if first_load { "first load" } else { "load" };
        let sources: Vec<String> = stats
            .sources
            .iter()
            .map(|(kind, time)| format!("{kind:?} {}", crate::logger::ms(*time)))
            .collect();
        let line = format!(
            "{what} complete in {elapsed}: {} entries received, {} changed, version {version}, {total} entries ({})",
            stats.received,
            stats.changed.len(),
            sources.join(", ")
        );
        if first_load || !stats.changed.is_empty() {
            log::info!("{line}");
        } else {
            log::debug!("{line}");
        }
    }

    fn set_loading(&self, loading: bool) {
        self.inner.loading.store(loading, Ordering::Relaxed);
        self.send(Event::Loading { loading });
    }

    /// Merge all the messages that the loader threads sent. Returns true when
    /// the load is complete.
    fn drain_load(&self, load: &mut Load, sources: &mut Sources, stats: &mut LoadStats) -> bool {
        let mut batch: Vec<HistoryEntry> = Vec::new();
        let mut titles: HashMap<String, String> = HashMap::new();
        let mut summaries: HashMap<String, String> = HashMap::new();
        let mut parents: HashMap<String, String> = HashMap::new();
        let mut subagent_parents: HashMap<String, String> = HashMap::new();
        let mut archived: Option<HashSet<String>> = None;
        let mut attribution = false;
        let mut disconnected = false;
        loop {
            match load.rx.try_recv() {
                Ok(LoadEvent::Batch(mut entries)) => batch.append(&mut entries),
                Ok(LoadEvent::SessionFacts(facts)) => {
                    titles.extend(facts.titles);
                    summaries.extend(facts.summaries);
                    parents.extend(facts.parents);
                }
                Ok(LoadEvent::ArchivedSessions(ids)) => archived = Some(ids),
                Ok(LoadEvent::CursorSessions { sessions, mtime }) => {
                    sources.cursor_sessions_mtime = mtime;
                    load.pending = load.pending.saturating_sub(1);
                    // The loader sends the previous map again when Cursor's
                    // database did not change. The store already holds its
                    // entries and its attribution.
                    if sources
                        .cursor_sessions
                        .as_ref()
                        .is_some_and(|prev| Arc::ptr_eq(prev, &sessions))
                    {
                        continue;
                    }
                    titles.extend(sessions.names_map().clone());
                    subagent_parents.extend(sessions.subagent_parents().clone());
                    let fresh = read(&self.inner.store).new_cursor_entries(&sessions, &batch);
                    batch.extend(fresh);
                    sources.cursor_sessions = Some(sessions);
                    attribution = true;
                }
                Ok(LoadEvent::SourceFinished { kind, mtimes, seen }) => {
                    let time = stats.started.map(|s| s.elapsed()).unwrap_or_default();
                    stats.sources.push((kind, time));
                    sources.finish(kind, mtimes, seen);
                    load.pending = load.pending.saturating_sub(1);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        stats.received += batch.len();
        if !batch.is_empty()
            || !titles.is_empty()
            || !summaries.is_empty()
            || !parents.is_empty()
            || !subagent_parents.is_empty()
            || archived.is_some()
            || attribution
        {
            sources.attach_plan_project(&mut batch);
            let merged = !batch.is_empty();
            let mut store = write(&self.inner.store);
            store.extend_titles(titles);
            store.extend_summaries(summaries);
            store.extend_parents(parents);
            store.extend_subagent_parents(subagent_parents);
            if let Some(ids) = archived {
                store.set_archived_sessions(ids);
            }
            if merged {
                store.merge(batch);
            }
            // Attribute the Cursor edits again when the map arrived, or when
            // new entries arrived while a map exists.
            if (attribution || merged)
                && let Some(sessions) = &sources.cursor_sessions
            {
                store.apply_cursor_attribution(sessions);
            }
            let changed = store.commit();
            let version = store.version();
            drop(store);
            stats.changed.extend(changed.iter().cloned());
            self.after_commit(&changed, version);
        }
        disconnected || load.pending == 0
    }

    /// Remove the cached values of the changed entries, and tell the clients.
    fn after_commit(&self, changed: &[EntryId], version: u64) {
        if changed.is_empty() {
            return;
        }
        {
            let mut content = lock(&self.inner.content);
            for id in changed {
                content.remove(id);
            }
        }
        {
            let mut summaries = lock(&self.inner.summaries);
            for id in changed {
                summaries.remove(id);
            }
        }
        self.send(Event::Changed { version });
    }

    fn drain_embed(&self) {
        let store = read(&self.inner.store);
        let mut embed = lock(&self.inner.embed);
        if embed.drain(&store) {
            let status = embed.status();
            drop(embed);
            drop(store);
            self.send(Event::Embed(status));
        }
    }

    fn start_embedding_round(&self) {
        let store = read(&self.inner.store);
        let mut embed = lock(&self.inner.embed);
        if embed.start(&store) {
            let status = embed.status();
            drop(embed);
            drop(store);
            self.send(Event::Embed(status));
        }
    }

    /// Scan the running sessions, and return the sessions that stopped their
    /// turn since the last scan.
    fn rescan_running(&self) -> Vec<Stop> {
        let scan = RunningSessions::scan();
        // Lock order: the store before the other locks.
        let store = read(&self.inner.store);
        let mut running = write(&self.inner.running);
        let mut stops = Vec::new();
        if *running != scan {
            stops = crate::triage::stops(&running, &scan, |sid| store.chain_id(sid).to_string());
            drop(store);
            let sessions = scan.list();
            let ids: Vec<String> = sessions
                .iter()
                .map(|s| {
                    format!(
                        "{} {}",
                        &s.session_id[..8.min(s.session_id.len())],
                        s.status.as_deref().unwrap_or("-")
                    )
                })
                .collect();
            log::info!(
                "running Claude Code sessions: {} [{}]",
                sessions.len(),
                ids.join(", ")
            );
            *running = scan;
            drop(running);
            self.send(Event::Running { sessions });
        }
        stops
    }

    // ── Helpers of the API calls ────────────────────────────────────────────

    fn index_of(store: &Store, id: &EntryId) -> ApiResult<usize> {
        store
            .index_of(id)
            .ok_or_else(|| ApiError(format!("no entry with id {id}")))
    }

    /// The lines that the Diff panel shows for one entry: the prompt and the
    /// response for a prompt, the summary for a compaction, the diff to the
    /// base for a file change.
    fn entry_diff(store: &Store, idx: usize) -> Vec<DiffLine> {
        let entry = &store.history.entries[idx];
        // The agent wrote the summary, so the summary shows as a response.
        if let Some(summary) = recap_api::EntryFields::compaction_summary(entry) {
            return summary
                .lines()
                .map(|l| DiffLine::Response(l.to_string()))
                .collect();
        }
        if let Some(text) = recap_api::EntryFields::prompt_text(entry) {
            let mut lines: Vec<DiffLine> = text
                .lines()
                .map(|l| DiffLine::Prompt(l.to_string()))
                .collect();
            if lines.is_empty() {
                lines.push(DiffLine::Prompt(String::new()));
            }
            // The response follows the prompt after one blank line.
            if let Some(response) = recap_api::EntryFields::response_text(entry) {
                lines.push(DiffLine::Context(String::new()));
                lines.extend(response.lines().map(|l| DiffLine::Response(l.to_string())));
            }
            return lines;
        }
        build_diff(store.prev_snapshot(idx), &entry.snapshot_path)
    }
}

/// Log one search: the query, the filter, the result count, what the search
/// read, and the time.
fn log_search(
    kind: SearchKind,
    query: &str,
    filter: &Filter,
    matches: usize,
    scan: &search::SearchScan,
    elapsed: Duration,
    embed_time: Option<Duration>,
) {
    let filter = describe_filter(filter);
    let time = crate::logger::ms(elapsed);
    let detail = match kind {
        SearchKind::Path => String::new(),
        SearchKind::Content => format!(
            ", {} cached, {} snapshot pairs read ({})",
            scan.cached,
            scan.snapshots_read,
            crate::logger::bytes(scan.bytes_read)
        ),
        SearchKind::Semantic => format!(
            ", {} without a vector, query embedded in {}",
            scan.without_vector,
            embed_time.map(crate::logger::ms).unwrap_or_default()
        ),
    };
    log::info!(
        "search {} {query:?}{filter}: {matches} matches in {time} ({} entries scanned{detail})",
        kind.as_str(),
        scan.candidates
    );
}

/// The filter as ` [project=/x kind=prompts]`, or nothing for no filter.
fn describe_filter(filter: &Filter) -> String {
    let pairs = filter.query_pairs();
    if pairs.is_empty() {
        return String::new();
    }
    let text: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!(" [{}]", text.join(" "))
}

impl Api for Server {
    fn status(&self) -> ApiResult<Status> {
        let (version, entries) = {
            let store = read(&self.inner.store);
            (store.version(), store.len())
        };
        Ok(Status {
            version,
            loading: self.inner.loading.load(Ordering::Relaxed),
            entries,
            embed: lock(&self.inner.embed).status(),
            running: read(&self.inner.running).len(),
            cache_dir: crate::paths::cache_root().clone(),
            triage: read(&self.inner.triage).clone(),
        })
    }

    fn projects(&self) -> ApiResult<ProjectsResponse> {
        let store = read(&self.inner.store);
        Ok(ProjectsResponse {
            version: store.version(),
            projects: store.projects(),
        })
    }

    fn files(&self, project: Option<&Path>, since: Option<u64>) -> ApiResult<FilesResponse> {
        let store = read(&self.inner.store);
        let version = store.version();
        let answer = self
            .inner
            .files
            .get(project, version, since, || store.files(project));
        Ok(FilesResponse {
            version,
            since: answer.since,
            files: answer.items,
            removed: answer.removed,
        })
    }

    fn agents(&self, project: Option<&Path>, since: Option<u64>) -> ApiResult<AgentsResponse> {
        let store = read(&self.inner.store);
        let version = store.version();
        let answer = self
            .inner
            .agents
            .get(project, version, since, || store.agents(project));
        Ok(AgentsResponse {
            version,
            since: answer.since,
            agents: answer.items,
            removed: answer.removed,
        })
    }

    fn history(&self, filter: &Filter, page: &Page) -> ApiResult<HistoryResponse> {
        Ok(read(&self.inner.store).history(filter, page))
    }

    fn entry(&self, id: &EntryId) -> ApiResult<Entry> {
        let store = read(&self.inner.store);
        let idx = Self::index_of(&store, id)?;
        Ok(store.wire(idx))
    }

    fn diff(&self, ids: &[EntryId]) -> ApiResult<Vec<DiffLine>> {
        let store = read(&self.inner.store);
        if let [id] = ids {
            let idx = Self::index_of(&store, id)?;
            return Ok(Self::entry_diff(&store, idx));
        }
        let mut lines = Vec::new();
        for id in ids {
            let Some(idx) = store.index_of(id) else {
                continue;
            };
            if !lines.is_empty() {
                lines.push(DiffLine::Header(String::new()));
            }
            lines.push(DiffLine::ChildSeparator(id.clone()));
            lines.extend(Self::entry_diff(&store, idx));
        }
        Ok(lines)
    }

    fn summaries(&self, ids: &[EntryId]) -> ApiResult<HashMap<EntryId, DiffSummary>> {
        let store = read(&self.inner.store);
        let mut cache = lock(&self.inner.summaries);
        let mut out = HashMap::new();
        let mut computed = 0usize;
        for id in ids {
            let Some(idx) = store.index_of(id) else {
                continue;
            };
            let summary = *cache.entry(id.clone()).or_insert_with(|| {
                computed += 1;
                let entry = &store.history.entries[idx];
                crate::diff::compute_summary(store.prev_snapshot(idx), &entry.snapshot_path)
            });
            out.insert(id.clone(), summary);
        }
        log::debug!(
            "summaries: {} asked, {computed} calculated, {} from the cache",
            ids.len(),
            ids.len() - computed
        );
        Ok(out)
    }

    fn search(&self, kind: SearchKind, query: &str, filter: &Filter) -> ApiResult<SearchResponse> {
        let started = Instant::now();
        let mut embed_time = None;
        let (matches, scan) = match kind {
            SearchKind::Path => search::path_matches(&read(&self.inner.store), filter, query),
            SearchKind::Content => {
                let store = read(&self.inner.store);
                let mut cache = lock(&self.inner.content);
                search::content_matches(&store, &mut cache, filter, query)
            }
            SearchKind::Semantic => {
                if lock(&self.inner.embed).is_off() {
                    self.start_embedding_round();
                }
                let Some(client) = lock(&self.inner.embed).client.clone() else {
                    let status = lock(&self.inner.embed).status();
                    log::warn!("search semantic {query:?}: not available: {status:?}");
                    return Err(ApiError(format!(
                        "semantic search is not available: {status:?}"
                    )));
                };
                // The model server needs a few seconds to load the model. A
                // query before that gets a refused connection, so wait here.
                // The wait costs one /health call when the server is ready.
                let query_vec = client
                    .wait_ready(MODEL_WAIT)
                    .and_then(|()| client.embed_query(query));
                embed_time = Some(started.elapsed());
                let query_vec = match query_vec {
                    Ok(q) => q,
                    Err(e) => {
                        log::error!("search semantic {query:?}: the model server failed: {e}");
                        lock(&self.inner.embed).fail(e.to_string());
                        self.send(Event::Embed(lock(&self.inner.embed).status()));
                        return Err(ApiError(e.to_string()));
                    }
                };
                let store = read(&self.inner.store);
                let embed = lock(&self.inner.embed);
                search::semantic_matches(&store, &embed.index, filter, &query_vec)
            }
        };
        log_search(
            kind,
            query,
            filter,
            matches.len(),
            &scan,
            started.elapsed(),
            embed_time,
        );
        Ok(SearchResponse {
            version: read(&self.inner.store).version(),
            matches,
        })
    }

    fn running(&self) -> ApiResult<Vec<RunningSession>> {
        Ok(read(&self.inner.running).list())
    }

    fn reload(&self) -> ApiResult<()> {
        log::info!("a client asked for a load now");
        if let Some(commands) = &self.inner.commands {
            lock(commands)
                .send(Command::Reload)
                .map_err(|_| ApiError("the manager thread stopped".to_string()))?;
        }
        Ok(())
    }

    fn start_embedding(&self) -> ApiResult<()> {
        self.start_embedding_round();
        Ok(())
    }

    fn blame(&self, request: &BlameRequest) -> ApiResult<Vec<HunkBlame>> {
        let files = recap_unidiff::parse_unified(&request.diff);
        let bash = match crate::claude::projects_dir() {
            Some(dir) => crate::claude::bash_calls(&dir, &crate::claude::archive_dir()),
            None => Vec::new(),
        };
        let store = read(&self.inner.store);
        Ok(crate::blame::attribute(
            &store.history,
            &files,
            &request.repo_root,
            &bash,
        ))
    }

    fn restore_session(&self, session_id: &str) -> ApiResult<()> {
        let dir = crate::claude::projects_dir().ok_or_else(|| {
            ApiError("the Claude Code projects directory does not exist".to_string())
        })?;
        crate::claude::restore_session(&dir, &crate::claude::archive_dir(), session_id).map_err(
            |e| {
                log::error!("restore session {session_id}: {e}");
                ApiError(e.to_string())
            },
        )?;
        // The next load reads the restored file and sends the new set of
        // archived sessions.
        self.reload()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    fn file(path: &str, count: usize) -> FileInfo {
        FileInfo {
            path: PathBuf::from(path),
            latest: Local::now(),
            count,
            chain: vec![],
        }
    }

    #[test]
    fn the_list_cache_sends_only_the_change_to_a_known_version() {
        let cache: ListCache<FileInfo> = ListCache::default();
        let a = file("/a", 1);
        let b = file("/b", 1);

        // A client without a list gets the full list.
        let full = cache.get(None, 1, None, || vec![a.clone(), b.clone()]);
        assert_eq!(full.since, None);
        assert_eq!(full.items.len(), 2);

        // The same version needs no new calculation, and has no change.
        let same = cache.get(None, 1, Some(1), || panic!("the cache holds version 1"));
        assert_eq!(same.since, Some(1));
        assert!(same.items.is_empty() && same.removed.is_empty());

        // The next version sends the changed file and the removed file.
        let a2 = FileInfo { count: 2, ..a.clone() };
        let next = cache.get(None, 2, Some(1), || vec![a2.clone()]);
        assert_eq!(next.since, Some(1));
        assert_eq!(next.items, vec![a2]);
        assert_eq!(next.removed, vec![PathBuf::from("/b")]);

        // An unknown version or another project gets the full list.
        let old = cache.get(None, 2, Some(1), || unreachable!());
        assert_eq!(old.since, None);
        let other = cache.get(Some(Path::new("/p")), 2, Some(2), || vec![b.clone()]);
        assert_eq!(other.since, None);
        assert_eq!(other.items, vec![b]);
    }
}
