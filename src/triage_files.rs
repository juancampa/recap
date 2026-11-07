//! The files of the triage agent, as the Queue mode reads and changes them.
//!
//! The triage agent owns the triage directory (see
//! `crates/recap-server/src/triage_prompt.md` for the format). The TUI reads
//! all the files again when one of them changes. The TUI changes one value
//! at a time: a value in the front matter of an agent file, or the status of
//! one item in `items.json`. It writes a temporary file and renames it, so
//! the triage agent never reads a half-written file.

use recap_api::triage::{ITEMS_FILE, Impact, Item, Items, RESULTS_FILE, Results, State, Status};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One follow-up prompt that the triage agent suggests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowUp {
    pub number: usize,
    /// The short description for the user.
    pub description: String,
    /// The prompt for the agent. It can contain `{placeholder}` text.
    pub prompt: String,
}

/// The file `agents/<chain-id>.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentNotes {
    /// The last turn that the triage agent read.
    pub turn: Option<usize>,
    /// The turn that needs no follow-up.
    pub resolved: Option<usize>,
    /// The turn that the user removed from the queue.
    pub hidden: Option<usize>,
    pub follow_ups: Vec<FollowUp>,
}

/// All the files of the triage directory.
#[derive(Debug, Clone, Default)]
pub struct TriageFiles {
    pub dir: PathBuf,
    /// The notes of each agent, by chain id.
    pub agents: HashMap<String, AgentNotes>,
    /// The chain ids of rank 1, 2 and 3.
    pub top: Vec<String>,
    /// The text of the `next-run` file after the delay.
    pub next_run_reason: Option<String>,
    /// The items of all the agents, from `items.json`.
    pub items: Vec<Item>,
    /// The check results of the items, from `results.json`.
    pub results: Results,
    /// The error when `items.json` cannot be read.
    pub items_error: Option<String>,
    /// The newest change time of the files, to detect a change.
    stamp: Option<SystemTime>,
}

impl TriageFiles {
    /// Read all the files of `dir`.
    pub fn read(dir: &Path) -> TriageFiles {
        let mut agents = HashMap::new();
        if let Ok(read) = std::fs::read_dir(dir.join("agents")) {
            for f in read.flatten() {
                let path = f.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Some(chain) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if let Ok(text) = std::fs::read_to_string(&path) {
                    agents.insert(chain.to_string(), parse_agent(&text));
                }
            }
        }
        let top = std::fs::read_to_string(dir.join("top.md"))
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '-'))
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .take(3)
            .collect();
        let next_run_reason = std::fs::read_to_string(dir.join("next-run"))
            .ok()
            .and_then(|t| t.lines().next().map(str::to_string))
            .and_then(|l| l.split_once(' ').map(|(_, r)| r.trim().to_string()));
        let (items, items_error) = match std::fs::read(dir.join(ITEMS_FILE)) {
            Err(_) => (Vec::new(), None),
            Ok(bytes) => match serde_json::from_slice::<Items>(&bytes) {
                Ok(i) => (i.items, None),
                Err(e) => (Vec::new(), Some(format!("{ITEMS_FILE}: {e}"))),
            },
        };
        let results = std::fs::read(dir.join(RESULTS_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        TriageFiles {
            dir: dir.to_path_buf(),
            agents,
            top,
            next_run_reason,
            items,
            results,
            items_error,
            stamp: stamp(dir),
        }
    }

    /// True when a file of the directory changed after [`Self::read`].
    pub fn changed(&self) -> bool {
        stamp(&self.dir) != self.stamp
    }

    /// The rank of an agent: 1, 2 or 3.
    pub fn rank(&self, chain_id: &str) -> Option<usize> {
        self.top.iter().position(|c| c == chain_id).map(|i| i + 1)
    }

    fn agent_path(&self, chain_id: &str) -> PathBuf {
        self.dir.join("agents").join(format!("{chain_id}.md"))
    }

    /// Set `key` (`resolved` or `hidden`) in the front matter of the agent
    /// file, or clear it with `None`.
    pub fn set_turn_field(
        &mut self,
        chain_id: &str,
        key: &str,
        turn: Option<usize>,
    ) -> std::io::Result<()> {
        let path = self.agent_path(chain_id);
        let text = std::fs::read_to_string(&path)?;
        let value = turn.map(|t| t.to_string()).unwrap_or_default();
        write_atomic(&path, &set_front_matter(&text, key, &value))?;
        *self = TriageFiles::read(&self.dir);
        Ok(())
    }

    /// The items of the chain `chain_id`: the items that are not closed
    /// first, the most important first; then the closed items.
    pub fn items_of(&self, chain_id: &str) -> Vec<&Item> {
        let mut items: Vec<&Item> = self
            .items
            .iter()
            .filter(|i| i.chain.as_deref() == Some(chain_id))
            .collect();
        items.sort_by_key(|i| (i.state(&self.results).closed(), i.impact));
        items
    }

    /// True when the chain has an item that is not closed. With `critical`,
    /// only a critical item counts.
    pub fn has_open_item(&self, chain_id: &str, critical: bool) -> bool {
        self.items.iter().any(|i| {
            i.chain.as_deref() == Some(chain_id)
                && !i.state(&self.results).closed()
                && (!critical || i.impact == Impact::Critical)
        })
    }

    /// The state of an item.
    pub fn state(&self, item: &Item) -> State {
        item.state(&self.results)
    }

    /// Confirm the item `id` as done, or open it again when the user
    /// confirmed it before. The function reads the file again first, so it
    /// keeps the changes of the triage agent.
    pub fn toggle_confirmed(&mut self, id: &str) -> std::io::Result<()> {
        let path = self.dir.join(ITEMS_FILE);
        let mut items: Items = serde_json::from_slice(&std::fs::read(&path)?).map_err(std::io::Error::other)?;
        let Some(item) = items.items.iter_mut().find(|i| i.id == id) else {
            return Err(std::io::Error::other(format!("no item {id}")));
        };
        if item.status == Status::Confirmed {
            item.status = Status::Open;
            item.note.clear();
        } else {
            item.status = Status::Confirmed;
            item.note = format!("The user confirmed it in the queue at {}.", chrono::Local::now().format("%Y-%m-%d %H:%M"));
        }
        let mut text = serde_json::to_string_pretty(&items).map_err(std::io::Error::other)?;
        text.push('\n');
        write_atomic(&path, &text)?;
        *self = TriageFiles::read(&self.dir);
        Ok(())
    }
}

/// The newest modification time of the files that the TUI reads.
fn stamp(dir: &Path) -> Option<SystemTime> {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut times = vec![
        mtime(&dir.join("top.md")),
        mtime(&dir.join("next-run")),
        mtime(&dir.join("agents")),
        mtime(&dir.join(ITEMS_FILE)),
        mtime(&dir.join(RESULTS_FILE)),
    ];
    if let Ok(read) = std::fs::read_dir(dir.join("agents")) {
        times.extend(read.flatten().map(|f| mtime(&f.path())));
    }
    times.into_iter().flatten().max()
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// `12.` at the start of `line`: the number and the rest of the line.
fn numbered(line: &str) -> Option<(usize, &str)> {
    let line = line.trim_start();
    let dot = line.find('.')?;
    let number = line[..dot].parse().ok()?;
    Some((number, line[dot + 1..].trim()))
}

fn parse_agent(text: &str) -> AgentNotes {
    let mut notes = AgentNotes::default();
    let (front, body) = split_front_matter(text);
    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().parse().ok();
        match key.trim() {
            "turn" => notes.turn = value,
            "resolved" => notes.resolved = value,
            "hidden" => notes.hidden = value,
            _ => {}
        }
    }
    let mut section = "";
    for line in body.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            section = match heading.trim().to_lowercase().as_str() {
                "follow-ups" | "follow ups" | "followups" => "follow-ups",
                _ => "",
            };
            continue;
        }
        match section {
            "follow-ups" => {
                if let Some((number, rest)) = numbered(line) {
                    notes.follow_ups.push(FollowUp {
                        number,
                        description: rest.to_string(),
                        prompt: String::new(),
                    });
                } else if let Some(prompt) = line.trim_start().strip_prefix('>')
                    && let Some(f) = notes.follow_ups.last_mut()
                {
                    if !f.prompt.is_empty() {
                        f.prompt.push('\n');
                    }
                    f.prompt.push_str(prompt.strip_prefix(' ').unwrap_or(prompt));
                }
            }
            _ => {}
        }
    }
    notes
}

/// The front matter between the two `---` lines, and the rest.
fn split_front_matter(text: &str) -> (&str, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return ("", text);
    };
    match rest.find("\n---\n") {
        Some(end) => (&rest[..end], &rest[end + 5..]),
        None => ("", text),
    }
}

/// `text` with `key: value` in its front matter. The function adds the key,
/// or the front matter, when it does not exist.
fn set_front_matter(text: &str, key: &str, value: &str) -> String {
    let line = if value.is_empty() { format!("{key}:") } else { format!("{key}: {value}") };
    let (front, body) = split_front_matter(text);
    if front.is_empty() && !text.starts_with("---\n") {
        return format!("---\n{line}\n---\n{text}");
    }
    let mut found = false;
    let mut lines: Vec<String> = front
        .lines()
        .map(|l| match l.split_once(':') {
            Some((k, _)) if k.trim() == key => {
                found = true;
                line.clone()
            }
            _ => l.to_string(),
        })
        .collect();
    if !found {
        lines.push(line);
    }
    format!("---\n{}\n---\n{body}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "---
turn: 12
resolved:
hidden: 9
---
## Follow-ups
1. Fix the CI failures — the agent pushed early
   > Run the tests of {crate} and fix the failures.
   > Then push.
2. Ask for a smaller PR
   > Split this change.

## Agent TODO
1. [ ] Remove debug logs
2. [x] Update README

## My TODO
1. [ ] Test red button
";

    #[test]
    fn the_parser_reads_all_the_parts() {
        let notes = parse_agent(FILE);
        assert_eq!(notes.turn, Some(12));
        assert_eq!(notes.resolved, None);
        assert_eq!(notes.hidden, Some(9));
        assert_eq!(notes.follow_ups.len(), 2);
        assert_eq!(notes.follow_ups[0].description, "Fix the CI failures — the agent pushed early");
        assert_eq!(notes.follow_ups[0].prompt, "Run the tests of {crate} and fix the failures.\nThen push.");
        // An old TODO section does not change the follow-ups.
        assert_eq!(notes.follow_ups[1].prompt, "Split this change.");
    }

    #[test]
    fn a_front_matter_change_keeps_the_body() {
        let text = set_front_matter(FILE, "resolved", "12");
        let notes = parse_agent(&text);
        assert_eq!(notes.resolved, Some(12));
        assert_eq!(notes.follow_ups.len(), 2);
        let cleared = set_front_matter(&text, "hidden", "");
        assert_eq!(parse_agent(&cleared).hidden, None);
        let added = set_front_matter("## My TODO\n", "hidden", "3");
        assert_eq!(parse_agent(&added).hidden, Some(3));
    }

    #[test]
    fn a_confirmation_keeps_the_other_items() {
        let dir = std::env::temp_dir().join(format!("recap-triage-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = r#"{"items":[
            {"id":"a","chain":"c1","owner":"user","text":"Test red button","impact":"normal","status":"open"},
            {"id":"b","chain":"c1","owner":"agent","text":"Fix crash","impact":"critical","why":"Prod aborts","status":"claimed"}]}"#;
        std::fs::write(dir.join(ITEMS_FILE), text).unwrap();
        let mut files = TriageFiles::read(&dir);
        assert_eq!(files.items_of("c1")[0].id, "b");
        assert!(files.has_open_item("c1", true));
        files.toggle_confirmed("b").unwrap();
        assert_eq!(files.state(files.items_of("c1").iter().find(|i| i.id == "b").unwrap()), State::Confirmed);
        assert!(!files.has_open_item("c1", true));
        assert!(files.has_open_item("c1", false));
        files.toggle_confirmed("b").unwrap();
        let b = files.items.iter().find(|i| i.id == "b").unwrap();
        assert_eq!(b.status, Status::Open);
        assert!(b.note.is_empty());
        assert_eq!(files.items.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
