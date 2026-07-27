use similar::{ChangeTag, TextDiff};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub enum DiffLine {
    Added(String),
    Removed(String),
    Context(String),
    Header(String),
    /// One line of a prompt entry's body. Rendered in light blue, with
    /// a `> ` prefix on the first line.
    Prompt(String),
    /// Per-child separator emitted when a collapsed group's diff is the
    /// concatenation of its children. The renderer formats this with the
    /// same spans the history panel uses, keyed by `entry_idx`.
    ChildSeparator { entry_idx: usize },
}

#[derive(Debug, Clone, Copy)]
pub struct DiffSummary {
    pub added: usize,
    pub removed: usize,
}

/// Cheap diff that only counts added/removed lines, for the history sidebar.
pub fn compute_summary(old_path: Option<&Path>, new_path: &Path) -> DiffSummary {
    let new_content = match fs::read_to_string(new_path) {
        Ok(c) => c,
        Err(_) => return DiffSummary { added: 0, removed: 0 },
    };
    let old_content = match old_path {
        Some(p) => fs::read_to_string(p).unwrap_or_default(),
        None => String::new(),
    };
    let diff = TextDiff::from_lines(&old_content, &new_content);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    DiffSummary { added, removed }
}

/// Build a unified-style diff between `old_path` and `new_path`.
/// Returns lines ready for rendering. If either file can't be read, returns a message.
pub fn build_diff(old_path: Option<&Path>, new_path: &Path) -> Vec<DiffLine> {
    let new_content = match fs::read_to_string(new_path) {
        Ok(c) => c,
        Err(e) => return vec![DiffLine::Header(format!("(cannot read snapshot: {e})"))],
    };

    let old_content = match old_path {
        Some(p) => match fs::read_to_string(p) {
            Ok(c) => c,
            Err(_) => String::new(),
        },
        None => String::new(),
    };

    let diff = TextDiff::from_lines(&old_content, &new_content);
    let mut lines: Vec<DiffLine> = Vec::new();

    for group in diff.grouped_ops(3) {
        // hunk header
        let old_start = group.first().map(|op| op.old_range().start).unwrap_or(0) + 1;
        let new_start = group.first().map(|op| op.new_range().start).unwrap_or(0) + 1;
        let old_len: usize = group.iter().map(|op| op.old_range().len()).sum();
        let new_len: usize = group.iter().map(|op| op.new_range().len()).sum();
        lines.push(DiffLine::Header(format!(
            "@@ -{},{} +{},{} @@",
            old_start, old_len, new_start, new_len
        )));

        for op in &group {
            for change in diff.iter_changes(op) {
                let s = change.value().trim_end_matches('\n').to_string();
                match change.tag() {
                    ChangeTag::Insert => lines.push(DiffLine::Added(s)),
                    ChangeTag::Delete => lines.push(DiffLine::Removed(s)),
                    ChangeTag::Equal => lines.push(DiffLine::Context(s)),
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(DiffLine::Header("(no changes)".to_string()));
    }

    lines
}
