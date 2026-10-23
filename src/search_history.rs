//! Remembered search queries, one list per search prompt.
//!
//! Every search prompt in the UI (History path / content / semantic, the
//! Projects, Files and Agents panels, and the Diff panel) keeps its own query list.
//! The user commits a query with Enter, and the prompt appends it to that
//! list. Up and Down then walk the list, as vim walks its command line.
//!
//! Vim filters the walk by the text that the user typed before the first Up.
//! This module does the same: it stores that text as the draft, and it offers
//! only the queries that start with the draft. Down past the newest match puts
//! the draft back on the prompt.
//!
//! The lists survive a restart through [`crate::persist::PersistedState`].

use std::collections::HashMap;

/// How many queries one mode keeps. The oldest query drops out first.
pub const MAX_ENTRIES: usize = 100;

/// One search prompt. Each mode owns a separate query list, because a path
/// query, a regex and a natural-language question are not interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchMode {
    /// History panel, fuzzy match on the display path (`/`).
    Path,
    /// History panel, regex match on the diff body and the prompt text (`f`).
    Content,
    /// History panel, embedding match (`F`).
    Semantic,
    /// Projects panel.
    Project,
    /// Files panel.
    File,
    /// Agents panel.
    Agent,
    /// Diff panel.
    Diff,
}

impl SearchMode {
    /// Name of the mode in the persisted file. A name that the file holds but
    /// this list does not name is dropped on load.
    pub fn key(self) -> &'static str {
        match self {
            SearchMode::Path => "path",
            SearchMode::Content => "content",
            SearchMode::Semantic => "semantic",
            SearchMode::Project => "project",
            SearchMode::File => "file",
            SearchMode::Agent => "agent",
            SearchMode::Diff => "diff",
        }
    }

    /// Every mode, so the caller can save or load all the lists.
    pub const ALL: [SearchMode; 7] = [
        SearchMode::Path,
        SearchMode::Content,
        SearchMode::Semantic,
        SearchMode::Project,
        SearchMode::File,
        SearchMode::Agent,
        SearchMode::Diff,
    ];
}

/// The remembered queries of one mode, plus the walk that Up and Down drive.
#[derive(Debug, Default, Clone)]
pub struct ModeHistory {
    /// Committed queries, oldest first. Holds no duplicate.
    entries: Vec<String>,
    /// Index in `entries` of the query that the prompt shows, while the user
    /// walks the list. `None` when the user types instead of walking.
    cursor: Option<usize>,
    /// The text on the prompt before the walk started. It is the prefix that
    /// filters the walk, and Down restores it at the end of the walk.
    draft: Option<String>,
}

impl ModeHistory {
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Append a committed query. An empty query is not worth a slot. A repeat
    /// of an earlier query moves to the end, so it stays one entry.
    pub fn push(&mut self, query: &str) {
        self.reset_walk();
        if query.is_empty() {
            return;
        }
        self.entries.retain(|e| e != query);
        self.entries.push(query.to_string());
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..excess);
        }
    }

    /// Forget the walk, so the next Up starts from the newest query and takes
    /// a new draft. The caller runs this when the user opens the prompt or
    /// edits the text.
    pub fn reset_walk(&mut self) {
        self.cursor = None;
        self.draft = None;
    }

    /// The next older query that starts with the draft, or `None` when the
    /// walk is at the oldest match. `current` is the text on the prompt; it
    /// becomes the draft if the walk starts here.
    pub fn older(&mut self, current: &str) -> Option<String> {
        if self.draft.is_none() {
            self.draft = Some(current.to_string());
        }
        let prefix = self.draft.clone().unwrap_or_default();
        let start = self.cursor.unwrap_or(self.entries.len());
        let found = self.entries[..start]
            .iter()
            .rposition(|e| e.starts_with(&prefix))?;
        self.cursor = Some(found);
        Some(self.entries[found].clone())
    }

    /// The next newer query that starts with the draft. At the end of the
    /// walk it returns the draft and stops the walk. Returns `None` when no
    /// walk is in progress.
    pub fn newer(&mut self) -> Option<String> {
        let cursor = self.cursor?;
        let prefix = self.draft.clone().unwrap_or_default();
        let next = self.entries[cursor + 1..]
            .iter()
            .position(|e| e.starts_with(&prefix))
            .map(|offset| cursor + 1 + offset);
        match next {
            Some(i) => {
                self.cursor = Some(i);
                Some(self.entries[i].clone())
            }
            None => {
                let draft = self.draft.take().unwrap_or_default();
                self.cursor = None;
                Some(draft)
            }
        }
    }
}

/// The query list of every search prompt.
#[derive(Debug, Default)]
pub struct SearchHistories {
    modes: HashMap<SearchMode, ModeHistory>,
}

impl SearchHistories {
    pub fn get(&self, mode: SearchMode) -> Option<&ModeHistory> {
        self.modes.get(&mode)
    }

    pub fn get_mut(&mut self, mode: SearchMode) -> &mut ModeHistory {
        self.modes.entry(mode).or_default()
    }

    /// The query lists in the shape that the persisted file holds.
    pub fn snapshot(&self) -> HashMap<String, Vec<String>> {
        SearchMode::ALL
            .iter()
            .filter_map(|&mode| {
                let history = self.modes.get(&mode)?;
                if history.entries.is_empty() {
                    return None;
                }
                Some((mode.key().to_string(), history.entries.clone()))
            })
            .collect()
    }

    /// Rebuild the query lists from the persisted file. A name that no mode
    /// claims is dropped, and each list keeps its newest [`MAX_ENTRIES`].
    pub fn from_snapshot(saved: &HashMap<String, Vec<String>>) -> Self {
        let mut modes = HashMap::new();
        for &mode in SearchMode::ALL.iter() {
            let Some(entries) = saved.get(mode.key()) else {
                continue;
            };
            let mut history = ModeHistory::default();
            for query in entries {
                history.push(query);
            }
            modes.insert(mode, history);
        }
        Self { modes }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(queries: &[&str]) -> ModeHistory {
        let mut h = ModeHistory::default();
        for q in queries {
            h.push(q);
        }
        h
    }

    #[test]
    fn up_walks_from_the_newest_query_to_the_oldest() {
        let mut h = history(&["one", "two", "three"]);
        assert_eq!(h.older("").as_deref(), Some("three"));
        assert_eq!(h.older("").as_deref(), Some("two"));
        assert_eq!(h.older("").as_deref(), Some("one"));
        assert_eq!(h.older(""), None, "the walk stops at the oldest query");
    }

    #[test]
    fn down_walks_back_and_restores_the_draft() {
        let mut h = history(&["one", "two"]);
        assert_eq!(h.older("").as_deref(), Some("two"));
        assert_eq!(h.older("").as_deref(), Some("one"));
        assert_eq!(h.newer().as_deref(), Some("two"));
        assert_eq!(h.newer().as_deref(), Some(""), "the draft comes back");
        assert_eq!(h.newer(), None, "no walk is in progress");
    }

    #[test]
    fn the_draft_filters_the_walk_by_prefix() {
        let mut h = history(&["secret handling", "cursor prompts", "secret worker"]);
        assert_eq!(h.older("secret").as_deref(), Some("secret worker"));
        assert_eq!(h.older("secret").as_deref(), Some("secret handling"));
        assert_eq!(h.older("secret"), None);
        assert_eq!(h.newer().as_deref(), Some("secret worker"));
        assert_eq!(h.newer().as_deref(), Some("secret"), "the prefix comes back");
    }

    #[test]
    fn a_repeat_query_moves_to_the_end_and_stays_one_entry() {
        let h = history(&["one", "two", "one"]);
        assert_eq!(h.entries(), ["two", "one"]);
    }

    #[test]
    fn push_drops_the_oldest_query_above_the_cap() {
        let mut h = ModeHistory::default();
        for i in 0..MAX_ENTRIES + 5 {
            h.push(&format!("q{i}"));
        }
        assert_eq!(h.entries().len(), MAX_ENTRIES);
        assert_eq!(h.entries()[0], "q5");
    }

    #[test]
    fn push_ignores_an_empty_query() {
        let h = history(&["", "one", ""]);
        assert_eq!(h.entries(), ["one"]);
    }

    #[test]
    fn a_snapshot_round_trips_every_mode_that_holds_a_query() {
        let mut all = SearchHistories::default();
        all.get_mut(SearchMode::Content).push("worker");
        all.get_mut(SearchMode::Diff).push("fn main");
        let saved = all.snapshot();
        assert_eq!(saved.len(), 2, "an empty mode takes no room in the file");

        let back = SearchHistories::from_snapshot(&saved);
        assert_eq!(back.get(SearchMode::Content).unwrap().entries(), ["worker"]);
        assert_eq!(back.get(SearchMode::Diff).unwrap().entries(), ["fn main"]);
        assert!(back.get(SearchMode::Path).is_none());
    }

    #[test]
    fn from_snapshot_drops_a_name_that_no_mode_claims() {
        let saved = HashMap::from([("nonesuch".to_string(), vec!["x".to_string()])]);
        let back = SearchHistories::from_snapshot(&saved);
        assert!(back.snapshot().is_empty());
    }
}
