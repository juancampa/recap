//! The text files of the triage agent, as the Queue mode reads and changes
//! them.
//!
//! The triage agent owns the triage directory (see
//! `crates/recap-server/src/triage_prompt.md` for the format). The TUI reads
//! all the files again when one of them changes. The TUI changes only one
//! line at a time: a value in the front matter, or the check box of a TODO
//! item. It writes a temporary file and renames it, so the triage agent never
//! reads a half-written file.

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

/// One TODO item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub number: usize,
    pub done: bool,
    pub text: String,
}

/// The two TODO lists of an agent file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoList {
    Agent,
    Mine,
}

impl TodoList {
    fn heading(self) -> &'static str {
        match self {
            TodoList::Agent => "agent todo",
            TodoList::Mine => "my todo",
        }
    }
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
    pub agent_todo: Vec<Todo>,
    pub my_todo: Vec<Todo>,
}

impl AgentNotes {
    pub fn todo(&self, list: TodoList) -> &[Todo] {
        match list {
            TodoList::Agent => &self.agent_todo,
            TodoList::Mine => &self.my_todo,
        }
    }
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
        TriageFiles {
            dir: dir.to_path_buf(),
            agents,
            top,
            next_run_reason,
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

    /// Toggle the check box of TODO item `number` of `list`.
    pub fn toggle_todo(&mut self, chain_id: &str, list: TodoList, number: usize) -> std::io::Result<()> {
        let path = self.agent_path(chain_id);
        let text = std::fs::read_to_string(&path)?;
        write_atomic(&path, &toggle_todo(&text, list, number))?;
        *self = TriageFiles::read(&self.dir);
        Ok(())
    }
}

/// The newest modification time of the files that the TUI reads.
fn stamp(dir: &Path) -> Option<SystemTime> {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut times = vec![mtime(&dir.join("top.md")), mtime(&dir.join("next-run")), mtime(&dir.join("agents"))];
    if let Ok(read) = std::fs::read_dir(dir.join("agents")) {
        times.extend(read.flatten().map(|f| mtime(&f.path())));
    }
    times.into_iter().flatten().max()
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("md.tmp");
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

/// `[ ] text` or `[x] text`.
fn check_box(rest: &str) -> Option<(bool, &str)> {
    let done = match rest.get(..3)? {
        "[ ]" => false,
        "[x]" | "[X]" => true,
        _ => return None,
    };
    Some((done, rest[3..].trim()))
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
                h if h == TodoList::Agent.heading() => "agent",
                h if h == TodoList::Mine.heading() => "mine",
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
            "agent" | "mine" => {
                if let Some((number, rest)) = numbered(line)
                    && let Some((done, text)) = check_box(rest)
                {
                    let todo = Todo {
                        number,
                        done,
                        text: text.to_string(),
                    };
                    if section == "agent" {
                        notes.agent_todo.push(todo);
                    } else {
                        notes.my_todo.push(todo);
                    }
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

/// `text` with the check box of item `number` of `list` toggled.
fn toggle_todo(text: &str, list: TodoList, number: usize) -> String {
    let mut in_list = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if let Some(heading) = line.strip_prefix("## ") {
            in_list = heading.trim().to_lowercase() == list.heading();
        }
        let toggled = in_list
            .then(|| numbered(line))
            .flatten()
            .filter(|(n, _)| *n == number)
            .and_then(|_| {
                if let Some(i) = line.find("[ ]") {
                    Some(format!("{}[x]{}", &line[..i], &line[i + 3..]))
                } else {
                    let i = line.find("[x]").or_else(|| line.find("[X]"))?;
                    Some(format!("{}[ ]{}", &line[..i], &line[i + 3..]))
                }
            });
        out.push(toggled.unwrap_or_else(|| line.to_string()));
    }
    out.join("\n")
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
        assert_eq!(
            notes.agent_todo,
            vec![
                Todo { number: 1, done: false, text: "Remove debug logs".to_string() },
                Todo { number: 2, done: true, text: "Update README".to_string() },
            ]
        );
        assert_eq!(notes.my_todo.len(), 1);
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
    fn a_toggle_changes_only_one_item() {
        let text = toggle_todo(FILE, TodoList::Agent, 2);
        let notes = parse_agent(&text);
        assert!(!notes.agent_todo[1].done);
        assert!(!notes.agent_todo[0].done);
        assert!(!notes.my_todo[0].done);
        let text = toggle_todo(&text, TodoList::Mine, 1);
        assert!(parse_agent(&text).my_todo[0].done);
        assert_eq!(toggle_todo(&toggle_todo(FILE, TodoList::Mine, 1), TodoList::Mine, 1), FILE);
    }
}
