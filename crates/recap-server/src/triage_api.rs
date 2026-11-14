//! The triage functions of the server for the HTTP API: the state view, the
//! changes of the data, the approval of the check scripts, and the chat of
//! the user with triagent.
//!
//! A chat thread is one Claude Code session. The first message starts the
//! session, and each later message resumes it. The server starts
//! `claude -p` for each message, with the same tools as a triage run, and
//! sends the text of the reply to the clients as [`Event::TriageChat`]
//! events while triagent writes it.

use crate::checks;
use crate::server::Server;
use crate::triage::{self, TriageConfig};
use crate::triage_store::TriageStore;
use chrono::Local;
use recap_api::triage::{
    Model,
    Actor, Batch, BatchResult, CHECKS_DIR, ChatMessage, ChatThread, ItemView, Op, QUOTE_MAX, SessionNote, SessionView, TriageState,
    TurnRef,
};
use recap_api::{ApiError, ApiResult, EditSource, Event};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The server stops a chat reply that takes longer than this time.
const MAX_REPLY: Duration = Duration::from_secs(20 * 60);
/// The shortest time between two events of one reply.
const EVENT_INTERVAL: Duration = Duration::from_millis(80);

/// What the API calls need from the triage runner.
pub struct TriageHandle {
    pub store: Arc<TriageStore>,
    pub config: TriageConfig,
    /// The threads that wait for a reply of triagent.
    busy: Mutex<HashSet<i64>>,
    /// The GitHub page of the `origin` remote of each project directory.
    /// The server reads a remote one time.
    repos: Mutex<HashMap<PathBuf, Option<String>>>,
}

impl TriageHandle {
    pub fn new(store: Arc<TriageStore>, config: TriageConfig) -> TriageHandle {
        TriageHandle { store, config, busy: Mutex::new(HashSet::new()), repos: Mutex::new(HashMap::new()) }
    }

    /// The GitHub page of the `origin` remote of the repository at `dir`.
    fn repo(&self, dir: &Path) -> Option<String> {
        let mut repos = self.repos.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(known) = repos.get(dir) {
            return known.clone();
        }
        let output = Command::new("git").arg("-C").arg(dir).args(["remote", "get-url", "origin"]).output();
        let repo = output
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| github_page(String::from_utf8_lossy(&o.stdout).trim()));
        repos.insert(dir.to_path_buf(), repo.clone());
        repo
    }
}

/// The GitHub page of a git remote: `https://github.com/<owner>/<name>`.
/// None for a remote of another host.
fn github_page(remote: &str) -> Option<String> {
    let path = ["git@github.com:", "https://github.com/", "ssh://git@github.com/"]
        .iter()
        .find_map(|prefix| remote.strip_prefix(prefix))?;
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');
    let valid = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    let (owner, name) = path.split_once('/')?;
    (valid(owner) && valid(name)).then(|| format!("https://github.com/{owner}/{name}"))
}

/// The filter of `GET /v1/triage/state`.
#[derive(Debug, Default, Deserialize)]
pub struct StateQuery {
    /// Only the items and the session of this chain.
    pub chain: Option<String>,
    /// Only the items and the sessions of this topic.
    pub topic: Option<String>,
    /// `1`: only the items that are not closed.
    pub open: Option<String>,
}

/// One check script, for the approval view.
#[derive(Debug, Serialize)]
pub struct CheckScript {
    pub name: String,
    /// True when the approved copy is equal to the script.
    pub approved: bool,
    pub text: String,
    /// The approved copy, when it differs from the script.
    pub approved_text: Option<String>,
    /// The ids of the items that use the script.
    pub items: Vec<String>,
}

fn no_triage() -> ApiError {
    ApiError("the server runs without --triage-dir".to_string())
}

fn error(e: impl std::fmt::Display) -> ApiError {
    ApiError(e.to_string())
}

impl Server {
    fn handle(&self) -> ApiResult<&Arc<TriageHandle>> {
        self.triage_handle().ok_or_else(no_triage)
    }

    /// The triage data with the values that the server calculates: the
    /// state of each item, the priority order, and the current data of each
    /// session.
    pub fn triage_state(&self, query: &StateQuery) -> ApiResult<TriageState> {
        let handle = self.handle()?;
        let store = &handle.store;
        let now = Local::now();
        let version = store.version();
        let results = store.results();
        let notes: BTreeMap<String, SessionNote> =
            store.sessions().into_iter().map(|n| (n.chain.clone(), n)).collect();
        let (agents, running) = self.chain_data();

        let topic_of = |item: &recap_api::triage::Item| -> Option<String> {
            match &item.chain {
                Some(chain) => notes.get(chain).and_then(|n| n.topic.clone()),
                None => item.topic.clone(),
            }
        };
        let mut items: Vec<_> = store
            .items()
            .into_iter()
            .filter(|i| query.chain.as_ref().is_none_or(|c| i.chain.as_ref() == Some(c)))
            .filter(|i| query.topic.as_ref().is_none_or(|t| topic_of(i).as_ref() == Some(t)))
            .map(|item| {
                let state = item.state(&results);
                let waiting = item
                    .chain
                    .as_ref()
                    .and_then(|c| running.get(c))
                    .is_some_and(|r| r.status.as_deref() == Some("waiting"));
                let key = (state.closed(), item.priority(&results, waiting, now));
                (key, state, item)
            })
            .filter(|(_, state, _)| query.open.as_deref() != Some("1") || !state.closed())
            .collect();
        items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.id.cmp(&b.2.id)));
        let items: Vec<ItemView> = items
            .into_iter()
            .map(|(_, state, item)| ItemView {
                state,
                late: !state.closed() && item.late(&results, now),
                in_topic: topic_of(&item),
                check_run: item.check_run(&results).cloned(),
                event_run: item.event_run(&results).cloned(),
                item,
            })
            .collect();

        // A session is in the view when it has a note or an item.
        let mut chains: BTreeSet<String> = items.iter().filter_map(|i| i.item.chain.clone()).collect();
        chains.extend(
            notes
                .values()
                .filter(|n| query.chain.as_ref().is_none_or(|c| &n.chain == c))
                .filter(|n| query.topic.as_ref().is_none_or(|t| n.topic.as_ref() == Some(t)))
                .map(|n| n.chain.clone()),
        );
        let sessions = chains
            .into_iter()
            .map(|chain| {
                let note = notes.get(&chain).cloned().unwrap_or(SessionNote {
                    chain: chain.clone(),
                    topic: None,
                    turn: None,
                    summary: String::new(),
                    muted_turn: None,
                });
                let agent = agents.get(&chain);
                let run = running.get(&chain);
                let last_turn = agent.and_then(|a| a.last_ordinal);
                SessionView {
                    muted: note.muted_turn.is_some() && note.muted_turn == last_turn,
                    title: agent.and_then(|a| a.title.clone()).or_else(|| {
                        agent.map(|a| a.first_prompt.clone())
                    }),
                    repo: agent.and_then(|a| a.project_roots.first()).and_then(|dir| handle.repo(dir)),
                    session_id: agent.map(|a| a.session_id.clone()),
                    last_ts: agent.map(|a| a.last_ts),
                    last_turn,
                    status: run.and_then(|r| r.status.clone()),
                    waiting_for: run.and_then(|r| r.waiting_for.clone()),
                    note,
                }
            })
            .collect();
        Ok(TriageState { version, last_change: store.last_change(), topics: store.topics(), sessions, items })
    }

    /// The error of the first reference in `set` whose `quote` is not an
    /// excerpt of its turn. The triage store cannot test this, because the
    /// turns are in the history of the server.
    fn quote_error(&self, set: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
        // The triage store reports a `refs` value that is not valid.
        let refs: Vec<TurnRef> = serde_json::from_value(set.get("refs")?.clone()).ok()?;
        refs.iter().filter(|r| !r.quote.is_empty()).find_map(|r| {
            let place = format!("turn {} of chain {}", r.turn, r.chain);
            if r.quote.chars().count() > QUOTE_MAX {
                return Some(format!("the quote for {place} has more than {QUOTE_MAX} characters"));
            }
            let Some((prompt, response)) = self.turn_texts(&r.chain, r.turn) else {
                return Some(format!("the history has no {place}"));
            };
            match quote_count(&r.quote, &prompt) + response.map_or(0, |text| quote_count(&r.quote, &text)) {
                1 => None,
                0 => Some(format!(
                    "the quote `{}` is not in the prompt or the response of {place}: copy the exact text, or leave `quote` out",
                    r.quote
                )),
                n => Some(format!("the quote `{}` occurs {n} times in {place}: use a longer excerpt", r.quote)),
            }
        })
    }

    /// Do the operations of `batch` for `actor`. An item operation with a
    /// reference whose quote is not in its turn changes nothing.
    pub fn triage_apply(&self, actor: Actor, batch: &Batch) -> ApiResult<BatchResult> {
        let handle = self.handle()?;
        let refused: Vec<Option<String>> = batch
            .ops
            .iter()
            .map(|op| match op {
                Op::Item { set, .. } => self.quote_error(set),
                _ => None,
            })
            .collect();
        let accepted = Batch { ops: batch.ops.iter().zip(&refused).filter(|(_, e)| e.is_none()).map(|(op, _)| op.clone()).collect() };
        let done = handle.store.apply(actor, &accepted);
        // The errors of the store, at the places of their operations.
        let mut errors = done.errors.into_iter();
        let result = BatchResult {
            version: done.version,
            errors: refused.into_iter().map(|e| e.or_else(|| errors.next().flatten())).collect(),
        };
        for (op, e) in batch.ops.iter().zip(&result.errors) {
            if let Some(e) = e {
                log::warn!("triage apply ({actor:?}): refused {}: {e}", serde_json::to_string(op).unwrap_or_default());
            }
        }
        Ok(result)
    }

    /// The changes of the triage data after the sequence number `after`.
    pub fn triage_changes(&self, after: u64, to: Option<u64>, actor: Option<Actor>) -> ApiResult<Vec<recap_api::triage::Change>> {
        let store = &self.handle()?.store;
        Ok(match (actor, to) {
            (Some(actor), Some(to)) => store.changes_between(actor, after, to),
            _ => store.changes_after(after, 500),
        })
    }

    /// All the check scripts, with the text of each script.
    pub fn triage_checks(&self) -> ApiResult<Vec<CheckScript>> {
        let handle = self.handle()?;
        let dir = &handle.config.dir;
        let unapproved = checks::unapproved_scripts(dir);
        let items = handle.store.items();
        let mut names: Vec<String> = std::fs::read_dir(dir.join(CHECKS_DIR))
            .map(|read| {
                read.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                    .filter_map(|e| e.file_name().to_str().map(str::to_string))
                    .filter(|n| checks::valid_script_name(n))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| {
                let approved = !unapproved.contains(&name);
                let text = std::fs::read_to_string(dir.join(CHECKS_DIR).join(&name)).unwrap_or_default();
                let approved_text = (!approved)
                    .then(|| checks::approved_copy(dir, &name))
                    .flatten()
                    .map(|b| String::from_utf8_lossy(&b).into_owned());
                let uses = |spec: Option<&recap_api::triage::CheckSpec>| spec.is_some_and(|s| s.script == name);
                let items = items
                    .iter()
                    .filter(|i| uses(i.check.as_ref()) || uses(i.before.as_ref().and_then(|b| b.check.as_ref())))
                    .map(|i| i.id.clone())
                    .collect();
                CheckScript { name, approved, text, approved_text, items }
            })
            .collect())
    }

    /// Approve the current version of the check script `name`.
    pub fn triage_approve(&self, name: &str) -> ApiResult<()> {
        let handle = self.handle()?;
        checks::approve(&handle.config.dir, name).map_err(error)?;
        log::info!("triage: the user approved the check script {name}");
        Ok(())
    }

    /// Show the terminal window of the newest session of the chain `chain`.
    /// A session that does not run gets a new window that resumes it.
    pub fn focus_chain(&self, chain: &str) -> ApiResult<()> {
        let (agents, running) = self.chain_data();
        let agent = agents.get(chain).ok_or_else(|| error(format!("no session chain {chain}")))?;
        if agent.source != EditSource::ClaudeCode {
            return Err(error("only a Claude Code session has a terminal window"));
        }
        let run = running.get(chain);
        let cwd = run.and_then(|r| r.cwd.clone()).or_else(|| agent.project_roots.first().cloned());
        let session = run.map(|r| r.session_id.as_str()).unwrap_or(&agent.session_id);
        crate::focus::focus_or_resume(session, run.map(|r| r.pid), cwd.as_deref());
        Ok(())
    }

    // ── The chat ────────────────────────────────────────────────────────────

    pub fn chat_threads(&self) -> ApiResult<Vec<ChatThread>> {
        Ok(self.handle()?.store.threads())
    }

    pub fn chat_thread(&self, id: i64) -> ApiResult<ChatThread> {
        self.handle()?.store.thread(id).ok_or_else(|| error(format!("no chat thread {id}")))
    }

    /// Send `text` to triagent in the thread `thread`, or in a new
    /// thread. It returns the thread with the new message and an empty
    /// reply. The reply comes as [`Event::TriageChat`] events. `model`
    /// writes the reply.
    pub fn chat_send(&self, thread: Option<i64>, text: &str, model: Model) -> ApiResult<ChatThread> {
        let handle = Arc::clone(self.handle()?);
        let text = text.trim();
        if text.is_empty() {
            return Err(error("the message is empty"));
        }
        let store = &handle.store;
        let (thread, first) = match thread {
            Some(id) => (self.chat_thread(id)?, false),
            None => {
                let title: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect();
                let session = triage::new_session_id().map_err(error)?;
                (store.new_thread(&title, &session).map_err(error)?, true)
            }
        };
        if !handle.busy.lock().unwrap_or_else(|e| e.into_inner()).insert(thread.id) {
            return Err(error("triagent still writes a reply in this thread"));
        }
        let started = (|| -> anyhow::Result<()> {
            store.add_message(thread.id, "user", text, true)?;
            let reply = store.add_message(thread.id, "agent", "", false)?;
            let prompt = chat_prompt(self, &handle, thread.id, text);
            let (_, running) = self.chain_data();
            let mut dirs: BTreeSet<PathBuf> = running.values().filter_map(|r| r.cwd.clone()).collect();
            dirs.extend(self.recent_project_dirs());
            dirs.remove(&handle.config.dir);
            let server = self.clone();
            let session = thread.session_id.clone();
            let handle = Arc::clone(&handle);
            std::thread::Builder::new().name("triage-chat".to_string()).spawn(move || {
                let mut reply = reply;
                if let Err(e) = run_reply(&server, &handle, &session, first, model, &prompt, &dirs, &mut reply) {
                    log::error!("triage chat: {e}");
                    let _ = write!(reply.text, "\n\n(The reply failed: {e})");
                }
                reply.done = true;
                reply.activity = None;
                if let Err(e) = handle.store.set_message(&mut reply) {
                    log::error!("triage chat: cannot write the reply: {e}");
                }
                handle.busy.lock().unwrap_or_else(|e| e.into_inner()).remove(&reply.thread);
                server.send(Event::TriageChat { message: reply });
            })?;
            Ok(())
        })();
        if let Err(e) = started {
            handle.busy.lock().unwrap_or_else(|e| e.into_inner()).remove(&thread.id);
            return Err(error(e));
        }
        self.chat_thread(thread.id)
    }
}

/// The user message of a chat reply: the text of the user, and the current
/// data of each object that the text names with `@item:<id>`,
/// `@session:<chain>` or `@topic:<id>`.
fn chat_prompt(server: &Server, handle: &TriageHandle, thread: i64, text: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "Time: {}", Local::now().format("%Y-%m-%d %H:%M %:z (%A)"));
    let _ = writeln!(s, "recap server: http://127.0.0.1:{}", handle.config.port);
    let _ = writeln!(s, "This is a chat message of the user (see \"Chat\"). The chat thread is {thread}.");
    let _ = writeln!(s);
    let _ = writeln!(s, "{text}");
    let Ok(state) = server.triage_state(&StateQuery::default()) else {
        return s;
    };
    let mut seen = HashSet::new();
    let mut blocks = String::new();
    for word in text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ')' | '(' | '?')) {
        let Some((kind, id)) = word.strip_prefix('@').and_then(|w| w.split_once(':')) else {
            continue;
        };
        let id = id.trim_end_matches(['.', ':', '!']);
        if id.is_empty() || !seen.insert((kind.to_string(), id.to_string())) {
            continue;
        }
        let found = match kind {
            "item" => state.items.iter().find(|i| i.item.id == id).map(json),
            "session" => state.sessions.iter().find(|x| x.note.chain.starts_with(id)).map(|x| {
                let items: Vec<&str> = state
                    .items
                    .iter()
                    .filter(|i| i.item.chain.as_deref() == Some(&x.note.chain) && !i.state.closed())
                    .map(|i| i.item.id.as_str())
                    .collect();
                format!("{}\nOpen items of the session: {}", json(x), items.join(", "))
            }),
            "topic" => state.topics.iter().find(|t| t.id == id).map(|t| {
                let chains: Vec<&str> = state
                    .sessions
                    .iter()
                    .filter(|x| x.note.topic.as_deref() == Some(id))
                    .map(|x| x.note.chain.as_str())
                    .collect();
                format!("{}\nSessions of the topic: {}", json(t), chains.join(", "))
            }),
            _ => None,
        };
        match found {
            Some(data) => {
                let _ = writeln!(blocks, "\n@{kind}:{id}\n{data}");
            }
            None => {
                let _ = writeln!(blocks, "\n@{kind}:{id}\n(no such {kind})");
            }
        }
    }
    if !blocks.is_empty() {
        let _ = write!(s, "\nThe objects that the message names, as they are now:\n{blocks}");
    }
    s
}

fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// Run `claude -p` for one reply, and write the text to `reply` while the
/// agent writes it.
fn run_reply(
    server: &Server,
    handle: &TriageHandle,
    session: &str,
    first: bool,
    model: Model,
    prompt: &str,
    repo_dirs: &BTreeSet<PathBuf>,
    reply: &mut ChatMessage,
) -> anyhow::Result<()> {
    let mut command = triage::agent_command(&handle.config, repo_dirs, model);
    // Each text delta of the reply is one event.
    command.arg("--include-partial-messages");
    if first {
        command.args(["--session-id", session]);
    } else {
        command.args(["--resume", session]);
    }
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes())?;
    }
    let pid = child.id();
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log::info!("triage chat stderr: {line}");
            }
        });
    }
    // The watch thread stops a reply that takes too long. The read loop
    // below then gets the end of the output.
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let started = Instant::now();
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                if started.elapsed() > MAX_REPLY {
                    log::warn!("triage chat: stopped the reply after {}", crate::logger::ms(MAX_REPLY));
                    let _ = Command::new("kill").arg(pid.to_string()).status();
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
    }
    let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    let mut last_event = Instant::now() - EVENT_INTERVAL;
    let mut result: Option<String> = None;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let mut changed = false;
        match event["type"].as_str() {
            Some("stream_event") => {
                let inner = &event["event"];
                match inner["type"].as_str() {
                    Some("content_block_delta") => {
                        if let Some(text) = inner["delta"]["text"].as_str() {
                            reply.text.push_str(text);
                            reply.activity = None;
                            changed = true;
                        }
                    }
                    // A new text block after a tool call starts a new
                    // paragraph.
                    Some("content_block_start") => {
                        if inner["content_block"]["type"].as_str() == Some("text") && !reply.text.is_empty() {
                            reply.text.push_str("\n\n");
                        }
                    }
                    _ => {}
                }
            }
            Some("assistant") => {
                for action in triage::stream_actions(&line) {
                    log::info!("triage chat: {action}");
                }
                let tool = event["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|part| part["type"].as_str() == Some("tool_use"))
                    .filter_map(|part| triage::tool_action(part))
                    .last();
                if let Some(tool) = tool {
                    reply.activity = Some(tool);
                    changed = true;
                }
            }
            Some("result") => {
                result = event["result"].as_str().map(str::to_string);
                if event["is_error"].as_bool() == Some(true) {
                    log::warn!("triage chat: the run ended with an error: {}", result.clone().unwrap_or_default());
                }
            }
            _ => {}
        }
        if changed && last_event.elapsed() >= EVENT_INTERVAL {
            last_event = Instant::now();
            server.send(Event::TriageChat { message: reply.clone() });
        }
    }
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let status = child.wait()?;
    // Without the partial messages, the result holds the only text.
    if reply.text.trim().is_empty() {
        reply.text = result.unwrap_or_default();
    }
    anyhow::ensure!(status.success() || !reply.text.trim().is_empty(), "claude ended with {status}");
    Ok(())
}

/// The number of times that `quote` occurs in `text`. A sequence of white
/// space in one text is equal to any sequence of white space in the other.
fn quote_count(quote: &str, text: &str) -> usize {
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let quote = flat(quote);
    if quote.is_empty() {
        return 0;
    }
    flat(text).matches(&quote).count()
}

#[cfg(test)]
mod tests {
    use super::{github_page, quote_count};

    #[test]
    fn a_quote_matches_with_other_white_space() {
        let text = "The PR is merged.\nI ran  the tests. The PR is open.";
        assert_eq!(quote_count("merged. I ran the tests", text), 1);
        assert_eq!(quote_count("The PR is", text), 2);
        assert_eq!(quote_count("the pr is merged", text), 0);
        assert_eq!(quote_count("  ", text), 0);
    }

    #[test]
    fn the_github_page_of_a_remote() {
        let page = Some("https://github.com/membrane-io/mnode".to_string());
        assert_eq!(github_page("git@github.com:membrane-io/mnode.git"), page);
        assert_eq!(github_page("https://github.com/membrane-io/mnode"), page);
        assert_eq!(github_page("ssh://git@github.com/membrane-io/mnode.git"), page);
        assert_eq!(github_page("git@gitlab.com:membrane-io/mnode.git"), None);
        assert_eq!(github_page("https://github.com/membrane-io"), None);
    }
}
