//! The check runner of the triage items.
//!
//! The triage agent writes the items in `items.json`, and the check scripts
//! in the `checks` directory (see `triage_prompt.md`). The runner runs the
//! check of each open item at an interval that depends on the impact of the
//! item, and writes the outcome to `results.json`. Only the runner writes
//! that file.
//!
//! The runner runs a script only after the user approves it with
//! `recap triage approve`. The approval copies the script to the approved
//! directory, next to the triage directory and outside the files that the
//! triage agent can change. The runner runs the approved copy, so a change
//! of the script after the approval has no effect until the user approves
//! it again.
//!
//! The runner returns a [`Notice`] when an outcome changes, when the event
//! of an item occurs before the item is done, and when the `due` time of a
//! critical or high item comes near. An urgent notice starts a triage run.
//! The runner also sends a macOS notification for a critical item, because
//! the triage run can fail or schedule nothing.

use chrono::{DateTime, Local, TimeDelta};
use recap_api::triage::{
    CHECKS_DIR, CheckRun, CheckSpec, ITEMS_FILE, Impact, Item, ItemResults, Items, Outcome,
    RESULTS_FILE, Results,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The runner stops a check that takes longer than this time.
const CHECK_TIMEOUT: Duration = Duration::from_secs(120);
/// The interval between two evaluations of the items.
const EVAL_INTERVAL: Duration = Duration::from_secs(10);
/// The maximum number of checks in one batch.
const MAX_BATCH: usize = 20;
/// The number of bytes of the output that the runner keeps.
const MAX_OUTPUT: usize = 2000;
/// An item with a `due` time in this period gets the check interval of a
/// critical item.
const DUE_SOON: TimeDelta = TimeDelta::hours(24);

/// A change that the triage agent must know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub item: String,
    pub text: String,
    /// True when the notice starts a triage run. Other notices go into the
    /// next run.
    pub urgent: bool,
}

/// The directory of the approved copies of the check scripts. It is next to
/// the triage directory, so the triage agent cannot write to it.
pub fn approved_dir(triage_dir: &Path) -> PathBuf {
    let name = triage_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "triage".to_string());
    triage_dir.with_file_name(format!("{name}.approved"))
}

/// True for a script name without a path: letters, digits, `.`, `_` and
/// `-`, and not `.` or `..`.
pub fn valid_script_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The SHA-256 of the script `name` when the script and its approved copy
/// are equal. None when the script is not approved in its current version.
fn approved_sha(triage_dir: &Path, name: &str) -> Option<String> {
    if !valid_script_name(name) {
        return None;
    }
    let script = std::fs::read(triage_dir.join(CHECKS_DIR).join(name)).ok()?;
    let copy = std::fs::read(approved_dir(triage_dir).join(name)).ok()?;
    (script == copy).then(|| sha256_hex(&copy))
}

/// The scripts of the `checks` directory that have no approved copy, or
/// that changed after the approval.
pub fn unapproved_scripts(triage_dir: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(triage_dir.join(CHECKS_DIR)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| valid_script_name(n) && approved_sha(triage_dir, n).is_none())
        .collect();
    names.sort();
    names
}

/// Approve the current version of the script `name`: copy it to the
/// approved directory.
pub fn approve(triage_dir: &Path, name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(valid_script_name(name), "not a valid script name: {name}");
    let bytes = std::fs::read(triage_dir.join(CHECKS_DIR).join(name))?;
    let dir = approved_dir(triage_dir);
    std::fs::create_dir_all(&dir)?;
    write_atomic(&dir.join(name), &bytes)?;
    Ok(())
}

/// The approved copy of `name`, if one exists.
pub fn approved_copy(triage_dir: &Path, name: &str) -> Option<Vec<u8>> {
    valid_script_name(name)
        .then(|| std::fs::read(approved_dir(triage_dir).join(name)).ok())
        .flatten()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Which check of an item a job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The check of `done_when`.
    Done,
    /// The check of the `before` event.
    Event,
}

/// One check that the batch thread runs.
#[derive(Debug, Clone)]
struct Job {
    item: String,
    kind: Kind,
    spec: CheckSpec,
    /// The SHA-256 of the approved script. None when the script is not
    /// approved; the job then runs nothing.
    sha256: Option<String>,
}

/// The check runner. The triage runner owns it.
pub struct Checker {
    dir: PathBuf,
    /// The PATH of the scripts.
    path_env: String,
    /// The recap server URL, for the scripts.
    recap_url: String,
    results: Results,
    batch: Option<JoinHandle<Vec<(Job, CheckRun)>>>,
    next_eval: Instant,
    /// The scripts that wait for an approval, at the last evaluation.
    unapproved: Vec<String>,
    /// The unapproved script versions that got a notification.
    notified: BTreeSet<(String, Option<String>)>,
}

impl Checker {
    pub fn new(dir: PathBuf, path_env: String, recap_url: String) -> Checker {
        let results = std::fs::read(dir.join(RESULTS_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Checker {
            dir,
            path_env,
            recap_url,
            results,
            batch: None,
            next_eval: Instant::now(),
            unapproved: Vec::new(),
            notified: BTreeSet::new(),
        }
    }

    /// The scripts that wait for an approval.
    pub fn unapproved(&self) -> &[String] {
        &self.unapproved
    }

    /// Collect a batch that ended, and start a new batch when checks are
    /// due. It returns the notices for the triage agent.
    pub fn tick(&mut self) -> Vec<Notice> {
        let mut notices = Vec::new();
        if self.batch.as_ref().is_some_and(JoinHandle::is_finished) {
            let batch = self.batch.take().expect("the batch exists");
            match batch.join() {
                Ok(done) => {
                    let items = self.read_items();
                    for (job, run) in done {
                        notices.extend(self.record(&items, job, run));
                    }
                    self.save();
                }
                Err(_) => log::error!("checks: the batch thread panicked"),
            }
        }
        if self.batch.is_some() || Instant::now() < self.next_eval {
            return notices;
        }
        self.next_eval = Instant::now() + EVAL_INTERVAL;
        let items = self.read_items();
        let unapproved = unapproved_scripts(&self.dir);
        for name in &unapproved {
            let version = std::fs::read(self.dir.join(CHECKS_DIR).join(name)).ok().map(|b| sha256_hex(&b));
            if self.notified.insert((name.clone(), version)) {
                notify("Triage check", &format!("Script {name} waits for recap triage approve"));
            }
        }
        if unapproved != self.unapproved {
            self.unapproved = unapproved;
            // The status changed. An empty notice tells the caller.
            notices.push(Notice { item: String::new(), text: String::new(), urgent: false });
        }
        let now = Local::now();
        let (alerts, changed) = self.alerts(&items, now);
        notices.extend(alerts);
        if changed {
            self.save();
        }
        let jobs = self.due_jobs(&items, now);
        if !jobs.is_empty() {
            let dir = self.dir.clone();
            let approved = approved_dir(&self.dir);
            let env = [("PATH".to_string(), self.path_env.clone()), ("RECAP_URL".to_string(), self.recap_url.clone())];
            match std::thread::Builder::new()
                .name("triage-checks".to_string())
                .spawn(move || jobs.into_iter().map(|j| {
                    let run = run_job(&dir, &approved, &env, &j);
                    (j, run)
                }).collect())
            {
                Ok(handle) => self.batch = Some(handle),
                Err(e) => log::error!("checks: cannot start the batch thread: {e}"),
            }
        }
        notices
    }

    fn read_items(&self) -> Vec<Item> {
        let path = self.dir.join(ITEMS_FILE);
        let Ok(bytes) = std::fs::read(&path) else {
            return Vec::new();
        };
        match serde_json::from_slice::<Items>(&bytes) {
            Ok(items) => items.items,
            Err(e) => {
                log::warn!("checks: cannot read {}: {e}", path.display());
                Vec::new()
            }
        }
    }

    fn save(&self) {
        let path = self.dir.join(RESULTS_FILE);
        let result = serde_json::to_vec_pretty(&self.results)
            .map_err(std::io::Error::other)
            .and_then(|b| write_atomic(&path, &b));
        if let Err(e) = result {
            log::error!("checks: cannot write {}: {e}", path.display());
        }
    }

    /// The checks that must run now. A check runs again when its script or
    /// its arguments changed, when the approval of its script changed, or
    /// after the interval of the impact of the item.
    fn due_jobs(&self, items: &[Item], now: DateTime<Local>) -> Vec<Job> {
        let mut jobs = Vec::new();
        for item in items {
            if item.state(&self.results).closed() {
                continue;
            }
            let soon = item.due.is_some_and(|d| d - now < DUE_SOON);
            let interval = if soon { Impact::Critical } else { item.impact }.check_interval_secs();
            let results = self.results.get(&item.id);
            let specs = [
                (Kind::Done, item.check.as_ref(), results.and_then(|r| r.check.as_ref())),
                (
                    Kind::Event,
                    item.before.as_ref().and_then(|b| b.check.as_ref()),
                    results.and_then(|r| r.event.as_ref()),
                ),
            ];
            for (kind, spec, last) in specs {
                let Some(spec) = spec else { continue };
                // An event that occurred stays in the past.
                if kind == Kind::Event && item.event_occurred(&self.results) {
                    continue;
                }
                let sha256 = approved_sha(&self.dir, &spec.script);
                let due = match last {
                    None => true,
                    Some(last) => {
                        last.spec != *spec
                            || last.sha256 != sha256.clone().unwrap_or_default()
                            || (now - last.at).num_seconds() >= interval
                    }
                };
                if due {
                    jobs.push(Job { item: item.id.clone(), kind, spec: spec.clone(), sha256 });
                }
            }
            if jobs.len() >= MAX_BATCH {
                break;
            }
        }
        jobs.truncate(MAX_BATCH);
        jobs
    }

    /// Record the run of `job`, and give a notice when the outcome
    /// changed.
    fn record(&mut self, items: &[Item], job: Job, run: CheckRun) -> Vec<Notice> {
        let Some(item) = items.iter().find(|i| i.id == job.item) else {
            return Vec::new();
        };
        let entry = self.results.entry(job.item.clone()).or_default();
        let slot = match job.kind {
            Kind::Done => &mut entry.check,
            Kind::Event => &mut entry.event,
        };
        let before = slot.as_ref().filter(|r| r.spec == run.spec).map(|r| r.outcome);
        let after = run.outcome;
        log::info!(
            "checks: {} {} -> {after:?}",
            job.item,
            run.spec.command_line(),
        );
        *slot = Some(run);
        if before == Some(after) {
            return Vec::new();
        }
        let important = item.impact <= Impact::High;
        let mut notices = Vec::new();
        match job.kind {
            Kind::Done => {
                // The first `not-done` is the expected start state.
                if before.is_none() && after == Outcome::NotDone {
                    return Vec::new();
                }
                notices.push(Notice {
                    item: item.id.clone(),
                    text: format!(
                        "item {} ({:?}): the check changed from {} to {}",
                        item.id,
                        item.impact,
                        before.map(outcome_word).unwrap_or("no result"),
                        outcome_word(after),
                    ),
                    urgent: important,
                });
                if item.impact == Impact::Critical && after != Outcome::Done {
                    notify("Critical item", &format!("{}: check is {}", item.text, outcome_word(after)));
                }
            }
            Kind::Event => {
                if after == Outcome::Done && !item.state(&self.results).closed() {
                    let event = item.before.as_ref().map(|b| b.event.as_str()).unwrap_or_default();
                    notices.push(Notice {
                        item: item.id.clone(),
                        text: format!(
                            "item {} ({:?}) is not done, but its event occurred: {event}",
                            item.id, item.impact
                        ),
                        urgent: item.impact <= Impact::Normal,
                    });
                    if important {
                        notify("Item missed its event", &format!("{}: {event}", item.text));
                    }
                } else if !matches!(after, Outcome::Done | Outcome::NotDone) {
                    notices.push(Notice {
                        item: item.id.clone(),
                        text: format!("item {}: the event check gives {}", item.id, outcome_word(after)),
                        urgent: important,
                    });
                }
            }
        }
        notices
    }

    /// The `due` alerts of the items that are not done. It returns the
    /// notices, and true when it added an alert to the results.
    fn alerts(&mut self, items: &[Item], now: DateTime<Local>) -> (Vec<Notice>, bool) {
        let mut notices = Vec::new();
        let mut changed = false;
        for item in items {
            let (Some(due), Some(warning)) = (item.due, item.impact.due_warning_secs()) else {
                continue;
            };
            if item.state(&self.results).closed() {
                continue;
            }
            let stamp = due.to_rfc3339();
            let kind = if now >= due {
                "due-passed"
            } else if (due - now).num_seconds() <= warning {
                "due-soon"
            } else {
                continue;
            };
            let key = format!("{kind}:{stamp}");
            let alerts = &mut self.results.entry(item.id.clone()).or_insert_with(ItemResults::default).alerts;
            if alerts.contains_key(&key) {
                continue;
            }
            alerts.insert(key, now);
            changed = true;
            let when = due.format("%Y-%m-%d %H:%M");
            let text = if kind == "due-passed" {
                format!("item {} ({:?}) is not done, and its due time {when} passed", item.id, item.impact)
            } else {
                format!("item {} ({:?}) is not done, and it is due at {when}", item.id, item.impact)
            };
            if item.impact == Impact::Critical {
                notify("Critical item due", &format!("{} at {when}", item.text));
            }
            notices.push(Notice { item: item.id.clone(), text, urgent: true });
        }
        (notices, changed)
    }
}

fn outcome_word(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Done => "done",
        Outcome::NotDone => "not-done",
        Outcome::Unknown => "unknown",
        Outcome::Unapproved => "unapproved",
        Outcome::Missing => "missing",
    }
}

/// Run one check. The function waits for the script to end.
fn run_job(dir: &Path, approved: &Path, env: &[(String, String)], job: &Job) -> CheckRun {
    let result = |outcome, exit, output: String| CheckRun {
        spec: job.spec.clone(),
        sha256: job.sha256.clone().unwrap_or_default(),
        outcome,
        at: Local::now(),
        exit,
        output: tail(&output),
    };
    if !valid_script_name(&job.spec.script) || !dir.join(CHECKS_DIR).join(&job.spec.script).is_file() {
        return result(Outcome::Missing, None, format!("no script {CHECKS_DIR}/{}", job.spec.script));
    }
    if job.sha256.is_none() {
        return result(
            Outcome::Unapproved,
            None,
            format!("the user must approve {CHECKS_DIR}/{} (recap triage approve)", job.spec.script),
        );
    }
    let script = approved.join(&job.spec.script);
    match execute(dir, &script, env, &job.spec.args) {
        Ok((code, output)) => {
            let outcome = match code {
                Some(0) => Outcome::Done,
                Some(1) => Outcome::NotDone,
                _ => Outcome::Unknown,
            };
            result(outcome, code, output)
        }
        Err(e) => result(Outcome::Unknown, None, e.to_string()),
    }
}

/// Run `script` with `args` in `dir`. It returns the exit code and the
/// output. The output goes to a temporary file, so a large output cannot
/// block the script.
fn execute(
    dir: &Path,
    script: &Path,
    env: &[(String, String)],
    args: &[String],
) -> anyhow::Result<(Option<i32>, String)> {
    let first_line = std::fs::read_to_string(script)?
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    // A script file has no execute permission when the agent writes it, so
    // the runner starts its interpreter.
    let mut command = match first_line.strip_prefix("#!") {
        Some(shebang) => {
            let mut words = shebang.split_whitespace();
            let program = words.next().unwrap_or("/bin/sh");
            let mut c = Command::new(program);
            c.args(words);
            c
        }
        None => Command::new("/bin/sh"),
    };
    let out_path = std::env::temp_dir().join(format!(
        "recap-check-{}-{}.out",
        std::process::id(),
        script.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    let out = std::fs::File::create(&out_path)?;
    let mut child = command
        .arg(script)
        .args(args)
        .current_dir(dir)
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .spawn()?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() > CHECK_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut output = String::new();
    let mut bytes = Vec::new();
    if std::fs::File::open(&out_path).and_then(|mut f| f.read_to_end(&mut bytes)).is_ok() {
        output = String::from_utf8_lossy(&bytes).into_owned();
    }
    let _ = std::fs::remove_file(&out_path);
    match status {
        Some(status) => Ok((status.code(), output)),
        None => Ok((None, format!("{output}\nthe runner stopped the check after {} s", CHECK_TIMEOUT.as_secs()))),
    }
}

/// The last [`MAX_OUTPUT`] bytes of `text`, at a character boundary.
fn tail(text: &str) -> String {
    let text = text.trim_end();
    if text.len() <= MAX_OUTPUT {
        return text.to_string();
    }
    let mut start = text.len() - MAX_OUTPUT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

/// Send a macOS notification through Hammerspoon. The function does not
/// wait, because `hs` can take more than a minute.
pub fn notify(title: &str, text: &str) {
    let clean = |s: &str| -> String {
        s.chars()
            .filter(|c| !matches!(c, '"' | '\'' | '\\' | '\n'))
            .take(80)
            .collect()
    };
    let lua = format!("recapNotify(\"{}\", \"{}\")", clean(title), clean(text));
    log::info!("checks: notify {title}: {text}");
    match Command::new("hs")
        .args(["-t", "5", "-c", &lua])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // Wait in a thread, so the process does not stay a zombie.
            let _ = std::thread::Builder::new()
                .name("triage-notify".to_string())
                .spawn(move || child.wait());
        }
        Err(e) => log::warn!("checks: cannot run hs: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use recap_api::triage::{Event, Owner, Status};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("recap-checks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(approved_dir(&dir));
        std::fs::create_dir_all(dir.join(CHECKS_DIR)).unwrap();
        dir
    }

    fn item(id: &str, script: &str, impact: Impact) -> Item {
        Item {
            id: id.to_string(),
            chain: None,
            owner: Owner::Agent,
            text: "Fix it".to_string(),
            impact,
            why: String::new(),
            done_when: String::new(),
            check: Some(CheckSpec { script: script.to_string(), args: vec!["x".to_string()] }),
            due: None,
            before: None,
            status: Status::Open,
            note: String::new(),
            created: None,
        }
    }

    fn write_items(dir: &Path, items: Vec<Item>) {
        std::fs::write(dir.join(ITEMS_FILE), serde_json::to_vec(&Items { items }).unwrap()).unwrap();
    }

    /// Tick until the batch ends and the results contain `id`.
    fn settle(checker: &mut Checker) -> Vec<Notice> {
        let mut all = Vec::new();
        for _ in 0..200 {
            checker.next_eval = Instant::now();
            all.extend(checker.tick());
            if checker.batch.is_none() && !checker.due_jobs(&checker.read_items(), Local::now()).iter().any(|j| j.sha256.is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        all
    }

    #[test]
    fn the_runner_runs_only_the_approved_version() {
        let dir = temp_dir("approve");
        std::fs::write(dir.join("checks/ok"), "#!/bin/sh\necho \"arg $1\"\nexit 0\n").unwrap();
        write_items(&dir, vec![item("a", "ok", Impact::High)]);
        let mut checker = Checker::new(dir.clone(), std::env::var("PATH").unwrap(), String::new());
        settle(&mut checker);
        assert_eq!(checker.results["a"].check.as_ref().unwrap().outcome, Outcome::Unapproved);
        assert_eq!(checker.unapproved(), ["ok".to_string()]);

        approve(&dir, "ok").unwrap();
        let notices = settle(&mut checker);
        let run = checker.results["a"].check.clone().unwrap();
        assert_eq!(run.outcome, Outcome::Done);
        assert_eq!(run.output, "arg x");
        assert!(notices.iter().any(|n| n.item == "a" && n.urgent));
        assert!(checker.unapproved().is_empty());

        // A change after the approval stops the runs of the script.
        std::fs::write(dir.join("checks/ok"), "#!/bin/sh\nexit 1\n").unwrap();
        assert!(approved_sha(&dir, "ok").is_none());
        let saved: Results = serde_json::from_slice(&std::fs::read(dir.join(RESULTS_FILE)).unwrap()).unwrap();
        assert_eq!(saved["a"].check.as_ref().unwrap().outcome, Outcome::Done);
    }

    #[test]
    fn an_event_before_the_item_is_urgent() {
        let dir = temp_dir("event");
        std::fs::write(dir.join("checks/no"), "exit 1\n").unwrap();
        std::fs::write(dir.join("checks/yes"), "exit 0\n").unwrap();
        approve(&dir, "no").unwrap();
        approve(&dir, "yes").unwrap();
        let mut it = item("b", "no", Impact::Normal);
        it.before = Some(Event {
            event: "prod deploy".to_string(),
            check: Some(CheckSpec { script: "yes".to_string(), args: Vec::new() }),
        });
        write_items(&dir, vec![it]);
        let mut checker = Checker::new(dir.clone(), std::env::var("PATH").unwrap(), String::new());
        let notices = settle(&mut checker);
        assert!(notices.iter().any(|n| n.urgent && n.text.contains("prod deploy")), "{notices:?}");
        // The first `not-done` of the item check gives no notice.
        assert!(!notices.iter().any(|n| n.text.contains("changed")), "{notices:?}");
    }

    #[test]
    fn a_due_time_gives_one_alert() {
        let dir = temp_dir("due");
        let mut it = item("c", "none", Impact::Critical);
        it.check = None;
        it.due = Some(Local::now() + TimeDelta::hours(1));
        write_items(&dir, vec![it]);
        let mut checker = Checker::new(dir.clone(), String::new(), String::new());
        let items = checker.read_items();
        let (first, changed) = checker.alerts(&items, Local::now());
        assert_eq!(first.len(), 1);
        assert!(changed && first[0].urgent);
        let (second, _) = checker.alerts(&items, Local::now());
        assert!(second.is_empty());
        let (passed, _) = checker.alerts(&items, Local::now() + TimeDelta::hours(2));
        assert_eq!(passed.len(), 1);
        assert!(passed[0].text.contains("passed"));
    }

    #[test]
    fn a_script_name_has_no_path() {
        assert!(valid_script_name("pr-merged"));
        assert!(valid_script_name("commit_in.sh"));
        assert!(!valid_script_name("../x"));
        assert!(!valid_script_name("a/b"));
        assert!(!valid_script_name(".hidden"));
        assert!(!valid_script_name(""));
    }
}
