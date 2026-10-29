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
//!
//! A review run reads all the agents of the last 24 hours. The runner starts
//! one when the triage directory has no agent file, and when a client asks
//! for one (`POST /v1/triage/review`).

use crate::session::RunningSessions;
use chrono::{DateTime, Local};
use recap_api::{RunningSession, TriageRun, TriageStatus};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// The instructions of the triage agent. The runner gives them to `claude`
/// with `--append-system-prompt`.
const INSTRUCTIONS: &str = include_str!("triage_prompt.md");

/// The name of the file in the triage directory that holds the time of the
/// next scheduled run.
const NEXT_RUN_FILE: &str = "next-run";

/// The runner stops a run that takes longer than this time.
const MAX_RUN: Duration = Duration::from_secs(15 * 60);
/// The runner stops a review run that takes longer than this time. A review
/// reads many agents.
const MAX_REVIEW_RUN: Duration = Duration::from_secs(40 * 60);

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
    info: TriageRun,
    /// The newest action of the run. The thread that reads stdout writes it.
    activity: Arc<Mutex<Option<String>>>,
}

impl Run {
    /// `info` with the newest action.
    fn info(&self) -> TriageRun {
        TriageRun {
            activity: self.activity.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            ..self.info.clone()
        }
    }
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
    /// True when the next run is a review of the agents of the last 24 hours.
    review: bool,
    run: Option<Run>,
    /// The last run that ended.
    last: Option<TriageRun>,
    /// The time that the `next-run` file gave at the end of the last run.
    next_run: Option<SystemTime>,
}

impl Triage {
    /// A runner for `config`. It makes the triage directory if it does not
    /// exist. A directory without agent files gets a review run.
    pub fn new(config: TriageConfig) -> anyhow::Result<Triage> {
        let agents = config.dir.join("agents");
        std::fs::create_dir_all(&agents)?;
        let dir = config.dir.canonicalize()?;
        let next_run = read_next_run(&dir);
        let review = !std::fs::read_dir(&agents)?
            .filter_map(Result::ok)
            .any(|e| e.path().extension().is_some_and(|x| x == "md"));
        Ok(Triage {
            config: TriageConfig { dir, ..config },
            pending: Vec::new(),
            review,
            run: None,
            last: None,
            next_run,
        })
    }

    pub fn status(&self) -> TriageStatus {
        TriageStatus {
            dir: self.config.dir.clone(),
            pending: self.pending.len(),
            review: self.review,
            next_run: self.next_run.map(DateTime::<Local>::from),
            run: self.run.as_ref().map(Run::info).or_else(|| self.last.clone()),
        }
    }

    /// Make the next run a review of the agents of the last 24 hours.
    pub fn request_review(&mut self) {
        log::info!("triage: a client asked for a review");
        self.review = true;
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
    /// is the last scan of the running sessions. `recent_dirs` gives the
    /// project directories of the agents of the last 24 hours. It returns
    /// true when the state changed.
    pub fn tick(
        &mut self,
        running: &RunningSessions,
        recent_dirs: impl FnOnce() -> Vec<PathBuf>,
    ) -> bool {
        if let Some(run) = self.run.as_mut() {
            let limit = if run.info.review { MAX_REVIEW_RUN } else { MAX_RUN };
            let success = match run.child.try_wait() {
                Ok(None) if run.started.elapsed() < limit => {
                    // A new action changes the status.
                    let activity = run.info().activity;
                    let changed = activity != run.info.activity;
                    run.info.activity = activity;
                    return changed;
                }
                Ok(None) => {
                    log::warn!("triage run: stopped after {}", crate::logger::ms(limit));
                    false
                }
                Ok(Some(status)) => {
                    log::info!(
                        "triage run: ended with {status} after {}",
                        crate::logger::ms(run.started.elapsed())
                    );
                    status.success()
                }
                Err(e) => {
                    log::warn!("triage run: {e}");
                    false
                }
            };
            let mut info = run.info();
            info.ended = Some(Local::now());
            info.success = Some(success);
            self.last = Some(info);
            self.run = None;
            self.next_run = read_next_run(&self.config.dir);
            return true;
        }
        let scheduled = self.next_run.is_some_and(|t| SystemTime::now() >= t);
        if self.pending.is_empty() && !scheduled && !self.review {
            return false;
        }
        let stops = std::mem::take(&mut self.pending);
        let review = std::mem::take(&mut self.review);
        // A scheduled run uses its time. Without a new `next-run` file, the
        // next run waits for a stop.
        self.next_run = None;
        let dirs = repo_dirs(&self.config, &stops, running, recent_dirs());
        let prompt = run_prompt(&self.config, &stops, review, running, &dirs);
        let info = |session_id: String| TriageRun {
            session_id,
            started: Local::now(),
            stops: stops.len(),
            review,
            ended: None,
            success: None,
            activity: None,
        };
        match new_session_id()
            .map_err(anyhow::Error::from)
            .and_then(|id| self.start(&prompt, &dirs, info(id)))
        {
            Ok(run) => {
                log::info!(
                    "triage run: started session {} for {} stops{}",
                    short(&run.info.session_id),
                    stops.len(),
                    if review { " and a review" } else { "" }
                );
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

    fn start(&self, prompt: &str, repo_dirs: &BTreeSet<PathBuf>, info: TriageRun) -> anyhow::Result<Run> {
        let mut child = Command::new(&self.config.claude)
            .current_dir(&self.config.dir)
            .args([
                "-p",
                "--model",
                "opus",
                "--append-system-prompt",
                INSTRUCTIONS,
            ])
            // The sandbox of Claude Code refuses the connections to the
            // recap server. The allowed tools limit the run instead.
            .args(["--settings", r#"{"sandbox":{"enabled":false}}"#])
            // The TUI finds the transcript of the run by this id.
            .args(["--session-id", &info.session_id])
            // One JSON event for each message and tool call, so the runner
            // can show the newest action. `-p` needs `--verbose` for it.
            .args(["--output-format", "stream-json", "--verbose"])
            .arg("--allowedTools")
            .arg(allowed_tools(self.config.port, repo_dirs))
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
        let activity = Arc::new(Mutex::new(None));
        if let Some(stdout) = child.stdout.take() {
            let activity = Arc::clone(&activity);
            std::thread::Builder::new()
                .name("triage-stdout".to_string())
                .spawn(move || {
                    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                        for action in stream_actions(&line) {
                            log::info!("triage: {action}");
                            *activity.lock().unwrap_or_else(|e| e.into_inner()) = Some(action);
                        }
                    }
                })?;
        }
        if let Some(stderr) = child.stderr.take() {
            std::thread::Builder::new()
                .name("triage-stderr".to_string())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        log::info!("triage stderr: {line}");
                    }
                })?;
        }
        Ok(Run {
            child,
            started: Instant::now(),
            info,
            activity,
        })
    }
}

fn short(id: &str) -> &str {
    &id[..8.min(id.len())]
}

/// The directories of the stopped and the running sessions and the
/// `recent` project directories, except the triage directory. A triage run
/// can read the repository state of these directories.
fn repo_dirs(
    config: &TriageConfig,
    stops: &[Stop],
    running: &RunningSessions,
    recent: Vec<PathBuf>,
) -> BTreeSet<PathBuf> {
    stops
        .iter()
        .filter_map(|s| s.cwd.clone())
        .chain(running.list().into_iter().filter_map(|r| r.cwd))
        .chain(recent)
        .filter(|d| *d != config.dir)
        .collect()
}

/// The actions in one line of `--output-format stream-json`, each on one
/// line: the tool calls and the texts of the agent, and the final result.
fn stream_actions(line: &str) -> Vec<String> {
    let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let one_line = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    match event["type"].as_str() {
        Some("assistant") => event["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|part| match part["type"].as_str() {
                Some("text") => part["text"].as_str().map(one_line),
                Some("tool_use") => {
                    let input = &part["input"];
                    let detail = ["description", "file_path", "pattern", "command"]
                        .iter()
                        .find_map(|k| input[k].as_str())
                        .map(one_line)
                        .unwrap_or_default();
                    Some(format!("{} {detail}", part["name"].as_str().unwrap_or("tool")))
                }
                _ => None,
            })
            .filter(|s| !s.trim().is_empty())
            .collect(),
        Some("result") => vec![format!(
            "result: {}",
            one_line(event["result"].as_str().unwrap_or_default())
        )],
        _ => Vec::new(),
    }
}

/// A random version 4 UUID, for the session id of a run.
fn new_session_id() -> std::io::Result<String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    ))
}

/// The tools that a triage run can use without a permission prompt.
/// `claude -p` refuses all other tools.
///
/// Each repository rule names one exact directory. A `*` in the place of
/// the directory also approves options such as `-c`, which can run any
/// command.
fn allowed_tools(port: u16, repo_dirs: &BTreeSet<PathBuf>) -> String {
    let repo_rules = repo_dirs.iter().flat_map(|dir| {
        ["log", "status", "diff", "show"]
            .map(|cmd| format!("Bash(git -C {} {cmd} *)", dir.display()))
    });
    [
        "Read".to_string(),
        "Glob".to_string(),
        "Grep".to_string(),
        // An Edit rule also covers the Write tool.
        "Edit(./**)".to_string(),
        "Agent".to_string(),
        "Bash(recap list:*)".to_string(),
        format!("Bash(curl -s http://127.0.0.1:{port}/v1/*)"),
        "Bash(jq:*)".to_string(),
        "Bash(hs -t 5 -c:*)".to_string(),
    ]
    .into_iter()
    .chain(repo_rules)
    .collect::<Vec<_>>()
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
    dirs.push("/Applications/Hammerspoon.app/Contents/Frameworks/hs".to_string());
    if let Ok(path) = std::env::var("PATH") {
        dirs.push(path);
    }
    dirs.join(":")
}

/// The user message of a run: the time, the reasons, the running sessions
/// and the directories that the run can read with git.
fn run_prompt(
    config: &TriageConfig,
    stops: &[Stop],
    review: bool,
    running: &RunningSessions,
    repo_dirs: &BTreeSet<PathBuf>,
) -> String {
    let mut s = String::new();
    let now = Local::now();
    let _ = writeln!(s, "Time: {}", now.format("%Y-%m-%d %H:%M %:z (%A)"));
    let _ = writeln!(s, "recap server: http://127.0.0.1:{}", config.port);
    let _ = writeln!(s, "Triage directory: {}", config.dir.display());
    let _ = writeln!(s);
    if review {
        let _ = writeln!(
            s,
            "Reason: a review of all the agents of the last 24 hours (see \"Review runs\")."
        );
    }
    if stops.is_empty() && !review {
        let _ = writeln!(
            s,
            "Reason: the scheduled run. No session stopped since the last run."
        );
    } else if !stops.is_empty() {
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
    let _ = writeln!(s);
    let _ = writeln!(s, "Directories that you can read with git:");
    for dir in repo_dirs {
        let _ = writeln!(s, "- {}", dir.display());
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

    #[test]
    fn a_directory_without_agent_files_gets_a_review() {
        let dir = std::env::temp_dir().join(format!("recap-triage-review-{}", std::process::id()));
        let config = TriageConfig {
            dir: dir.clone(),
            claude: PathBuf::from("/nonexistent/claude"),
            port: 1,
        };
        assert!(Triage::new(config.clone()).unwrap().status().review);
        std::fs::write(dir.join("agents/a.md"), "").unwrap();
        assert!(!Triage::new(config).unwrap().status().review);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_stream_gives_the_tool_calls_and_the_result() {
        let assistant = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"I read\nthe memory."},
            {"type":"tool_use","name":"Read","input":{"file_path":"/t/memory.md"}},
            {"type":"tool_use","name":"Bash","input":{"command":"curl -s x","description":"List the agents"}}]}}"#
            .replace('\n', "");
        assert_eq!(
            stream_actions(&assistant),
            ["I read the memory.", "Read /t/memory.md", "Bash List the agents"]
        );
        assert_eq!(
            stream_actions(r#"{"type":"result","result":"Done."}"#),
            ["result: Done."]
        );
        assert!(stream_actions(r#"{"type":"system"}"#).is_empty());
        assert!(stream_actions("not json").is_empty());
    }

    #[test]
    fn a_session_id_is_a_version_4_uuid() {
        let id = new_session_id().unwrap();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('4'));
        assert!(matches!(&parts[3][..1], "8" | "9" | "a" | "b"));
    }
}
