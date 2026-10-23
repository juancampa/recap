//! The external entry sources and the loader threads that read them.
//!
//! [`Sources`] holds the directory of each source and the per-source caches
//! (file mtimes and seen keys), so a later load emits only the new entries.
//! [`Sources::start_load`] starts one thread per source. The threads send
//! [`LoadEvent`]s on a channel. The server manager thread merges them (see
//! [`crate::server`]).

use crate::cursor_sessions::CursorSessions;
use crate::history::{HistoryEntry, HistoryStore, find_project_chain};
use crate::{claude, cursor_workspace, nvim};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::SystemTime;

/// The external entry sources, with the caches of the last load. Each source
/// has its own seen-set, because the three sources produce disjoint keys and
/// the split sets let each loader thread own its state without locks.
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
    /// `~/.cursor/plans/X.plan.md` → the workspace folder that opened it.
    /// The code reads it once from Cursor's workspaceStorage.
    pub plan_project_map: HashMap<PathBuf, PathBuf>,
    /// The `find_project_chain` result of each plan workspace, so the code
    /// does not walk the ancestors again for each plan entry.
    plan_chain_cache: HashMap<PathBuf, Vec<PathBuf>>,
    /// The Cursor composer attribution map, and the database mtime that it
    /// comes from. A load builds the map again only when Cursor's database
    /// changed, because the scan reads several GB. See
    /// [`crate::cursor_sessions`].
    pub cursor_sessions: Option<Arc<CursorSessions>>,
    pub cursor_sessions_mtime: Option<SystemTime>,
}

/// The source that a loader thread reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Cursor,
    Claude,
    Nvim,
}

/// A message from a loader thread. The threads send batches while they run.
/// Each thread sends one `SourceFinished` at the end, with its updated mtime
/// and seen-key caches, so the next load continues from this load.
pub enum LoadEvent {
    Batch(Vec<HistoryEntry>),
    /// The Cursor composer attribution map, and the database mtime that it
    /// comes from.
    CursorSessions {
        sessions: Arc<CursorSessions>,
        mtime: Option<SystemTime>,
    },
    /// The titles, summaries and parents of sessions, from the Claude Code
    /// session files.
    SessionFacts(claude::SessionFacts),
    /// The ids of all the Claude Code sessions that Claude Code deleted and
    /// that the loader read from the archive (see [`claude::load`]). The
    /// Claude loader sends it once in each load.
    ArchivedSessions(HashSet<String>),
    SourceFinished {
        kind: SourceKind,
        mtimes: HashMap<PathBuf, SystemTime>,
        seen: HashSet<PathBuf>,
    },
}

/// A load in progress: the channel of the loader threads, and the number of
/// messages that still must arrive before the load is complete.
pub struct Load {
    pub rx: mpsc::Receiver<LoadEvent>,
    /// The loader threads that did not send `SourceFinished` yet, plus the
    /// Cursor attribution thread until it sends `CursorSessions`.
    pub pending: u8,
}

impl Sources {
    /// Find the directory of each source (Cursor/VSCode history, the Claude
    /// projects directory, the Neovim undodir). A missing source stays `None`.
    pub fn detect() -> Self {
        Self {
            cursor_dir: cursor_history_dir(),
            claude_dir: claude::projects_dir(),
            nvim_dir: nvim::undodir(),
            plan_project_map: cursor_workspace::build_plan_project_map(),
            ..Self::default()
        }
    }

    /// `find_project_chain` walks up from a file to find project markers.
    /// For `~/.cursor/plans/<x>.plan.md` the walk finds nothing, because the
    /// plan directory has no marker. This function sets the chain from the
    /// workspace storage map: a known plan gets the chain of its workspace,
    /// and every other plan gets a synthetic `cursor plans` project, so it
    /// does not go into "no project".
    pub fn attach_plan_project(&mut self, entries: &mut [HistoryEntry]) {
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

    /// Start one loader thread for each detected source. Each thread sends
    /// its batches newest first. The function returns at once. It returns
    /// `None` when no source exists.
    pub fn start_load(&mut self) -> Option<Load> {
        let (tx, rx) = mpsc::channel::<LoadEvent>();
        let mut pending = 0u8;
        crate::history::clear_dir_states();

        if let Some(dir) = self.cursor_dir.clone() {
            let mut mtimes = std::mem::take(&mut self.cursor_mtimes);
            let mut seen = std::mem::take(&mut self.cursor_seen);
            let loader_tx = tx.clone();
            pending += 1;
            std::thread::spawn(move || {
                let mut on_batch = |batch: Vec<HistoryEntry>| -> bool {
                    loader_tx.send(LoadEvent::Batch(batch)).is_ok()
                };
                if let Err(e) =
                    HistoryStore::load_cursor(&dir, &mut mtimes, &mut seen, None, &mut on_batch)
                {
                    log::warn!("cursor history failed: {e}");
                }
                let _ = loader_tx.send(LoadEvent::SourceFinished {
                    kind: SourceKind::Cursor,
                    mtimes,
                    seen,
                });
            });

            // The attribution map has its own thread, so the scan of the
            // multi-GB database never delays the Cursor edits. The map is
            // cached by the database mtime, so an unchanged database returns
            // at once.
            let prev = self.cursor_sessions.clone();
            let prev_mtime = self.cursor_sessions_mtime;
            let attrib_tx = tx.clone();
            pending += 1;
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                let unchanged = prev.clone();
                let (sessions, mtime) = CursorSessions::load_cached(prev, prev_mtime);
                if !unchanged.is_some_and(|p| Arc::ptr_eq(&p, &sessions)) {
                    log::info!(
                        "read the Cursor composer database in {}: {} prompts, {} edits",
                        crate::logger::ms(started.elapsed()),
                        sessions.prompts().len(),
                        sessions.edits().len()
                    );
                }
                let _ = attrib_tx.send(LoadEvent::CursorSessions { sessions, mtime });
            });
        }

        if let Some(dir) = self.claude_dir.clone() {
            let mut mtimes = std::mem::take(&mut self.claude_mtimes);
            let mut seen = std::mem::take(&mut self.claude_seen);
            let tx = tx.clone();
            pending += 1;
            std::thread::spawn(move || {
                let mut on_batch =
                    |batch: Vec<HistoryEntry>, facts: claude::SessionFacts| -> bool {
                        if !facts.is_empty() && tx.send(LoadEvent::SessionFacts(facts)).is_err() {
                            return false;
                        }
                        if batch.is_empty() {
                            return true;
                        }
                        tx.send(LoadEvent::Batch(batch)).is_ok()
                    };
                match claude::load(
                    &dir,
                    &claude::archive_dir(),
                    &mut mtimes,
                    &mut seen,
                    None,
                    &mut on_batch,
                ) {
                    Ok(archived) => {
                        let _ = tx.send(LoadEvent::ArchivedSessions(archived));
                    }
                    Err(e) => log::warn!("claude history failed: {e}"),
                }
                let _ = tx.send(LoadEvent::SourceFinished {
                    kind: SourceKind::Claude,
                    mtimes,
                    seen,
                });
            });
        }

        if let Some(dir) = self.nvim_dir.clone() {
            let mut mtimes = std::mem::take(&mut self.nvim_mtimes);
            let mut seen = std::mem::take(&mut self.nvim_seen);
            let tx = tx.clone();
            pending += 1;
            std::thread::spawn(move || {
                let mut on_batch =
                    |batch: Vec<HistoryEntry>| -> bool { tx.send(LoadEvent::Batch(batch)).is_ok() };
                if let Err(e) = nvim::load(&dir, &mut mtimes, &mut seen, None, &mut on_batch) {
                    log::warn!("neovim history failed: {e}");
                }
                let _ = tx.send(LoadEvent::SourceFinished {
                    kind: SourceKind::Nvim,
                    mtimes,
                    seen,
                });
            });
        }

        (pending > 0).then_some(Load { rx, pending })
    }

    /// Put the caches that a finished loader thread returns back in place.
    pub fn finish(
        &mut self,
        kind: SourceKind,
        mtimes: HashMap<PathBuf, SystemTime>,
        seen: HashSet<PathBuf>,
    ) {
        match kind {
            SourceKind::Cursor => {
                self.cursor_mtimes = mtimes;
                self.cursor_seen = seen;
            }
            SourceKind::Claude => {
                self.claude_mtimes = mtimes;
                self.claude_seen = seen;
            }
            SourceKind::Nvim => {
                self.nvim_mtimes = mtimes;
                self.nvim_seen = seen;
            }
        }
    }
}

/// The project chain of a file inside `dir`. The probe `dir/.` lets `dir`
/// itself be a project root.
fn chain_for_dir(dir: &Path) -> Vec<PathBuf> {
    let probe = dir.join(".");
    let mut chain = find_project_chain(&probe);
    // If the workspace folder has no project marker (for example `~/git/mnode`
    // with several sub-projects), use the folder as the only project, so the
    // display names still show it.
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
