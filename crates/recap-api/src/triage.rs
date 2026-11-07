//! The formats of the item files of the triage directory.
//!
//! The triage agent and the user write `items.json`. The triage runner of the
//! server writes `results.json`. The runner runs the check of each item and
//! records the result, so that the state of an item comes from a mechanical
//! check and not from a claim of an agent. See
//! `crates/recap-server/src/triage_prompt.md` for the rules of the agent.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The file of the items, in the triage directory.
pub const ITEMS_FILE: &str = "items.json";
/// The file of the check results, in the triage directory. Only the runner
/// writes it.
pub const RESULTS_FILE: &str = "results.json";
/// The directory of the check scripts, in the triage directory.
pub const CHECKS_DIR: &str = "checks";

/// The file `items.json`: all the items of all the agents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Items {
    pub items: Vec<Item>,
}

/// One item: a piece of work that an agent or the user must do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// A unique short id, for example `blob-ensure-fix`.
    pub id: String,
    /// The chain of the agent that the item came from. None for an item
    /// that is not about one agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
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
pub struct Event {
    pub event: String,
    /// The script that exits with 0 when the event occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckSpec>,
}

/// The file `results.json`: the check results of each item, by item id.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> Item {
        Item {
            id: "a".to_string(),
            chain: None,
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
    fn the_file_format_is_stable() {
        let text = r#"{"items":[{"id":"x","owner":"user","text":"Deploy prod","impact":"critical",
            "why":"Prod aborts","done_when":"PR 3 merged","check":{"script":"pr-merged","args":["o/r","3"]},
            "before":{"event":"next prod deploy"},"status":"open"}]}"#;
        let items: Items = serde_json::from_str(text).unwrap();
        assert_eq!(items.items[0].impact, Impact::Critical);
        assert_eq!(items.items[0].before.as_ref().unwrap().event, "next prod deploy");
        let back: Items = serde_json::from_str(&serde_json::to_string(&items).unwrap()).unwrap();
        assert_eq!(back, items);
    }
}
