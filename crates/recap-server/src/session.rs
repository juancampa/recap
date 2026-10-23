//! Detect running Claude Code sessions.
//!
//! Claude Code writes one `~/.claude/sessions/<pid>.json` file per live
//! session, keyed by process id and carrying the session id, cwd, and a
//! coarse status. We scan that directory and index the entries by session id
//! so a History entry (which knows its `<session>.jsonl` id) can ask "is this
//! session still running, and if so on what pid?".
//!
//! Liveness: the files are cleaned up on a clean exit, but a crash can leave
//! a stale one behind, so every entry is validated with `kill(pid, 0)` before
//! it's trusted. Pid reuse is not guarded against here (it would require
//! comparing `procStart` to the live process's start time); a reused pid is
//! rare and the worst case is focusing the wrong window.

use recap_api::RunningSession;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct SessionFile {
    pid: i32,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    status: Option<String>,
}

/// Snapshot of the live sessions, indexed by session id. Rebuild with
/// [`RunningSessions::scan`] to refresh (cheap: a handful of small JSON files).
#[derive(Default, PartialEq, Eq)]
pub struct RunningSessions {
    by_session: HashMap<String, RunningSession>,
}

fn sessions_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".claude/sessions");
    dir.exists().then_some(dir)
}

/// True if `pid` names a live process this user can signal.
fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // Signal 0 performs error checking without delivering a signal: success
    // means the process exists and we have permission to signal it.
    unsafe { libc::kill(pid, 0) == 0 }
}

impl RunningSessions {
    /// Scan `~/.claude/sessions/*.json`, keeping only entries whose pid is
    /// alive. Missing dir / unreadable files yield an empty index rather than
    /// an error — a missing registry just means "nothing running".
    pub fn scan() -> Self {
        let mut by_session = HashMap::new();
        let Some(dir) = sessions_dir() else {
            return Self { by_session };
        };
        let Ok(read) = fs::read_dir(&dir) else {
            return Self { by_session };
        };
        for f in read.flatten() {
            let p = f.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(data) = fs::read_to_string(&p) else {
                continue;
            };
            let Ok(sf) = serde_json::from_str::<SessionFile>(&data) else {
                continue;
            };
            let Some(sid) = sf.session_id else { continue };
            if !pid_alive(sf.pid) {
                continue;
            }
            by_session.insert(
                sid.clone(),
                RunningSession {
                    session_id: sid,
                    pid: sf.pid,
                    cwd: sf.cwd.map(PathBuf::from),
                    status: sf.status,
                },
            );
        }
        Self { by_session }
    }

    /// The running session for `session_id`, if one is live.
    pub fn get(&self, session_id: &str) -> Option<&RunningSession> {
        self.by_session.get(session_id)
    }

    pub fn is_running(&self, session_id: &str) -> bool {
        self.by_session.contains_key(session_id)
    }

    pub fn len(&self) -> usize {
        self.by_session.len()
    }

    /// All running sessions, sorted by session id.
    pub fn list(&self) -> Vec<RunningSession> {
        let mut out: Vec<RunningSession> = self.by_session.values().cloned().collect();
        out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        out
    }
}
