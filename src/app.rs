use crate::diff::{build_diff, compute_summary, DiffLine, DiffSummary};
use crate::history::{relative_time_compact, HistoryEntry, HistoryStore};
use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Projects,
    History,
}

/// A consecutive run of edits on the same file.
pub struct HistoryGroup {
    /// Indices into store.entries, sorted most-recent first.
    pub entry_indices: Vec<usize>,
    pub expanded: bool,
}

/// A row in the flattened display list.
#[derive(Debug, Clone)]
pub enum DisplayRow {
    Group(usize),
    Child { group_idx: usize, child_local: usize },
}

pub struct App {
    pub store: HistoryStore,
    pub active_panel: Panel,

    pub selected_project_idx: Option<usize>,
    pub project_scroll: usize,

    pub groups: Vec<HistoryGroup>,
    pub display_rows: Vec<DisplayRow>,

    /// Logical selection — always points at a Child row.
    pub sel_group: usize,
    pub sel_child: usize,

    /// Scroll offset into display_rows.
    pub history_scroll: usize,

    pub diff_scroll: usize,
    pub absolute_time: bool,
    pub diff_lines: Vec<DiffLine>,

    /// Lazily-computed +/- line counts, keyed by entry index.
    pub diff_summaries: HashMap<usize, DiffSummary>,
}

impl App {
    pub fn new(store: HistoryStore) -> Self {
        let mut app = Self {
            store,
            active_panel: Panel::History,
            selected_project_idx: None,
            project_scroll: 0,
            groups: Vec::new(),
            display_rows: Vec::new(),
            sel_group: 0,
            sel_child: 0,
            history_scroll: 0,
            diff_scroll: 0,
            absolute_time: false,
            diff_lines: Vec::new(),
            diff_summaries: HashMap::new(),
        };
        app.refilter();
        app
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn filtered_indices(&self) -> Vec<usize> {
        let project = self
            .selected_project_idx
            .and_then(|i| self.store.projects.get(i));
        self.store
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| match project {
                None => true,
                Some(p) => e.project_root.as_ref() == Some(p),
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn build_groups(entries: &[HistoryEntry], filtered: &[usize]) -> Vec<HistoryGroup> {
        let mut groups: Vec<HistoryGroup> = Vec::new();
        for &idx in filtered {
            let fp = &entries[idx].file_path;
            if let Some(last) = groups.last_mut() {
                if &entries[last.entry_indices[0]].file_path == fp {
                    last.entry_indices.push(idx);
                    continue;
                }
            }
            groups.push(HistoryGroup {
                entry_indices: vec![idx],
                expanded: false,
            });
        }
        groups
    }

    fn rebuild_display(&mut self) {
        self.display_rows = self
            .groups
            .iter()
            .enumerate()
            .flat_map(|(gi, g)| {
                let mut rows = vec![DisplayRow::Group(gi)];
                // Single-entry groups render as a flat row — don't emit a child.
                if g.expanded && g.entry_indices.len() > 1 {
                    for ci in 0..g.entry_indices.len() {
                        rows.push(DisplayRow::Child {
                            group_idx: gi,
                            child_local: ci,
                        });
                    }
                }
                rows
            })
            .collect();
    }

    pub fn refilter(&mut self) {
        let filtered = self.filtered_indices();
        self.groups = Self::build_groups(&self.store.entries, &filtered);
        self.sel_group = 0;
        self.sel_child = 0;
        self.history_scroll = 0;
        self.diff_scroll = 0;
        if !self.groups.is_empty() {
            self.groups[0].expanded = true;
        }
        self.rebuild_display();
        self.refresh_diff();
    }

    pub fn refresh_diff(&mut self) {
        if self.groups.is_empty() {
            self.diff_lines = vec![DiffLine::Header("(no history)".to_string())];
            return;
        }
        let Some(group) = self.groups.get(self.sel_group) else {
            self.diff_lines = vec![DiffLine::Header("(no selection)".to_string())];
            return;
        };
        let Some(&entry_idx) = group.entry_indices.get(self.sel_child) else {
            self.diff_lines = vec![DiffLine::Header("(no selection)".to_string())];
            return;
        };
        let entry = &self.store.entries[entry_idx];
        // Previous snapshot = next entry in the global list with the same file path (older).
        let prev = self.store.entries[entry_idx + 1..]
            .iter()
            .find(|e| e.file_path == entry.file_path)
            .map(|e| e.snapshot_path.as_path());
        self.diff_lines = build_diff(prev, &entry.snapshot_path);
    }

    /// Returns the display-row index of the current selection.
    pub fn sel_display_row(&self) -> usize {
        let single = self
            .groups
            .get(self.sel_group)
            .map(|g| g.entry_indices.len() == 1)
            .unwrap_or(false);
        self.display_rows
            .iter()
            .position(|r| {
                if single {
                    matches!(r, DisplayRow::Group(gi) if *gi == self.sel_group)
                } else {
                    matches!(r, DisplayRow::Child { group_idx, child_local }
                        if *group_idx == self.sel_group && *child_local == self.sel_child)
                }
            })
            .unwrap_or(0)
    }

    fn adjust_scroll(&mut self, visible: usize) {
        let row = self.sel_display_row();
        if row < self.history_scroll {
            self.history_scroll = row;
        } else if visible > 0 && row >= self.history_scroll + visible {
            self.history_scroll = row + 1 - visible;
        }
    }

    /// Epoch (0 = yellow, 1 = orange) per group, flipping on gaps > 1 hour.
    pub fn group_epochs(&self) -> Vec<u8> {
        let mut epochs = vec![0u8; self.groups.len()];
        let mut epoch = 0u8;
        for i in 1..self.groups.len() {
            let prev_oldest_idx = *self.groups[i - 1].entry_indices.last().unwrap();
            let cur_newest_idx = self.groups[i].entry_indices[0];
            let gap = self.store.entries[prev_oldest_idx]
                .timestamp
                .signed_duration_since(self.store.entries[cur_newest_idx].timestamp);
            if gap.num_minutes() > 60 {
                epoch = 1 - epoch;
            }
            epochs[i] = epoch;
        }
        epochs
    }

    pub fn timestamp_str(&self, ts: &DateTime<Local>) -> String {
        if self.absolute_time {
            ts.format("%Y-%m-%d %H:%M").to_string()
        } else {
            relative_time_compact(ts)
        }
    }

    /// Compute diff summaries for the entries currently visible in the
    /// History panel. Cached entries are not recomputed.
    pub fn ensure_summaries_visible(&mut self, visible: usize) {
        let end = (self.history_scroll + visible).min(self.display_rows.len());
        for row_idx in self.history_scroll..end {
            let entry_idx = match self.display_rows[row_idx] {
                DisplayRow::Group(gi) => self.groups[gi].entry_indices[0],
                DisplayRow::Child {
                    group_idx,
                    child_local,
                } => self.groups[group_idx].entry_indices[child_local],
            };
            if self.diff_summaries.contains_key(&entry_idx) {
                continue;
            }
            let entry = &self.store.entries[entry_idx];
            let prev = self.store.entries[entry_idx + 1..]
                .iter()
                .find(|e| e.file_path == entry.file_path)
                .map(|e| e.snapshot_path.clone());
            let summary = compute_summary(prev.as_deref(), &entry.snapshot_path);
            self.diff_summaries.insert(entry_idx, summary);
        }
    }

    pub fn current_entry(&self) -> Option<&HistoryEntry> {
        let group = self.groups.get(self.sel_group)?;
        let &idx = group.entry_indices.get(self.sel_child)?;
        self.store.entries.get(idx)
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    pub fn history_down(&mut self, visible: usize) {
        if self.groups.is_empty() {
            return;
        }
        let group_len = self.groups[self.sel_group].entry_indices.len();
        if self.sel_child + 1 < group_len {
            self.sel_child += 1;
        } else if self.sel_group + 1 < self.groups.len() {
            self.sel_group += 1;
            if !self.groups[self.sel_group].expanded {
                self.groups[self.sel_group].expanded = true;
                self.rebuild_display();
            }
            self.sel_child = 0;
        } else {
            return;
        }
        self.diff_scroll = 0;
        self.adjust_scroll(visible);
        self.refresh_diff();
    }

    pub fn history_up(&mut self, visible: usize) {
        if self.groups.is_empty() {
            return;
        }
        if self.sel_child > 0 {
            self.sel_child -= 1;
        } else if self.sel_group > 0 {
            self.sel_group -= 1;
            if !self.groups[self.sel_group].expanded {
                self.groups[self.sel_group].expanded = true;
                self.rebuild_display();
            }
            self.sel_child = self.groups[self.sel_group].entry_indices.len() - 1;
        } else {
            return;
        }
        self.diff_scroll = 0;
        self.adjust_scroll(visible);
        self.refresh_diff();
    }

    pub fn history_page_down(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        for _ in 0..steps {
            let before = (self.sel_group, self.sel_child);
            self.history_down(visible);
            if (self.sel_group, self.sel_child) == before {
                break;
            }
        }
    }

    pub fn history_page_up(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        for _ in 0..steps {
            let before = (self.sel_group, self.sel_child);
            self.history_up(visible);
            if (self.sel_group, self.sel_child) == before {
                break;
            }
        }
    }

    pub fn diff_page_down(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        let max = self.diff_lines.len().saturating_sub(1);
        self.diff_scroll = (self.diff_scroll + steps).min(max);
    }

    pub fn diff_page_up(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        self.diff_scroll = self.diff_scroll.saturating_sub(steps);
    }

    pub fn diff_down(&mut self) {
        if self.diff_scroll + 1 < self.diff_lines.len() {
            self.diff_scroll += 1;
        }
    }

    pub fn diff_up(&mut self) {
        if self.diff_scroll > 0 {
            self.diff_scroll -= 1;
        }
    }

    pub fn project_down(&mut self, visible: usize) {
        let max = self.store.projects.len();
        let current = self.selected_project_idx.map(|i| i + 1).unwrap_or(0);
        if current < max {
            self.selected_project_idx = Some(current);
            let display_idx = current + 1; // +1 for "All" row
            if display_idx >= self.project_scroll + visible {
                self.project_scroll += 1;
            }
            self.refilter();
        }
    }

    pub fn project_up(&mut self, _visible: usize) {
        match self.selected_project_idx {
            None => {}
            Some(0) => {
                self.selected_project_idx = None;
                self.project_scroll = self.project_scroll.saturating_sub(1);
                self.refilter();
            }
            Some(i) => {
                self.selected_project_idx = Some(i - 1);
                let display_idx = i; // i-1 project = display row i (0 = "All")
                if display_idx < self.project_scroll {
                    self.project_scroll = display_idx;
                }
                self.refilter();
            }
        }
    }

    pub fn project_page_down(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        for _ in 0..steps {
            let before = self.selected_project_idx;
            self.project_down(visible);
            if self.selected_project_idx == before {
                break;
            }
        }
    }

    pub fn project_page_up(&mut self, visible: usize) {
        let steps = (visible / 2).max(1);
        for _ in 0..steps {
            let before = self.selected_project_idx;
            self.project_up(visible);
            if self.selected_project_idx == before {
                break;
            }
        }
    }

    pub fn toggle_panel(&mut self) {
        self.active_panel = match self.active_panel {
            Panel::Projects => Panel::History,
            Panel::History => Panel::Projects,
        };
    }

    pub fn toggle_time(&mut self) {
        self.absolute_time = !self.absolute_time;
    }

    pub fn project_label(path: &PathBuf) -> String {
        let parts: Vec<_> = path.components().collect();
        let start = parts.len().saturating_sub(3);
        parts[start..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }
}
