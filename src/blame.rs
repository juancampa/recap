//! `recap blame`: attribute the hunks of a unified diff to the agent session
//! and the prompt that produced them.
//!
//! The command reads a `git diff`, finds for each hunk the recap entries that
//! added or removed the same lines, and reports the session and the prompt
//! behind the most matched lines. The output is JSON, one object per hunk.

use crate::claude::BashCall;
use crate::history::{EditSource, HistoryStore};
use chrono::{DateTime, Local};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A line shorter than this, after trim, does not vote. Lines like `}` or
/// `)` appear in almost every edit of a file, so they identify no session.
const MIN_VOTING_CHARS: usize = 4;

/// A group wins only when it explains at least this share of the voting
/// lines. Below it the hunk has no winner: one generic line that a script
/// of some session holds does not explain a hunk of 100 lines, and a wrong
/// winner costs the caller more than none.
const MIN_WINNER_SHARE: f64 = 0.25;

/// One file section of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// The path relative to the repository root. The new path for a rename,
    /// the old path for a deleted file.
    pub path: PathBuf,
    /// The lines before the first hunk: `diff --git`, `index`, `---`, `+++`.
    pub header: Vec<String>,
    pub hunks: Vec<Hunk>,
}

/// One `@@` hunk of a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@` line.
    pub header: String,
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    /// The body lines with their ` `, `+`, `-` or `\` prefix.
    pub body: Vec<String>,
}

impl Hunk {
    /// The file header plus this one hunk, as a patch that `git apply` reads.
    pub fn patch(&self, file: &FileDiff) -> String {
        let mut out = String::new();
        for line in &file.header {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&self.header);
        out.push('\n');
        for line in &self.body {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// Parse the text of a unified diff. Binary files are skipped.
pub fn parse_unified(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut file: Option<FileDiff> = None;
    let mut hunk: Option<Hunk> = None;
    let mut binary = false;
    let mut old_path: Option<PathBuf> = None;
    let mut new_path: Option<PathBuf> = None;

    // Close the current file and push it, unless it is binary.
    fn flush(
        files: &mut Vec<FileDiff>,
        file: &mut Option<FileDiff>,
        hunk: &mut Option<Hunk>,
        binary: &mut bool,
        old_path: &mut Option<PathBuf>,
        new_path: &mut Option<PathBuf>,
    ) {
        if let Some(mut f) = file.take() {
            if let Some(h) = hunk.take() {
                f.hunks.push(h);
            }
            if let Some(p) = new_path.take().or_else(|| old_path.take()) {
                f.path = p;
            }
            if !*binary && !f.hunks.is_empty() {
                files.push(f);
            }
        }
        *binary = false;
        *old_path = None;
        *new_path = None;
    }

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush(
                &mut files, &mut file, &mut hunk, &mut binary, &mut old_path, &mut new_path,
            );
            file = Some(FileDiff {
                path: git_header_path(rest),
                header: vec![line.to_string()],
                hunks: Vec::new(),
            });
            continue;
        }
        if file.is_none() {
            // A diff without `diff --git` lines starts at `---`.
            if line.starts_with("--- ") {
                file = Some(FileDiff {
                    path: PathBuf::new(),
                    header: Vec::new(),
                    hunks: Vec::new(),
                });
            } else {
                continue;
            }
        }
        let f = file.as_mut().expect("file is set");

        if line.starts_with("@@ ") {
            if let Some(h) = hunk.take() {
                f.hunks.push(h);
            }
            hunk = parse_hunk_header(line);
            continue;
        }

        match hunk.as_mut() {
            Some(h) => {
                let prefix = line.chars().next();
                match prefix {
                    Some(' ') | Some('+') | Some('-') | Some('\\') => h.body.push(line.to_string()),
                    // Some tools print an empty context line with no space.
                    None => h.body.push(" ".to_string()),
                    _ => {
                        // Not a hunk line: the file ends here.
                        f.hunks.push(hunk.take().expect("hunk is set"));
                        f.header.push(line.to_string());
                    }
                }
            }
            None => {
                if line.starts_with("Binary files") || line.starts_with("GIT binary patch") {
                    binary = true;
                }
                if let Some(p) = line.strip_prefix("--- ") {
                    old_path = header_path(p);
                }
                if let Some(p) = line.strip_prefix("+++ ") {
                    new_path = header_path(p);
                }
                f.header.push(line.to_string());
            }
        }
    }
    flush(
        &mut files, &mut file, &mut hunk, &mut binary, &mut old_path, &mut new_path,
    );
    files
}

/// The new path of a `diff --git a/<old> b/<new>` line.
fn git_header_path(rest: &str) -> PathBuf {
    match rest.find(" b/") {
        Some(i) => PathBuf::from(&rest[i + 3..]),
        None => PathBuf::from(rest),
    }
}

/// The path of a `--- a/<path>` or `+++ b/<path>` line. `None` for `/dev/null`.
fn header_path(p: &str) -> Option<PathBuf> {
    let p = p.split('\t').next().unwrap_or(p);
    if p == "/dev/null" {
        return None;
    }
    let p = p
        .strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p);
    Some(PathBuf::from(p))
}

/// Parse `@@ -a[,b] +c[,d] @@ ...`. A missing count means 1.
fn parse_hunk_header(line: &str) -> Option<Hunk> {
    let mut parts = line.split(' ');
    parts.next()?; // "@@"
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let (old_start, old_lines) = parse_range(old)?;
    let (new_start, new_lines) = parse_range(new)?;
    Some(Hunk {
        header: line.to_string(),
        old_start,
        old_lines,
        new_start,
        new_lines,
        body: Vec::new(),
    })
}

fn parse_range(s: &str) -> Option<(usize, usize)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// One session-and-prompt group that contributed lines to a hunk.
#[derive(Debug, Clone, Serialize)]
pub struct Attribution {
    pub source: EditSource,
    /// The Claude session or the Cursor composer. `None` for a manual edit.
    pub session_id: Option<String>,
    /// The Cursor composer name. Claude has no session name.
    pub session_name: Option<String>,
    /// The synthetic key of the prompt entry, the `file_path` of the prompt.
    pub prompt_key: Option<String>,
    pub prompt_time: Option<String>,
    pub prompt: Option<String>,
    pub response: Option<String>,
    /// The newest edit of this group, RFC 3339.
    pub edit_time: String,
    pub matched_lines: usize,
    /// The part of `matched_lines` that only a Bash command of the session
    /// holds: the agent wrote the file with a shell command, so recap has no
    /// snapshot of it. Weaker evidence than a snapshot line.
    pub bash_lines: usize,
}

/// The attribution of one hunk.
#[derive(Debug, Clone, Serialize)]
pub struct HunkBlame {
    pub file: String,
    pub hunk_index: usize,
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    /// The number of `+` and `-` lines that were allowed to vote.
    pub voting_lines: usize,
    /// The group with the most matched lines, when it explains at least a
    /// quarter of the voting lines. `None` otherwise; the groups are then
    /// all in `others`.
    pub winner: Option<Attribution>,
    /// The other groups, sorted by matched lines.
    pub others: Vec<Attribution>,
    /// The file header plus this hunk, for `git apply --cached`.
    pub patch: String,
}

/// The trimmed added and removed lines of one edit entry.
struct LineSets {
    added: HashSet<String>,
    removed: HashSet<String>,
}

/// The lines of `new` that `old` does not hold, and the reverse. A count per
/// line, so a line that the edit repeats once more counts as added. This is
/// not a diff: a diff of two large files with many changes needs seconds,
/// and the vote only needs the set of lines.
fn line_sets_from(old: &str, new: &str) -> LineSets {
    let mut old_count: HashMap<&str, usize> = HashMap::new();
    for line in old.lines() {
        *old_count.entry(line.trim()).or_default() += 1;
    }
    let mut new_count: HashMap<&str, usize> = HashMap::new();
    for line in new.lines() {
        *new_count.entry(line.trim()).or_default() += 1;
    }
    let mut sets = LineSets {
        added: HashSet::new(),
        removed: HashSet::new(),
    };
    for (line, &n) in &new_count {
        if n > old_count.get(line).copied().unwrap_or(0) {
            sets.added.insert(line.to_string());
        }
    }
    for (line, &n) in &old_count {
        if n > new_count.get(line).copied().unwrap_or(0) {
            sets.removed.insert(line.to_string());
        }
    }
    sets
}

fn line_sets(store: &HistoryStore, idx: usize) -> LineSets {
    let entry = &store.entries[idx];
    let new = std::fs::read_to_string(&entry.snapshot_path).unwrap_or_default();
    let old = match store.prev_snapshot(idx) {
        Some(p) => std::fs::read_to_string(p).unwrap_or_default(),
        None => String::new(),
    };
    line_sets_from(&old, &new)
}

/// Compute the line sets of every candidate entry on all cores.
fn all_line_sets(store: &HistoryStore, todo: &[usize]) -> HashMap<usize, LineSets> {
    if todo.is_empty() {
        return HashMap::new();
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(todo.len());
    let chunk = todo.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = todo
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|&idx| (idx, line_sets(store, idx)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .flatten()
            .collect()
    })
}

/// The prompt entries of each session, oldest first.
fn prompts_by_session(store: &HistoryStore) -> HashMap<&str, Vec<usize>> {
    let mut map: HashMap<&str, Vec<usize>> = HashMap::new();
    // The store is newest-first, so walk it backwards to get oldest-first.
    for (idx, entry) in store.entries.iter().enumerate().rev() {
        if !entry.is_prompt() {
            continue;
        }
        if let Some(session) = entry.session_id.as_deref() {
            map.entry(session).or_default().push(idx);
        }
    }
    map
}

/// The prompt of an edit: the last prompt of the same session that is not
/// newer than the edit.
fn prompt_of(
    store: &HistoryStore,
    prompts: &HashMap<&str, Vec<usize>>,
    session: Option<&str>,
    time: DateTime<Local>,
) -> Option<usize> {
    let list = prompts.get(session?)?;
    let pos = list.partition_point(|&p| store.entries[p].timestamp <= time);
    pos.checked_sub(1).map(|i| list[i])
}

/// One vote of a hunk line: the edit entry, or the Bash call, that holds it.
#[derive(Clone, Copy)]
enum Vote {
    Entry(usize),
    Bash(usize),
}

/// Attribute every hunk of `files` to the sessions and prompts in `store`.
/// `repo_root` turns the relative diff paths into the absolute paths that the
/// store entries carry. `bash` holds the Bash calls of the Claude sessions;
/// a line that no snapshot explains votes for the newest call that holds it.
pub fn attribute(
    store: &HistoryStore,
    files: &[FileDiff],
    repo_root: &Path,
    bash: &[BashCall],
) -> Vec<HunkBlame> {
    let wanted: HashSet<PathBuf> = files.iter().map(|f| repo_root.join(&f.path)).collect();
    // Candidates per absolute path, newest first (the store order).
    let mut candidates: HashMap<&Path, Vec<usize>> = HashMap::new();
    for (idx, entry) in store.entries.iter().enumerate() {
        if !entry.is_prompt() && wanted.contains(&entry.file_path) {
            candidates
                .entry(entry.file_path.as_path())
                .or_default()
                .push(idx);
        }
    }
    let todo: Vec<usize> = candidates.values().flatten().copied().collect();
    let sets = all_line_sets(store, &todo);
    let prompts = prompts_by_session(store);
    // Bash calls newest first, so `find` returns the newest match.
    let mut bash_order: Vec<usize> = (0..bash.len()).collect();
    bash_order.sort_by(|&a, &b| bash[b].timestamp.cmp(&bash[a].timestamp));

    let mut out = Vec::new();
    for file in files {
        let abs = repo_root.join(&file.path);
        let empty = Vec::new();
        let cands = candidates.get(abs.as_path()).unwrap_or(&empty);
        // Only a command that names the file can have written it. Without
        // this test a generic line (`continue;`) votes for whatever session
        // ran the newest script that holds the same line.
        let file_name = file.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let bash_for_file: Vec<usize> = bash_order
            .iter()
            .copied()
            .filter(|&b| !file_name.is_empty() && bash[b].command.contains(file_name))
            .collect();
        for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            let mut voting_lines = 0;
            let mut votes: Vec<Vote> = Vec::new();
            for line in &hunk.body {
                let (added, text) = match line.chars().next() {
                    Some('+') => (true, line[1..].trim()),
                    Some('-') => (false, line[1..].trim()),
                    _ => continue,
                };
                if text.chars().count() < MIN_VOTING_CHARS {
                    continue;
                }
                voting_lines += 1;
                let hit = cands.iter().copied().find(|idx| {
                    let s = &sets[idx];
                    if added {
                        s.added.contains(text)
                    } else {
                        s.removed.contains(text)
                    }
                });
                if let Some(idx) = hit {
                    votes.push(Vote::Entry(idx));
                    continue;
                }
                // No snapshot holds the line. A shell script that edits a
                // file holds both the new text and the text it replaces, so
                // an added and a removed line both vote through Bash. A line
                // that scripts of two sessions hold is generic and does not
                // vote.
                let mut holders = bash_for_file
                    .iter()
                    .copied()
                    .filter(|&b| bash[b].command.contains(text));
                if let Some(b) = holders.next()
                    && holders.all(|o| bash[o].session_id == bash[b].session_id)
                {
                    votes.push(Vote::Bash(b));
                }
            }

            // Group the votes by session and prompt.
            struct Group {
                matched_lines: usize,
                bash_lines: usize,
                source: EditSource,
                edit_time: DateTime<Local>,
            }
            let mut groups: HashMap<(Option<String>, Option<usize>), Group> = HashMap::new();
            for vote in &votes {
                let (session, time, source, is_bash) = match *vote {
                    Vote::Entry(idx) => {
                        let e = &store.entries[idx];
                        (e.session_id.clone(), e.timestamp, e.source, false)
                    }
                    Vote::Bash(b) => (
                        Some(bash[b].session_id.clone()),
                        bash[b].timestamp,
                        EditSource::ClaudeCode,
                        true,
                    ),
                };
                let prompt = prompt_of(store, &prompts, session.as_deref(), time);
                let g = groups.entry((session, prompt)).or_insert(Group {
                    matched_lines: 0,
                    bash_lines: 0,
                    source,
                    edit_time: time,
                });
                g.matched_lines += 1;
                g.bash_lines += usize::from(is_bash);
                if time > g.edit_time {
                    g.edit_time = time;
                    g.source = source;
                }
            }
            let mut attributions: Vec<Attribution> = groups
                .into_iter()
                .map(|((session_id, prompt_idx), g)| {
                    let prompt = prompt_idx.map(|p| &store.entries[p]);
                    Attribution {
                        source: g.source,
                        session_name: session_id
                            .as_deref()
                            .and_then(|s| store.session_titles.get(s).cloned()),
                        session_id,
                        prompt_key: prompt.map(|p| p.file_path.display().to_string()),
                        prompt_time: prompt.map(|p| rfc3339(&p.timestamp)),
                        prompt: prompt.and_then(|p| p.prompt_text().map(str::to_string)),
                        response: prompt.and_then(|p| p.response_text().map(str::to_string)),
                        edit_time: rfc3339(&g.edit_time),
                        matched_lines: g.matched_lines,
                        bash_lines: g.bash_lines,
                    }
                })
                .collect();
            attributions.sort_by(|a, b| {
                b.matched_lines
                    .cmp(&a.matched_lines)
                    .then_with(|| b.edit_time.cmp(&a.edit_time))
            });
            let enough = attributions
                .first()
                .is_some_and(|a| a.matched_lines as f64 >= voting_lines as f64 * MIN_WINNER_SHARE);
            let winner = if enough {
                Some(attributions.remove(0))
            } else {
                None
            };
            out.push(HunkBlame {
                file: file.path.display().to_string(),
                hunk_index,
                old_start: hunk.old_start,
                old_lines: hunk.old_lines,
                new_start: hunk.new_start,
                new_lines: hunk.new_lines,
                voting_lines,
                winner,
                others: attributions,
                patch: hunk.patch(file),
            });
        }
    }
    out
}

fn rfc3339(t: &DateTime<Local>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{HistoryEntry, HistoryEntryKind};

    const TWO_FILES: &str = "\
diff --git a/src/old.rs b/src/new.rs
similarity index 90%
rename from src/old.rs
rename to src/new.rs
index 1111111..2222222 100644
--- a/src/old.rs
+++ b/src/new.rs
@@ -1,3 +1,4 @@
 fn main() {
+    println!(\"hello\");
     let x = 1;
 }
@@ -10 +11 @@
-old line
+new line
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 3333333..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-first
-second
diff --git a/img.png b/img.png
index 4444444..5555555 100644
Binary files a/img.png and b/img.png differ
";

    #[test]
    fn parses_rename_delete_and_skips_binary() {
        let files = parse_unified(TWO_FILES);
        assert_eq!(files.len(), 2);

        let f = &files[0];
        assert_eq!(f.path, PathBuf::from("src/new.rs"));
        assert_eq!(f.header.len(), 7);
        assert_eq!(f.hunks.len(), 2);
        let h = &f.hunks[0];
        assert_eq!((h.old_start, h.old_lines, h.new_start, h.new_lines), (1, 3, 1, 4));
        assert_eq!(h.body.len(), 4);
        let h = &f.hunks[1];
        assert_eq!((h.old_start, h.old_lines, h.new_start, h.new_lines), (10, 1, 11, 1));
        assert_eq!(h.body, vec!["-old line", "+new line", "\\ No newline at end of file"]);

        let g = &files[1];
        assert_eq!(g.path, PathBuf::from("gone.txt"));
        assert_eq!(g.hunks.len(), 1);
        assert_eq!(g.hunks[0].body, vec!["-first", "-second"]);
    }

    #[test]
    fn line_sets_count_repeated_lines() {
        let sets = line_sets_from("a\nb\n  c\n", "a\nb\nb\nc\nd\n");
        assert_eq!(sets.added, HashSet::from(["b".to_string(), "d".to_string()]));
        assert!(sets.removed.is_empty(), "`c` is only re-indented");
    }

    #[test]
    fn patch_holds_the_header_and_one_hunk() {
        let files = parse_unified(TWO_FILES);
        let patch = files[0].hunks[1].patch(&files[0]);
        let expected = "\
diff --git a/src/old.rs b/src/new.rs
similarity index 90%
rename from src/old.rs
rename to src/new.rs
index 1111111..2222222 100644
--- a/src/old.rs
+++ b/src/new.rs
@@ -10 +11 @@
-old line
+new line
\\ No newline at end of file
";
        assert_eq!(patch, expected);
    }

    fn ts(minutes: i64) -> DateTime<Local> {
        Local::now() - chrono::Duration::minutes(100 - minutes)
    }

    fn edit(path: &Path, snapshot: &Path, session: &str, minutes: i64) -> HistoryEntry {
        HistoryEntry {
            timestamp: ts(minutes),
            file_path: path.to_path_buf(),
            snapshot_path: snapshot.to_path_buf(),
            base_path: None,
            project_roots: Vec::new(),
            source: EditSource::ClaudeCode,
            kind: HistoryEntryKind::FileChange,
            session_id: Some(session.to_string()),
            session_cwd: None,
        }
    }

    fn prompt(text: &str, session: &str, minutes: i64) -> HistoryEntry {
        HistoryEntry {
            timestamp: ts(minutes),
            file_path: PathBuf::from(format!("claude:{session}:{text}")),
            snapshot_path: PathBuf::new(),
            base_path: None,
            project_roots: Vec::new(),
            source: EditSource::ClaudeCode,
            kind: HistoryEntryKind::Prompt {
                text: text.to_string(),
                response: Some(format!("done: {text}")),
            },
            session_id: Some(session.to_string()),
            session_cwd: None,
        }
    }

    #[test]
    fn attributes_each_hunk_to_the_session_and_prompt_that_added_its_lines() {
        let dir = std::env::temp_dir().join(format!("recap-blame-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("repo");
        let file = root.join("lib.rs");
        let snaps: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("snap{i}"))).collect();
        std::fs::write(&snaps[0], "fn one() {}\nfn keep() {}\n").unwrap();
        // Session s1 adds `two`.
        std::fs::write(&snaps[1], "fn one() {}\nfn two_long() {}\nfn keep() {}\n").unwrap();
        // Session s2 adds `three` and removes `one`.
        std::fs::write(
            &snaps[2],
            "fn two_long() {}\nfn keep() {}\nfn three_long() {}\n",
        )
        .unwrap();

        let store = HistoryStore::from_entries(vec![
            prompt("add two", "s1", 10),
            edit(&file, &snaps[0], "s1", 11),
            edit(&file, &snaps[1], "s1", 12),
            prompt("add three", "s2", 20),
            prompt("later prompt", "s2", 30),
            edit(&file, &snaps[2], "s2", 21),
        ]);

        let diff = "\
diff --git a/lib.rs b/lib.rs
--- a/lib.rs
+++ b/lib.rs
@@ -1,2 +1,2 @@
-fn one() {}
+fn two_long() {}
 fn keep() {}
@@ -3,0 +3,1 @@
+fn three_long() {}
@@ -5,0 +5,1 @@
+fn nobody_added_this() {}
";
        let files = parse_unified(diff);
        let bash = vec![BashCall {
            session_id: "s2".to_string(),
            timestamp: ts(25),
            command: "python3 - <<'EOF'\np = 'lib.rs'\ns = s.replace('x', 'fn nobody_added_this() {}')\nEOF".to_string(),
        }];
        let blames = attribute(&store, &files, &root, &bash);
        assert_eq!(blames.len(), 3);

        let first = &blames[0];
        assert_eq!(first.voting_lines, 2);
        let w = first.winner.as_ref().unwrap();
        // One vote each; the newer edit (s2) wins the tie.
        assert_eq!(w.session_id.as_deref(), Some("s2"));
        assert_eq!(w.prompt.as_deref(), Some("add three"));
        assert_eq!(w.matched_lines, 1);
        assert_eq!(first.others.len(), 1);
        assert_eq!(first.others[0].session_id.as_deref(), Some("s1"));
        assert_eq!(first.others[0].prompt.as_deref(), Some("add two"));
        assert_eq!(first.others[0].response.as_deref(), Some("done: add two"));

        let second = &blames[1];
        let w = second.winner.as_ref().unwrap();
        assert_eq!(w.session_id.as_deref(), Some("s2"));
        assert_eq!(w.prompt_key.as_deref(), Some("claude:s2:add three"));
        assert!(second.others.is_empty());

        // No snapshot holds the third line, but a Bash call of s2 does. The
        // call ran after the "add three" prompt and before "later prompt".
        let third = &blames[2];
        assert_eq!(third.voting_lines, 1);
        let w = third.winner.as_ref().unwrap();
        assert_eq!(w.session_id.as_deref(), Some("s2"));
        assert_eq!(w.prompt.as_deref(), Some("add three"));
        assert_eq!((w.matched_lines, w.bash_lines), (1, 1));
        assert_eq!(second.winner.as_ref().unwrap().bash_lines, 0);
        assert!(third.patch.starts_with("diff --git a/lib.rs b/lib.rs\n"));

        // A hunk that nothing explains has no winner.
        let unknown = parse_unified("--- a/lib.rs\n+++ b/lib.rs\n@@ -9,0 +9,1 @@\n+fn from_nowhere() {}\n");
        let blames = attribute(&store, &unknown, &root, &bash);
        assert!(blames[0].winner.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
