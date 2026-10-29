//! The triage runner. It starts a triage agent when a Claude Code session
//! stops its turn.
//!
//! The runner keeps no triage data. The triage agent keeps all its data as
//! text files in the triage directory (see `triage_prompt.md`). The runner
//! only detects the stops and starts the runs of `claude -p`.
//!
//! A stop is a change of the status of a running session from `busy` to
//! `idle`, or a change to `waiting`. The manager thread compares two scans
//! of `~/.claude/sessions` (see [`stops`]), so a stop does not need a hook
//! and does not depend on the focus of the terminal window.
//!
//! One run is active at a time. The stops that occur during a run wait, and
//! the next run gets all of them. At the end of each run, the runner reads
//! the `next-run` file that the triage agent wrote, and starts a run at that
//! time if no stop starts one first.

use crate::session::RunningSessions;
use chrono::{DateTime, Local};
use recap_api::{RunningSession, TriageStatus};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// The instructions of the triage agent. The runner gives them to `claude`
/// with `--append-system-prompt`.
const INSTRUCTIONS: &str = include_str!("triage_prompt.md");

/// The name of the file in the triage directory that holds the time of the
/// next scheduled run.
const NEXT_RUN_FILE: &str = "next-run";

/// The runner stops a run that takes longer than this time.
const MAX_RUN: Duration = Duration::from_secs(15 * 60);

/// The settings of the triage runner.
#[derive(Debug, Clone)]
pub struct TriageConfig {
    /// The directory of the text files of the triage agent.
    pub dir: PathBuf,
    /// The `claude` binary.
    pub claude: PathBuf,
    /// The port of this server. The triage agent reads the sessions through
    /// the API on this port.
    pub port: u16,
}

/// One stop of a Claude Code session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stop {
    pub session_id: String,
    /// The id of the session chain. The triage agent keeps one file for
    /// each chain.
    pub chain_id: String,
    pub cwd: Option<PathBuf>,
    /// The new status: `idle` or `waiting`.
    pub status: String,
    pub waiting_for: Option<String>,
    pub at: DateTime<Local>,
}

/// The stops between two scans of the running sessions. A session that is
/// not in `old` gives no stop, because the server does not know its earlier
/// status. `chain_id` gives the chain of a session id.
pub fn stops(
    old: &RunningSessions,
    new: &RunningSessions,
    chain_id: impl Fn(&str) -> String,
) -> Vec<Stop> {
    let mut out = Vec::new();
    for s in new.list() {
        let Some(before) = old.get(&s.session_id) else {
            continue;
        };
        let before = before.status.as_deref();
        let now = s.status.as_deref();
        let stop = match now {
            Some("idle") => before == Some("busy"),
            Some("waiting") => before != Some("waiting"),
            _ => false,
        };
        if stop {
            out.push(Stop {
                chain_id: chain_id(&s.session_id),
                session_id: s.session_id.clone(),
                cwd: s.cwd.clone(),
                status: now.unwrap_or_default().to_string(),
                waiting_for: s.waiting_for.clone(),
                at: s.status_since.unwrap_or_else(Local::now),
            });
        }
    }
    out
}

/// An active run. A drop stops the process.
struct Run {
    child: Child,
    started: Instant,
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The triage runner. The manager thread owns it.
pub struct Triage {
    config: TriageConfig,
    /// The stops that wait for the next run.
    pending: Vec<Stop>,
    run: Option<Run>,
    /// The time that the `next-run` file gave at the end of the last run.
    next_run: Option<SystemTime>,
}

impl Triage {
    /// A runner for `config`. It makes the triage directory if it does not
    /// exist.
    pub fn new(config: TriageConfig) -> anyhow::Result<Triage> {
        std::fs::create_dir_all(config.dir.join("agents"))?;
        let dir = config.dir.canonicalize()?;
        let next_run = read_next_run(&dir);
        Ok(Triage {
            config: TriageConfig { dir, ..config },
            pending: Vec::new(),
            run: None,
            next_run,
        })
    }

    pub fn status(&self) -> TriageStatus {
        TriageStatus {
            dir: self.config.dir.clone(),
            running: self.run.is_some(),
            pending: self.pending.len(),
            next_run: self.next_run.map(DateTime::<Local>::from),
        }
    }

    /// Add stops for the next run. The runner ignores the stops of its own
    /// runs: a triage run is also a Claude Code session, with the triage
    /// directory as its cwd.
    pub fn add(&mut self, stops: Vec<Stop>) {
        for stop in stops {
            if stop.cwd.as_deref().is_some_and(|c| self.is_triage_dir(c)) {
                continue;
            }
            log::info!(
                "stop: session {} in {} is {}{}",
                short(&stop.session_id),
                stop.cwd
                    .as_deref()
                    .map(Path::display)
                    .map(|d| d.to_string())
                    .unwrap_or_default(),
                stop.status,
                stop.waiting_for
                    .as_deref()
                    .map(|w| format!(" ({w})"))
                    .unwrap_or_default(),
            );
            self.pending.retain(|p| p.session_id != stop.session_id);
            self.pending.push(stop);
        }
    }

    fn is_triage_dir(&self, path: &Path) -> bool {
        path == self.config.dir || path.canonicalize().is_ok_and(|p| p == self.config.dir)
    }

    /// End a run that finished, and start a run when one is due. `running`
    /// is the last scan of the running sessions. It returns true when the
    /// state changed.
    pub fn tick(&mut self, running: &RunningSessions) -> bool {
        if let Some(run) = self.run.as_mut() {
            match run.child.try_wait() {
                Ok(None) if run.started.elapsed() < MAX_RUN => return false,
                Ok(None) => log::warn!("triage run: stopped after {}", crate::logger::ms(MAX_RUN)),
                Ok(Some(status)) => log::info!(
                    "triage run: ended with {status} after {}",
                    crate::logger::ms(run.started.elapsed())
                ),
                Err(e) => log::warn!("triage run: {e}"),
            }
            self.run = None;
            self.next_run = read_next_run(&self.config.dir);
            return true;
        }
        let scheduled = self.next_run.is_some_and(|t| SystemTime::now() >= t);
        if self.pending.is_empty() && !scheduled {
            return false;
        }
        let stops = std::mem::take(&mut self.pending);
        // A scheduled run uses its time. Without a new `next-run` file, the
        // next run waits for a stop.
        self.next_run = None;
        let prompt = run_prompt(&self.config, &stops, running);
        match self.start(&prompt) {
            Ok(run) => {
                log::info!("triage run: started for {} stops", stops.len());
                self.run = Some(run);
            }
            // The stops of a run that cannot start are lost. A retry at each
            // tick would fill the log.
            Err(e) => log::error!(
                "triage run: cannot start {}: {e}",
                self.config.claude.display()
            ),
        }
        true
    }

    fn start(&self, prompt: &str) -> anyhow::Result<Run> {
        let mut child = Command::new(&self.config.claude)
            .current_dir(&self.config.dir)
            .args([
                "-p",
                "--model",
                "opus",
                "--append-system-prompt",
                INSTRUCTIONS,
            ])
            .arg("--allowedTools")
            .arg(allowed_tools(self.config.port))
            .env("PATH", child_path())
            .env(
                "RECAP_URL",
                format!("http://127.0.0.1:{}", self.config.port),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // `claude -p` reads the prompt from stdin, because `--allowedTools`
        // takes all the arguments that follow it.
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes())?;
        }
        for (name, pipe) in [
            (
                "stdout",
                child
                    .stdout
                    .take()
                    .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
            ),
            (
                "stderr",
                child
                    .stderr
                    .take()
                    .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
            ),
        ] {
            let Some(pipe) = pipe else { continue };
            std::thread::Builder::new()
                .name(format!("triage-{name}"))
                .spawn(move || {
                    for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                        log::info!("triage {name}: {line}");
                    }
                })?;
        }
        Ok(Run {
            child,
            started: Instant::now(),
        })
    }
}

fn short(id: &str) -> &str {
    &id[..8.min(id.len())]
}

/// The tools that a triage run can use without a permission prompt.
/// `claude -p` refuses all other tools.
fn allowed_tools(port: u16) -> String {
    [
        "Read".to_string(),
        "Glob".to_string(),
        "Grep".to_string(),
        "Write(./**)".to_string(),
        "Edit(./**)".to_string(),
        "Agent".to_string(),
        "Bash(recap list:*)".to_string(),
        format!("Bash(curl -s http://127.0.0.1:{port}/v1/*)"),
        "Bash(git -C * log:*)".to_string(),
        "Bash(git -C * status:*)".to_string(),
        "Bash(git -C * diff:*)".to_string(),
        "Bash(git -C * show:*)".to_string(),
        "Bash(jq:*)".to_string(),
        "Bash(hs -c:*)".to_string(),
    ]
    .join(",")
}

/// The PATH of a triage run. launchd gives the server a short PATH, so the
/// runner adds the directory of this binary (for `recap`) and the usual
/// directories of `git`, `curl` and `hs`.
fn child_path() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs: Vec<String> = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        dirs.push(dir.to_string_lossy().into_owned());
    }
    dirs.push(format!("{home}/.local/bin"));
    dirs.push("/opt/homebrew/bin".to_string());
    dirs.push("/usr/local/bin".to_string());
    if let Ok(path) = std::env::var("PATH") {
        dirs.push(path);
    }
    dirs.join(":")
}

/// The user message of a run: the time, the stops and the running sessions.
fn run_prompt(config: &TriageConfig, stops: &[Stop], running: &RunningSessions) -> String {
    let mut s = String::new();
    let now = Local::now();
    let _ = writeln!(s, "Time: {}", now.format("%Y-%m-%d %H:%M %:z (%A)"));
    let _ = writeln!(s, "recap server: http://127.0.0.1:{}", config.port);
    let _ = writeln!(s, "Triage directory: {}", config.dir.display());
    let _ = writeln!(s);
    if stops.is_empty() {
        let _ = writeln!(
            s,
            "Reason: the scheduled run. No session stopped since the last run."
        );
    } else {
        let _ = writeln!(s, "Reason: these sessions stopped their turn.");
        for stop in stops {
            let _ = writeln!(
                s,
                "- chain {} (session {}) in {}: {}{} at {}",
                stop.chain_id,
                stop.session_id,
                stop.cwd
                    .as_deref()
                    .map(Path::display)
                    .map(|d| d.to_string())
                    .unwrap_or_default(),
                stop.status,
                stop.waiting_for
                    .as_deref()
                    .map(|w| format!(" for a {w}"))
                    .unwrap_or_default(),
                stop.at.format("%H:%M:%S"),
            );
        }
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "Running Claude Code sessions:");
    for r in running.list() {
        if r.cwd.as_deref().is_some_and(|c| c == config.dir) {
            continue;
        }
        let _ = writeln!(s, "- {}", running_line(&r));
    }
    s
}

fn running_line(r: &RunningSession) -> String {
    format!(
        "session {} in {}: {}{}{}",
        r.session_id,
        r.cwd
            .as_deref()
            .map(Path::display)
            .map(|d| d.to_string())
            .unwrap_or_default(),
        r.status.as_deref().unwrap_or("unknown"),
        r.waiting_for
            .as_deref()
            .map(|w| format!(" for a {w}"))
            .unwrap_or_default(),
        r.status_since
            .map(|t| format!(" since {}", t.format("%Y-%m-%d %H:%M")))
            .unwrap_or_default(),
    )
}

/// The time in the `next-run` file. The first word of the file is a delay
/// after the time of the last change of the file, such as `30m`, `2h` or
/// `90s`. The rest of the line is the reason, for the user.
fn read_next_run(dir: &Path) -> Option<SystemTime> {
    let path = dir.join(NEXT_RUN_FILE);
    let text = std::fs::read_to_string(&path).ok()?;
    let delay = parse_delay(text.split_whitespace().next()?)?;
    let written = std::fs::metadata(&path).ok()?.modified().ok()?;
    let at = written + delay;
    // An old file gives no run.
    (at > SystemTime::now()).then_some(at)
}

/// `90s`, `30m`, `2h` or `1d`.
fn parse_delay(word: &str) -> Option<Duration> {
    let split = word.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = word.split_at(split);
    let n: u64 = n.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, status: &str) -> RunningSession {
        RunningSession {
            session_id: id.to_string(),
            pid: 1,
            cwd: Some(PathBuf::from("/p")),
            status: Some(status.to_string()),
            waiting_for: (status == "waiting").then(|| "permission prompt".to_string()),
            status_since: None,
        }
    }

    fn scan(sessions: &[RunningSession]) -> RunningSessions {
        RunningSessions::from_list(sessions.to_vec())
    }

    #[test]
    fn a_stop_is_busy_to_idle_or_a_change_to_waiting() {
        let old = scan(&[
            session("a", "busy"),
            session("b", "idle"),
            session("c", "busy"),
            session("d", "waiting"),
            session("e", "idle"),
        ]);
        let new = scan(&[
            session("a", "idle"),
            session("b", "idle"),
            session("c", "waiting"),
            session("d", "waiting"),
            session("e", "busy"),
            session("f", "idle"),
        ]);
        let got: Vec<(String, String)> = stops(&old, &new, |s| format!("chain-{s}"))
            .into_iter()
            .map(|s| (s.chain_id, s.status))
            .collect();
        assert_eq!(
            got,
            vec![
                ("chain-a".to_string(), "idle".to_string()),
                ("chain-c".to_string(), "waiting".to_string()),
            ]
        );
    }

    #[test]
    fn the_delay_has_a_unit() {
        assert_eq!(parse_delay("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_delay("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_delay("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_delay("1d"), Some(Duration::from_secs(86400)));
        assert_eq!(parse_delay("30"), None);
        assert_eq!(parse_delay("m"), None);
        assert_eq!(parse_delay("3x"), None);
    }

    #[test]
    fn the_runner_ignores_its_own_sessions() {
        let dir = std::env::temp_dir().join(format!("recap-triage-test-{}", std::process::id()));
        let mut triage = Triage::new(TriageConfig {
            dir: dir.clone(),
            claude: PathBuf::from("/nonexistent/claude"),
            port: 1,
        })
        .unwrap();
        let stop = |cwd: &Path| Stop {
            session_id: "s".to_string(),
            chain_id: "s".to_string(),
            cwd: Some(cwd.to_path_buf()),
            status: "idle".to_string(),
            waiting_for: None,
            at: Local::now(),
        };
        triage.add(vec![stop(&dir)]);
        assert_eq!(triage.status().pending, 0);
        triage.add(vec![stop(Path::new("/elsewhere"))]);
        assert_eq!(triage.status().pending, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
