//! The semantic search subsystem of the server: the model server process,
//! the vector index in memory, and the indexer thread.
//!
//! The model server starts only when a client asks for it (`POST
//! /v1/embed/start`, or the first semantic search), because it holds GPU
//! memory. After the first round, the server manager starts a new indexer
//! round each time new entries arrive, so the index stays current.

use crate::embed::{EmbedClient, EmbedConfig, EmbedDb, EmbedIndex, EmbedKind, EmbedServer};
use crate::history::HistoryEntry;
use crate::store::{Store, entry_key};
use recap_api::{DiffLine, EmbedStatus, EntryFields, EntryKind};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

/// The searchable text of a prompt entry: the prompt, then the response
/// after a blank line when the turn has one.
pub fn turn_text(entry: &HistoryEntry) -> String {
    let prompt = entry.prompt_text().unwrap_or("");
    match entry.response_text() {
        Some(response) => format!("{prompt}\n\n{response}"),
        None => prompt.to_string(),
    }
}

/// Read the two file contents that a file-change entry diffs.
///
/// This function and [`diff_text`] hold the one definition of "the
/// searchable text of an entry". The content search and the indexer both
/// build their text from them, so the two searches agree about what one
/// entry says.
pub fn entry_snapshots(store: &Store, idx: usize) -> (String, String) {
    let entry = &store.history.entries[idx];
    let new = std::fs::read_to_string(&entry.snapshot_path).unwrap_or_default();
    let old = match store.prev_snapshot(idx) {
        Some(p) => std::fs::read_to_string(p).unwrap_or_default(),
        None => String::new(),
    };
    (old, new)
}

/// The `+` and `-` bodies of a diff, one on each line.
pub fn diff_text(old: &str, new: &str) -> String {
    let mut text = String::new();
    for line in crate::diff::build_diff_from(old, new) {
        if let DiffLine::Added(l) | DiffLine::Removed(l) = line {
            text.push_str(&l);
            text.push('\n');
        }
    }
    text
}

/// One entry that the indexer must embed. The job holds all the data that
/// the indexer thread needs, so the thread never reads the store.
struct EmbedJob {
    key: String,
    kind: EmbedKind,
    /// The prompt text. `None` for a file change: the indexer thread builds
    /// its text from the two snapshots.
    text: Option<String>,
    new_path: PathBuf,
    old_path: Option<PathBuf>,
}

/// A message from the indexer thread.
enum EmbedEvent {
    /// The vectors that the database already held, read once at the start.
    Index(EmbedIndex),
    /// One embedded batch, already written to the database.
    Batch(Vec<(String, EmbedKind, Vec<f32>)>),
    Done,
    Failed(String),
}

/// Embed each job, write each batch to the database, and send each batch
/// back, so a search can use it at once.
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
            return; // The server stopped.
        }
    }
    let _ = tx.send(EmbedEvent::Done);
}

/// What the subsystem does now. [`Embedder::status`] converts it to the wire
/// form.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Off,
    Starting,
    Indexing,
    Ready,
    Failed(String),
}

pub struct Embedder {
    pub cfg: EmbedConfig,
    pub index: EmbedIndex,
    pub client: Option<EmbedClient>,
    /// The model server that this recap server started. `None` when the
    /// model server of another recap server answered on the port. Dropping
    /// it stops the process.
    server: Option<EmbedServer>,
    rx: Option<mpsc::Receiver<EmbedEvent>>,
    state: State,
    /// Entries embedded and entries queued in the current indexer round.
    done: usize,
    total: usize,
    /// The start of the current indexer round, for the log.
    round_started: Option<std::time::Instant>,
}

impl Default for Embedder {
    fn default() -> Self {
        let cfg = EmbedConfig::load();
        let index = EmbedIndex::new(cfg.dims);
        Embedder {
            cfg,
            index,
            client: None,
            server: None,
            rx: None,
            state: State::Off,
            done: 0,
            total: 0,
            round_started: None,
        }
    }
}

impl Embedder {
    pub fn status(&self) -> EmbedStatus {
        match &self.state {
            State::Off => EmbedStatus::Off,
            State::Starting => EmbedStatus::Starting,
            State::Indexing => EmbedStatus::Indexing {
                done: self.done,
                total: self.total,
            },
            State::Ready => EmbedStatus::Ready,
            State::Failed(e) => EmbedStatus::Failed { error: e.clone() },
        }
    }

    pub fn is_off(&self) -> bool {
        self.state == State::Off
    }

    /// True when the first round is complete and no round runs. The manager
    /// then starts a new round for the entries that arrived after it.
    pub fn is_ready(&self) -> bool {
        self.state == State::Ready
    }

    /// One job for each entry that the index does not hold. Prompts come
    /// first: they are short, and they hold the user's own words, which a
    /// semantic query most often looks for.
    fn jobs(&self, store: &Store) -> Vec<EmbedJob> {
        let mut jobs = Vec::new();
        for (idx, entry) in store.history.entries.iter().enumerate() {
            let key = entry_key(entry);
            if self.index.contains(&key) {
                continue;
            }
            match &entry.kind {
                EntryKind::Prompt { .. } => jobs.push(EmbedJob {
                    key,
                    kind: EmbedKind::Prompt,
                    text: Some(turn_text(entry)),
                    new_path: entry.file_path.clone(),
                    old_path: None,
                }),
                EntryKind::FileChange => jobs.push(EmbedJob {
                    key,
                    kind: EmbedKind::Diff,
                    text: None,
                    new_path: entry.snapshot_path.clone(),
                    old_path: store.prev_snapshot(idx).map(|p| p.to_path_buf()),
                }),
                // The agent wrote the summary of a compaction. It repeats the
                // earlier prompts, so the index does not hold it.
                EntryKind::Compaction { .. } => {}
            }
        }
        jobs.sort_by_key(|j| match j.kind {
            EmbedKind::Prompt => 0,
            EmbedKind::Diff => 1,
        });
        jobs
    }

    /// Start the model server and the indexer, or start one more indexer
    /// round for the entries that arrived after the last round. The function
    /// returns at once: the work runs on a thread. It returns true if the
    /// status changed.
    pub fn start(&mut self, store: &Store) -> bool {
        let before = self.status();
        match self.state {
            // A round runs already.
            State::Starting | State::Indexing => return false,
            State::Failed(_) | State::Off | State::Ready => {}
        }
        if let Some(msg) = self.cfg.missing_parts() {
            log::error!("embedding: cannot start: {msg}");
            self.state = State::Failed(msg);
            return self.status() != before;
        }

        let first_round = self.client.is_none();
        if first_round {
            let client = EmbedClient::new(&self.cfg);
            if !client.is_ready() {
                match EmbedServer::spawn(&self.cfg) {
                    Ok(server) => {
                        log::info!("embedding: started the model server");
                        self.server = Some(server);
                    }
                    Err(e) => {
                        log::error!("embedding: cannot start the model server: {e}");
                        self.state = State::Failed(e.to_string());
                        return self.status() != before;
                    }
                }
            } else {
                log::info!(
                    "embedding: a model server already answers on its port. The server uses it"
                );
            }
            self.client = Some(client);
        }

        let jobs = self.jobs(store);
        if !first_round && jobs.is_empty() {
            self.state = State::Ready;
            return self.status() != before;
        }

        let (tx, rx) = mpsc::channel();
        let cfg = self.cfg.clone();
        let Some(client) = self.client.clone() else {
            return false;
        };
        self.rx = Some(rx);
        self.done = 0;
        self.total = jobs.len();
        self.state = if first_round {
            State::Starting
        } else {
            State::Indexing
        };
        self.round_started = Some(std::time::Instant::now());
        log::info!(
            "embedding: {} round started: {} entries without a vector{}",
            if first_round { "first" } else { "next" },
            self.total,
            if first_round {
                " (the stored vectors are not read yet)"
            } else {
                ""
            }
        );
        std::thread::spawn(move || run_indexer(cfg, client, jobs, first_round, tx));
        self.status() != before
    }

    /// Merge the output of the indexer thread. It returns true if the status
    /// changed.
    pub fn drain(&mut self, store: &Store) -> bool {
        let Some(rx) = &self.rx else {
            return false;
        };
        let before = self.status();
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(EmbedEvent::Index(index)) => {
                    // The jobs whose key the database already held need no work.
                    let known = store
                        .history
                        .entries
                        .iter()
                        .filter(|e| index.contains(&entry_key(e)))
                        .count();
                    self.total = self.total.saturating_sub(known);
                    log::info!(
                        "embedding: read {} stored vectors. {} entries need a vector",
                        index.len(),
                        self.total
                    );
                    self.index = index;
                    self.state = State::Indexing;
                }
                Ok(EmbedEvent::Batch(rows)) => {
                    let before = self.done;
                    self.done += rows.len();
                    // One line for each 500 vectors: a first round embeds
                    // tens of thousands of entries.
                    if before / 500 != self.done / 500 {
                        log::info!("embedding: {}/{} entries embedded", self.done, self.total);
                    } else {
                        log::debug!("embedding: {}/{} entries embedded", self.done, self.total);
                    }
                    for (key, kind, vec) in rows {
                        self.index.insert(key, kind, &vec);
                    }
                    self.state = State::Indexing;
                }
                Ok(EmbedEvent::Done) => {
                    log::info!(
                        "embedding: round complete in {}: {} entries embedded, {} vectors in the index",
                        self.round_started
                            .map(|s| crate::logger::ms(s.elapsed()))
                            .unwrap_or_default(),
                        self.done,
                        self.index.len()
                    );
                    self.state = State::Ready;
                    finished = true;
                }
                Ok(EmbedEvent::Failed(e)) => {
                    log::error!("embedding: the round failed: {e}");
                    self.state = State::Failed(e);
                    // Stop the model server, so the next start begins a clean
                    // round and does not trust a process that did not answer.
                    self.server = None;
                    self.client = None;
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
            self.rx = None;
        }
        self.status() != before
    }

    /// Record a failure that a search found, for example a model server
    /// that stopped.
    pub fn fail(&mut self, error: String) {
        log::error!("embedding: {error}");
        self.state = State::Failed(error);
        self.server = None;
        self.client = None;
    }
}
