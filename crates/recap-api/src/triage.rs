//! The types of the triage data.
//!
//! The server keeps the triage data in a database. Triagent and the
//! web UI read it with `GET /v1/triage/state` and change it with a batch of
//! operations ([`Batch`]). The triage runner of the server runs the check of
//! each item and records the result, so that the state of an item comes from
//! a mechanical check and not from a claim of an agent. See
//! `crates/recap-server/src/triage_prompt.md` for the rules of the agent.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The directory of the check scripts, in the triage directory.
pub const CHECKS_DIR: &str = "checks";

/// A topic: the goal that one or more sessions work on, for example
/// "Worker memory management".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Topic {
    /// A short id in lower case with `-`, for example `worker-memory`.
    pub id: String,
    /// 1 to 5 words.
    pub title: String,
    /// The state of the topic in 1 or 2 sentences.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
}

/// What triagent and the user recorded about one session chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionNote {
    pub chain: String,
    /// The id of the topic of the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// The last turn of the session that triagent read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<usize>,
    /// One sentence: what the session does now, or what it needs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// The user muted the session at this turn. The mute ends when the
    /// session gets a new turn. Only the user sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted_turn: Option<usize>,
}

/// One turn of a session chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnRef {
    pub chain: String,
    /// The ordinal of the prompt in the chain, from 1.
    pub turn: usize,
}

/// One item: a piece of work that an agent or the user must do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    /// A unique short id, for example `blob-ensure-fix`.
    pub id: String,
    /// The chain of the agent that the item came from. None for an item
    /// that is not about one agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
    /// The id of the topic, only for an item without a chain. The topic of
    /// an item with a chain is the topic of its session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// The id of the chat thread where triagent asked the user for the
    /// work of the item. The web UI shows a link to the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<i64>,
    pub owner: Owner,
    /// 1 to 8 words.
    pub text: String,
    pub impact: Impact,
    /// What goes wrong if nobody does the item. The agent must write it for
    /// a critical or a high item.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
    /// The observable facts that are true when the item is done.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub done_when: String,
    /// The script that tests `done_when`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckSpec>,
    /// The item must be done before this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<DateTime<Local>>,
    /// The item must be done before this event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Event>,
    #[serde(default)]
    pub status: Status,
    /// For `claimed` and `confirmed`: what shows that the item is done. For
    /// `dropped`: the reason.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// For an item of the user: the text that the user can send to the
    /// session of `chain` to do the item.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    /// The turns where the item came from, and the turns that show its
    /// state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<TurnRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Local>>,
}

/// Who must do the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Owner {
    Agent,
    User,
}

/// What goes wrong if nobody does the item, from the most to the least
/// important.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Impact {
    /// Prod breaks, data is lost, or users see errors.
    Critical,
    /// A deploy, a merge or another agent waits for the item.
    High,
    Normal,
    /// A cleanup or a nice-to-have.
    Low,
}

impl Impact {
    /// The interval between two runs of the check of an item that is not
    /// done.
    pub fn check_interval_secs(self) -> i64 {
        match self {
            Impact::Critical => 10 * 60,
            Impact::High => 30 * 60,
            Impact::Normal => 2 * 3600,
            Impact::Low => 6 * 3600,
        }
    }

    /// The runner alerts this many seconds before the `due` time of an item
    /// that is not done. None for an impact that gets no alert.
    pub fn due_warning_secs(self) -> Option<i64> {
        match self {
            Impact::Critical => Some(4 * 3600),
            Impact::High => Some(2 * 3600),
            Impact::Normal | Impact::Low => None,
        }
    }
}

/// The state that the agent or the user gives to an item. The runner gives
/// the state `verified` from the check result (see [`Item::state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Open,
    /// An agent said that the item is done, but no check confirmed it.
    Claimed,
    /// The user said that the item is done.
    Confirmed,
    /// Nobody will do the item. `note` gives the reason.
    Dropped,
}

/// One run of a script of the `checks` directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckSpec {
    /// The file name of the script in the `checks` directory.
    pub script: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

impl CheckSpec {
    /// The script and the arguments on one line.
    pub fn command_line(&self) -> String {
        std::iter::once(self.script.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// An event that the item must come before, for example "the next mnode
/// prod deploy".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub event: String,
    /// The script that exits with 0 when the event occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckSpec>,
}

/// The check results of each item, by item id.
pub type Results = BTreeMap<String, ItemResults>;

/// The check results of one item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemResults {
    /// The last run of `check`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckRun>,
    /// The last run of `before.check`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<CheckRun>,
    /// The alerts that the runner sent, by alert key. The key contains the
    /// value that caused the alert, so a new `due` time gives a new alert.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub alerts: BTreeMap<String, DateTime<Local>>,
}

/// One run of a check script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    /// The script and the arguments of the run. A result for other
    /// arguments does not apply to the item.
    pub spec: CheckSpec,
    /// The SHA-256 of the approved script that ran.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    pub outcome: Outcome,
    pub at: DateTime<Local>,
    /// The exit code. None when the script did not run or a signal stopped
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    /// The end of the output of the script (stdout and stderr). This is the
    /// evidence for the outcome.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
}

/// The outcome of a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// Exit code 0. For an event check: the event occurred.
    Done,
    /// Exit code 1. For an event check: the event did not occur.
    NotDone,
    /// Another exit code, a time-out, or a script that cannot start.
    Unknown,
    /// The user did not approve this version of the script.
    Unapproved,
    /// The script does not exist.
    Missing,
}

/// The state of an item that the user sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Open,
    /// The check gives `done`.
    Verified,
    Claimed,
    Confirmed,
    Dropped,
    /// The check cannot give a result: `unknown`, `unapproved` or
    /// `missing`.
    Unknown,
}

impl State {
    /// True when nobody must do more work for the item.
    pub fn closed(self) -> bool {
        matches!(self, State::Verified | State::Confirmed | State::Dropped)
    }
}

impl Item {
    /// The last run of the check of the item, if it ran with the current
    /// script and arguments.
    pub fn check_run<'a>(&self, results: &'a Results) -> Option<&'a CheckRun> {
        let run = results.get(&self.id)?.check.as_ref()?;
        (Some(&run.spec) == self.check.as_ref()).then_some(run)
    }

    /// The last run of the event check of the item, if it ran with the
    /// current script and arguments.
    pub fn event_run<'a>(&self, results: &'a Results) -> Option<&'a CheckRun> {
        let run = results.get(&self.id)?.event.as_ref()?;
        let spec = self.before.as_ref()?.check.as_ref()?;
        (&run.spec == spec).then_some(run)
    }

    /// The state of the item. The user and the agent decide `confirmed`
    /// and `dropped`. Otherwise a check result decides.
    pub fn state(&self, results: &Results) -> State {
        match self.status {
            Status::Confirmed => return State::Confirmed,
            Status::Dropped => return State::Dropped,
            Status::Open | Status::Claimed => {}
        }
        match self.check_run(results).map(|r| r.outcome) {
            Some(Outcome::Done) => State::Verified,
            Some(Outcome::Unknown | Outcome::Unapproved | Outcome::Missing) => State::Unknown,
            Some(Outcome::NotDone) | None if self.status == Status::Claimed => State::Claimed,
            Some(Outcome::NotDone) | None => State::Open,
        }
    }

    /// True when the `before` event occurred.
    pub fn event_occurred(&self, results: &Results) -> bool {
        self.event_run(results).is_some_and(|r| r.outcome == Outcome::Done)
    }
}

impl Item {
    /// True when the item is late: its `before` event occurred or its `due`
    /// time passed.
    pub fn late(&self, results: &Results, now: DateTime<Local>) -> bool {
        self.event_occurred(results) || self.due.is_some_and(|d| d <= now)
    }

    /// The sort key of an open item in the priority order. A smaller key
    /// comes first: a late item, then the impact, then an item whose session
    /// waits for the user (`waiting`), then the `due` time, then the age.
    pub fn priority(
        &self,
        results: &Results,
        waiting: bool,
        now: DateTime<Local>,
    ) -> (bool, Impact, bool, i64, i64) {
        (
            !self.late(results, now),
            self.impact,
            !waiting,
            self.due.map_or(i64::MAX, |d| d.timestamp()),
            self.created.map_or(i64::MAX, |c| c.timestamp()),
        )
    }
}

/// The response of `GET /v1/triage/state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageState {
    /// The version of the triage data. Each change gives a new version.
    pub version: u64,
    /// The sequence number of the newest change (see [`Change`]).
    pub last_change: u64,
    pub topics: Vec<Topic>,
    pub sessions: Vec<SessionView>,
    /// The open items in the priority order, then the closed items.
    pub items: Vec<ItemView>,
}

/// An item with the values that the server calculates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemView {
    #[serde(flatten)]
    pub item: Item,
    pub state: State,
    /// See [`Item::late`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub late: bool,
    /// The topic of the item: the topic of its session, or `topic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_topic: Option<String>,
    /// The last run of the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_run: Option<CheckRun>,
    /// The last run of the event check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_run: Option<CheckRun>,
}

/// A session note with the current data of the session chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    #[serde(flatten)]
    pub note: SessionNote,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The GitHub page of the repository of the session, for the links to
    /// its pull requests: `https://github.com/<owner>/<name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The newest session of the chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ts: Option<DateTime<Local>>,
    /// The ordinal of the last prompt of the chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn: Option<usize>,
    /// `busy`, `idle` or `waiting` for a session that runs now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    /// True when `muted_turn` is the last turn of the chain.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
}

/// The body of `POST /v1/triage/apply`: operations that change the triage
/// data. The server does each operation separately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Batch {
    pub ops: Vec<Op>,
}

/// One operation of a [`Batch`]. `set` holds the fields to change. A field
/// with the value `null` gets its default value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Change the item `id`, or make it.
    Item {
        id: String,
        #[serde(default)]
        set: serde_json::Map<String, serde_json::Value>,
    },
    /// Only the user can delete an item.
    DeleteItem { id: String },
    /// Change the note of the session chain `chain`, or make it.
    Session {
        chain: String,
        #[serde(default)]
        set: serde_json::Map<String, serde_json::Value>,
    },
    /// Change the topic `id`, or make it.
    Topic {
        id: String,
        #[serde(default)]
        set: serde_json::Map<String, serde_json::Value>,
    },
    /// Delete a topic that no session and no item uses.
    DeleteTopic { id: String },
}

/// The response of `POST /v1/triage/apply`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResult {
    pub version: u64,
    /// The error of each operation, in the order of the batch. None for an
    /// operation that the server did.
    pub errors: Vec<Option<String>>,
}

/// Who changes the triage data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    User,
    /// The earlier name of this actor was `agent`.
    #[serde(alias = "agent")]
    Triagent,
    Runner,
}

/// One change of one field of the triage data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub seq: u64,
    pub at: DateTime<Local>,
    pub actor: Actor,
    /// `item`, `session` or `topic`.
    pub kind: String,
    pub id: String,
    /// The field that changed. Empty when the object is new or deleted.
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<serde_json::Value>,
}

/// One conversation of the user with triagent. It is one Claude
/// Code session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatThread {
    pub id: i64,
    pub title: String,
    pub session_id: String,
    pub created: DateTime<Local>,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
}

/// One message of a chat thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: i64,
    pub thread: i64,
    /// `user` or `agent`.
    pub role: String,
    pub text: String,
    pub at: DateTime<Local>,
    /// False while the agent writes the message.
    pub done: bool,
    /// The newest action of the agent while it writes the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// The sequence number of the last change of the triage data before
    /// the agent started the message, and at the end of the message. The
    /// changes of the agent between them belong to the message.
    #[serde(default)]
    pub changes_from: u64,
    #[serde(default)]
    pub changes_to: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> Item {
        Item {
            id: "a".to_string(),
            chain: None,
            topic: None,
            thread: None,
            owner: Owner::Agent,
            text: "Fix it".to_string(),
            impact: Impact::High,
            why: String::new(),
            done_when: String::new(),
            check: Some(CheckSpec { script: "pr-merged".to_string(), args: vec!["1".to_string()] }),
            due: None,
            before: None,
            status: Status::Claimed,
            note: String::new(),
            prompt: String::new(),
            refs: Vec::new(),
            created: None,
        }
    }

    fn run(spec: CheckSpec, outcome: Outcome) -> CheckRun {
        CheckRun { spec, sha256: String::new(), outcome, at: Local::now(), exit: None, output: String::new() }
    }

    #[test]
    fn a_result_for_other_arguments_does_not_apply() {
        let item = item();
        let mut results = Results::new();
        let other = CheckSpec { script: "pr-merged".to_string(), args: vec!["2".to_string()] };
        results.insert("a".to_string(), ItemResults { check: Some(run(other, Outcome::Done)), ..Default::default() });
        assert_eq!(item.state(&results), State::Claimed);
        let same = item.check.clone().unwrap();
        results.get_mut("a").unwrap().check = Some(run(same, Outcome::Done));
        assert_eq!(item.state(&results), State::Verified);
    }

    #[test]
    fn the_user_decision_is_stronger_than_the_check() {
        let mut item = item();
        item.status = Status::Dropped;
        let mut results = Results::new();
        let spec = item.check.clone().unwrap();
        results.insert("a".to_string(), ItemResults { check: Some(run(spec, Outcome::NotDone)), ..Default::default() });
        assert_eq!(item.state(&results), State::Dropped);
    }

    #[test]
    fn a_late_item_comes_before_a_critical_item() {
        let now = Local::now();
        let results = Results::new();
        let mut late = item();
        late.impact = Impact::Low;
        late.due = Some(now - chrono::TimeDelta::hours(1));
        let mut critical = item();
        critical.impact = Impact::Critical;
        let mut waiting = item();
        waiting.impact = Impact::Critical;
        let mut keys = [
            (critical.priority(&results, false, now), "critical"),
            (waiting.priority(&results, true, now), "waiting"),
            (late.priority(&results, false, now), "late"),
        ];
        keys.sort();
        assert_eq!(keys.map(|k| k.1), ["late", "waiting", "critical"]);
    }

    #[test]
    fn the_file_format_is_stable() {
        let text = r#"{"items":[{"id":"x","owner":"user","text":"Deploy prod","impact":"critical",
            "why":"Prod aborts","done_when":"PR 3 merged","check":{"script":"pr-merged","args":["o/r","3"]},
            "before":{"event":"next prod deploy"},"status":"open"}]}"#;
        let items: Vec<Item> = serde_json::from_value(serde_json::from_str::<serde_json::Value>(text).unwrap()["items"].clone()).unwrap();
        assert_eq!(items[0].impact, Impact::Critical);
        assert_eq!(items[0].before.as_ref().unwrap().event, "next prod deploy");
        let back: Vec<Item> = serde_json::from_str(&serde_json::to_string(&items).unwrap()).unwrap();
        assert_eq!(back, items);
    }
}
