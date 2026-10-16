//! Loader for Claude Code transcripts (`~/.claude/projects/*/<session>.jsonl`).
//!
//! Walks each session in chronological order, replaying `Write`/`Edit`/
//! `MultiEdit` tool uses against an in-memory file-state map to produce
//! per-edit snapshots in `~/.cache/recap/claude/<session>/<message>-<tool>`.
//! The content before the tool call goes to `<message>-<tool>-base`, and the
//! entry carries it as `base_path`, so the diff of the entry shows that one
//! tool call. `Read` tool results seed the initial state for files Claude
//! touched before writing them, and seed it again when a tool call names an
//! `old_string` that the replayed state does not hold (see `apply_edit`).
//! An edit that an agent makes with a Bash command leaves no snapshot; see
//! `bash_calls` for what `recap blame` does with those.
//!
//! Each human prompt becomes a `Prompt` entry. The entry also carries the
//! agent's final response: the last assistant message of the turn that has
//! text and no tool use. Claude Code writes one API message as several lines
//! that share `message.id`, so a text line and a tool-use line of one message
//! are separate lines; a tool-use line discards the candidate response. User
//! lines that the harness writes (`<task-notification>`, `<system-reminder>`,
//! command output) are not prompts and do not end the turn.
//!
//! Claude Code deletes a session file after `cleanupPeriodDays` (30 days by
//! default). The loader keeps a compressed copy of each session in
//! [`archive_dir`], and reads the copy when the session file is gone. See
//! [`transcripts`].

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::history::{find_project_chain, HistoryEntry};
use recap_api::{EditSource, EntryKind};

#[derive(Debug, Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    uuid: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    message: Option<MessageField>,
    #[serde(rename = "toolUseResult")]
    tool_use_result: Option<Value>,
    cwd: Option<String>,
    /// The title Claude Code writes for the session. The app shows it in the
    /// History panel's title column, next to the Cursor composer names.
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    /// `away_summary` on the `system` line that holds the summary of the
    /// session that Claude Code shows while the session is idle.
    subtype: Option<String>,
    /// The text of a `system` line. A `Value`, because a line of another
    /// type can hold a `content` that is not a string, and a parse error
    /// drops the whole line.
    content: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct MessageField {
    role: Option<String>,
    content: Option<Value>,
}

pub fn projects_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".claude/projects");
    dir.exists().then_some(dir)
}

fn cache_root() -> PathBuf {
    crate::paths::cache_root().join("claude")
}

/// Scan all Claude project dirs, emitting one batch per session via
/// `on_batch`. Sessions are processed newest-first (sorted by `.jsonl`
/// mtime descending). Each call also carries the session titles and the
/// session summaries that the session file records (session id → text).
/// `on_batch` returning `false` aborts the scan.
/// Read the session files. `since` limits the scan: the loader skips a file
/// whose mtime is older, because such a file holds no newer entry. `None`
/// reads every file.
///
/// The loader also reads the archived sessions (see [`transcripts`]), and
/// copies each live session that it reads to `archive_dir`. The function
/// returns the ids of the archived sessions.
pub fn load(
    projects_dir: &Path,
    archive_dir: &Path,
    mtimes: &mut HashMap<PathBuf, SystemTime>,
    seen_keys: &mut HashSet<PathBuf>,
    since: Option<SystemTime>,
    on_batch: &mut dyn FnMut(Vec<HistoryEntry>, HashMap<String, String>, HashMap<String, String>) -> bool,
) -> Result<HashSet<String>> {
    let mut chain_cache: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

    let all = transcripts(projects_dir, archive_dir);
    let archived: HashSet<String> = all.iter().filter(|t| t.archived).map(|t| t.id.clone()).collect();

    let mut sessions: Vec<(Transcript, SystemTime)> = Vec::new();
    for t in all {
        let Some(mtime) = fs::metadata(&t.path).ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        // Skip session files whose jsonl hasn't grown since the last scan.
        if let Some(prev) = mtimes.get(&t.path).copied()
            && mtime <= prev
        {
            continue;
        }
        if since.is_some_and(|cut| mtime < cut) {
            continue;
        }
        sessions.push((t, mtime));
    }
    sessions.sort_by(|a, b| b.1.cmp(&a.1));

    for (t, mtime) in sessions {
        let mut batch: Vec<HistoryEntry> = Vec::new();
        let mut titles: HashMap<String, String> = HashMap::new();
        let mut summaries: HashMap<String, String> = HashMap::new();
        let loaded = t.read().and_then(|content| {
            load_session(&t.id, &content, &mut batch, &mut chain_cache, seen_keys, &mut titles, &mut summaries)
        });
        if let Err(e) = loaded {
            log::warn!("skipped session {}: {e}", t.path.display());
        }
        if !t.archived
            && let Err(e) = archive_session(projects_dir, archive_dir, &t.project, &t.id)
        {
            log::warn!("cannot archive session {}: {e}", t.path.display());
        }
        mtimes.insert(t.path, mtime);
        // A session whose file did not grow emits no entry, but it can still
        // carry a title or a summary that an earlier scan did not see.
        if (!batch.is_empty() || !titles.is_empty() || !summaries.is_empty())
            && !on_batch(batch, titles, summaries)
        {
            return Ok(archived);
        }
    }

    Ok(archived)
}

/// One `Bash` tool call of a session. `recap blame` searches these commands
/// for the lines that no snapshot explains: an agent that edits a file with
/// `sed`, `python3 - <<EOF` or a heredoc leaves no `Edit` tool call, but the
/// command text holds the new lines.
#[derive(Debug, Clone)]
pub struct BashCall {
    pub session_id: String,
    pub timestamp: DateTime<Local>,
    pub command: String,
}

/// Every `Bash` tool call in every session under `projects_dir`, and in
/// every archived session (see [`transcripts`]).
pub fn bash_calls(projects_dir: &Path, archive_dir: &Path) -> Vec<BashCall> {
    let mut out = Vec::new();
    for t in transcripts(projects_dir, archive_dir) {
        let Ok(content) = t.read() else {
            continue;
        };
        let session_id = t.id;
        for raw in content.lines() {
            // A cheap text test before the JSON parse: most lines hold
            // no Bash call.
            if !raw.contains("\"name\":\"Bash\"") {
                continue;
            }
            let Ok(line) = serde_json::from_str::<Line>(raw) else {
                continue;
            };
            if line.kind.as_deref() != Some("assistant") {
                continue;
            }
            let Some(ts) = line
                .timestamp
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            else {
                continue;
            };
            let Some(arr) = line
                .message
                .as_ref()
                .and_then(|m| m.content.as_ref())
                .and_then(|c| c.as_array())
            else {
                continue;
            };
            for item in arr {
                if item.get("type").and_then(|t| t.as_str()) != Some("tool_use")
                    || item.get("name").and_then(|n| n.as_str()) != Some("Bash")
                {
                    continue;
                }
                let Some(command) = item
                    .get("input")
                    .and_then(|i| i.get("command"))
                    .and_then(|c| c.as_str())
                else {
                    continue;
                };
                out.push(BashCall {
                    session_id: session_id.clone(),
                    timestamp: ts.with_timezone(&Local),
                    command: command.to_string(),
                });
            }
        }
    }
    out
}

// ── The transcript archive ───────────────────────────────────────────────────
//
// The code in this section is the only code that knows how Claude Code lays
// out a session in the projects directory. `transcripts` and `session_files`
// read that layout, and `restore_session` writes it. If Claude Code changes
// the layout, change the three functions together.

/// The directory of recap's copies of the Claude Code session files. It has
/// the layout of the projects directory, and each file name has the suffix
/// [`ARCHIVE_SUFFIX`].
pub fn archive_dir() -> PathBuf {
    crate::paths::cache_root().join("claude-archive")
}

/// The suffix of each file in [`archive_dir`]: the file is compressed with
/// zstd.
const ARCHIVE_SUFFIX: &str = ".zst";

/// The suffix of a session transcript in the projects directory.
const TRANSCRIPT_SUFFIX: &str = ".jsonl";

/// One session transcript that the loader can read.
struct Transcript {
    /// The name of the project directory, for example `-Users-juan-git-recap`.
    project: String,
    /// The session id: the file name without [`TRANSCRIPT_SUFFIX`].
    id: String,
    /// `<projects>/<project>/<id>.jsonl`, or the archive copy of it.
    path: PathBuf,
    /// True when Claude Code deleted the session file, and `path` is the
    /// archive copy.
    archived: bool,
}

impl Transcript {
    fn read(&self) -> Result<String> {
        if !self.archived {
            return Ok(fs::read_to_string(&self.path)?);
        }
        let data = zstd::decode_all(fs::File::open(&self.path)?)?;
        Ok(String::from_utf8(data)?)
    }
}

/// Every session transcript: each `<project>/<id>.jsonl` in `projects_dir`,
/// and each archive copy in `archive_dir` whose session file does not exist.
///
/// [`restore_session`] writes an archived session back to this layout. A
/// change to how this function or [`session_files`] reads the projects
/// directory must also change [`restore_session`].
fn transcripts(projects_dir: &Path, archive_dir: &Path) -> Vec<Transcript> {
    let mut out = Vec::new();
    for (root, suffix, archived) in [
        (projects_dir, TRANSCRIPT_SUFFIX.to_string(), false),
        (archive_dir, format!("{TRANSCRIPT_SUFFIX}{ARCHIVE_SUFFIX}"), true),
    ] {
        let Ok(read) = fs::read_dir(root) else {
            continue;
        };
        for proj in read.flatten() {
            let proj_path = proj.path();
            if !proj_path.is_dir() {
                continue;
            }
            let Some(project) = proj.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(inner) = fs::read_dir(&proj_path) else {
                continue;
            };
            for f in inner.flatten() {
                let path = f.path();
                if !path.is_file() {
                    continue;
                }
                let Some(id) = f.file_name().to_str().and_then(|n| n.strip_suffix(&suffix)).map(str::to_string)
                else {
                    continue;
                };
                if archived && projects_dir.join(&project).join(format!("{id}{TRANSCRIPT_SUFFIX}")).exists() {
                    continue;
                }
                out.push(Transcript { project: project.clone(), id, path, archived });
            }
        }
    }
    out
}

/// The files of the session `id` of `project` under `root`, relative to
/// `root` and without `suffix`. Claude Code keeps a session as the
/// transcript `<project>/<id>.jsonl` and the optional directory
/// `<project>/<id>/` (subagent transcripts, large tool results). The
/// archive has the same layout, with [`ARCHIVE_SUFFIX`] on each file name.
/// [`archive_session`] and [`restore_session`] copy the files that this
/// function returns.
fn session_files(root: &Path, project: &str, id: &str, suffix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let transcript = PathBuf::from(project).join(format!("{id}{TRANSCRIPT_SUFFIX}"));
    if root.join(with_suffix(&transcript, suffix)).is_file() {
        out.push(transcript);
    }
    add_dir_files(root, &PathBuf::from(project).join(id), suffix, &mut out);
    out
}

/// Add each file under `root/dir` to `out`, relative to `root` and without
/// `suffix`. The function ignores a file without `suffix`, for example a
/// temporary file of [`crate::cache::write_atomic`].
fn add_dir_files(root: &Path, dir: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(read) = fs::read_dir(root.join(dir)) else {
        return;
    };
    for item in read.flatten() {
        let rel = dir.join(item.file_name());
        let Ok(kind) = item.file_type() else {
            continue;
        };
        if kind.is_dir() {
            add_dir_files(root, &rel, suffix, out);
        } else if kind.is_file()
            && let Some(name) = item.file_name().to_str().and_then(|n| n.strip_suffix(suffix))
        {
            out.push(dir.join(name));
        }
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Copy the files of a live session to `archive_dir`. Each archive copy has
/// the mtime of its source file. The function skips a file whose copy has
/// the same mtime, because that file did not change.
fn archive_session(projects_dir: &Path, archive_dir: &Path, project: &str, id: &str) -> Result<()> {
    for rel in session_files(projects_dir, project, id, "") {
        let src = projects_dir.join(&rel);
        let dst = archive_dir.join(with_suffix(&rel, ARCHIVE_SUFFIX));
        // Read the mtime before the content. If Claude Code appends to the
        // file during the copy, the copy gets the older mtime, and the next
        // load copies the file again.
        let mtime = fs::metadata(&src)?.modified()?;
        if fs::metadata(&dst).and_then(|m| m.modified()).ok() == Some(mtime) {
            continue;
        }
        let data = zstd::encode_all(fs::File::open(&src)?, 0)?;
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        crate::cache::write_atomic(&dst, &data)?;
        fs::File::options().write(true).open(&dst)?.set_modified(mtime)?;
    }
    Ok(())
}

/// Write the archived files of the session `id` back to `projects_dir`, so
/// `claude --resume` can open the session again. The function does not
/// replace a file that exists, because Claude Code wrote it.
///
/// The restored files get the current time as mtime, not the original one.
/// With the original mtime, the cleanup of Claude Code can delete the files
/// again before `claude --resume` reads them.
pub fn restore_session(projects_dir: &Path, archive_dir: &Path, id: &str) -> Result<()> {
    let archived = transcripts(projects_dir, archive_dir);
    let Some(t) = archived.iter().find(|t| t.archived && t.id == id) else {
        anyhow::bail!("the archive has no session {id} that Claude Code deleted");
    };
    for rel in session_files(archive_dir, &t.project, id, ARCHIVE_SUFFIX) {
        let dst = projects_dir.join(&rel);
        if dst.exists() {
            continue;
        }
        let data = zstd::decode_all(fs::File::open(archive_dir.join(with_suffix(&rel, ARCHIVE_SUFFIX)))?)?;
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        crate::cache::write_atomic(&dst, &data)?;
    }
    log::info!("restored the archived Claude Code session {id} to {}", projects_dir.join(&t.project).display());
    Ok(())
}

/// Read the transcript `content` of the session `session_id`.
fn load_session(
    session_id: &str,
    content: &str,
    out: &mut Vec<HistoryEntry>,
    chain_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
    seen_keys: &mut HashSet<PathBuf>,
    titles: &mut HashMap<String, String>,
    summaries: &mut HashMap<String, String>,
) -> Result<()> {
    let raw_lines: Vec<Line> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // First pass: index Read tool results by tool_use_id, so an Edit on a
    // file Claude only Read can still reconstruct the base content.
    let mut read_results: HashMap<String, String> = HashMap::new();
    for line in &raw_lines {
        let Some(result) = &line.tool_use_result else {
            continue;
        };
        let Some(content) = result
            .get("file")
            .and_then(|f| f.get("content"))
            .and_then(|c| c.as_str())
        else {
            continue;
        };
        // The tool_use_id lives on the user message's content[].tool_use_id
        let Some(msg) = &line.message else { continue };
        let Some(arr) = msg.content.as_ref().and_then(|c| c.as_array()) else {
            continue;
        };
        for item in arr {
            if let Some(id) = item.get("tool_use_id").and_then(|s| s.as_str()) {
                read_results.insert(id.to_string(), content.to_string());
            }
        }
    }

    // Second pass: replay edits in document order, snapshotting after each.
    let session_id = session_id.to_string();
    let session_cache = cache_root().join(&session_id);

    // The session title. Claude Code appends an `ai-title` line each time it
    // renames the session, so the last line holds the current title.
    if let Some(title) = raw_lines.iter().rev().find_map(|l| {
        if l.kind.as_deref() != Some("ai-title") {
            return None;
        }
        l.ai_title.as_deref().filter(|t| !t.trim().is_empty())
    }) {
        titles.insert(session_id.clone(), title.to_string());
    }

    // The session summary. Claude Code appends an `away_summary` line each
    // time the session becomes idle, so the last line holds the current
    // summary.
    if let Some(summary) = raw_lines.iter().rev().find_map(|l| {
        if l.kind.as_deref() != Some("system") || l.subtype.as_deref() != Some("away_summary") {
            return None;
        }
        l.content.as_ref()?.as_str().filter(|t| !t.trim().is_empty())
    }) {
        summaries.insert(session_id.clone(), summary.to_string());
    }

    // The session's working directory, taken from the first line that records
    // one (a session stays in a single cwd). Used as the `claude --resume`
    // launch directory. `mode`/snapshot lines carry no cwd, so scan for the
    // first non-empty value.
    let session_cwd: Option<PathBuf> = raw_lines
        .iter()
        .find_map(|l| l.cwd.as_deref().filter(|c| !c.is_empty()))
        .map(PathBuf::from);

    let mut state: HashMap<PathBuf, String> = HashMap::new();

    // The prompt whose turn is open, and the final-response candidate of
    // that turn. A turn closes at the next prompt or at the end of the file.
    // A closed turn emits its prompt when the prompt is new, or when it now
    // carries a response, so a session that grows updates the prompt the
    // earlier scan emitted without one (see `App::merge_batch`).
    let mut open_prompt: Option<HistoryEntry> = None;
    let mut candidate: Option<String> = None;
    let mut close_turn = |prompt: Option<HistoryEntry>,
                          response: Option<String>,
                          out: &mut Vec<HistoryEntry>,
                          seen_keys: &mut HashSet<PathBuf>| {
        let Some(mut prompt) = prompt else { return };
        let is_new = !seen_keys.contains(&prompt.file_path);
        if !is_new && response.is_none() {
            return;
        }
        if let EntryKind::Prompt { response: slot, .. } = &mut prompt.kind {
            *slot = response;
        }
        seen_keys.insert(prompt.file_path.clone());
        out.push(prompt);
    };

    for line in &raw_lines {
        // User-typed prompt: open a turn. Tool results also use
        // type=="user" but their content is an array of tool_result items;
        // genuine prompts are either a string or an array of text-only items.
        if line.kind.as_deref() == Some("user") {
            match classify_user_line(line) {
                UserLine::Prompt => {
                    if let Some(entry) = extract_prompt(line, &session_id, chain_cache) {
                        close_turn(open_prompt.take(), candidate.take(), out, seen_keys);
                        open_prompt = Some(entry);
                    }
                }
                UserLine::Interrupt => candidate = None,
                UserLine::Other => {}
            }
            continue;
        }
        if line.kind.as_deref() != Some("assistant") {
            continue;
        }
        let Some(msg) = &line.message else { continue };
        if msg.role.as_deref() != Some("assistant") {
            continue;
        }
        let Some(arr) = msg.content.as_ref().and_then(|c| c.as_array()) else {
            continue;
        };

        // Final-response candidate: a line with text and no tool use. A
        // tool-use line discards the candidate, because the response must
        // come after the last tool use of the turn.
        if open_prompt.is_some() {
            let has_tool_use = arr
                .iter()
                .any(|item| item.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
            if has_tool_use {
                candidate = None;
            } else {
                let text: Vec<&str> = arr
                    .iter()
                    .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                    .collect();
                let text = text.join("\n");
                if !text.trim().is_empty() {
                    candidate = Some(text);
                }
            }
        }

        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let input = match item.get("input") {
                Some(v) => v,
                None => continue,
            };
            let tool_id = item
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let file_path = match input.get("file_path").and_then(|p| p.as_str()) {
                Some(s) => PathBuf::from(s),
                None => continue,
            };

            // `before` is the content the tool call started from. It becomes
            // the `base_path` of the entry, so the diff shows this one tool
            // call and nothing else, like a Cursor blob edit.
            let before: Option<Option<String>> = match name {
                "Write" => {
                    let content = input
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some(state.insert(file_path.clone(), content))
                }
                "Edit" => {
                    let old = input.get("old_string").and_then(|s| s.as_str()).unwrap_or("");
                    let new = input.get("new_string").and_then(|s| s.as_str()).unwrap_or("");
                    let replace_all = input
                        .get("replace_all")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false);
                    apply_edit(&mut state, &read_results, &file_path, old, new, replace_all)
                        .map(Some)
                }
                "MultiEdit" => {
                    let edits = input
                        .get("edits")
                        .and_then(|e| e.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let mut first: Option<String> = None;
                    for edit in edits {
                        let old = edit.get("old_string").and_then(|s| s.as_str()).unwrap_or("");
                        let new = edit.get("new_string").and_then(|s| s.as_str()).unwrap_or("");
                        let replace_all = edit
                            .get("replace_all")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false);
                        let before =
                            apply_edit(&mut state, &read_results, &file_path, old, new, replace_all);
                        if first.is_none() {
                            first = before;
                        }
                    }
                    first.map(Some)
                }
                _ => None,
            };

            let Some(before) = before else {
                continue;
            };
            let Some(ts_str) = &line.timestamp else { continue };
            let Ok(ts) = DateTime::parse_from_rfc3339(ts_str) else { continue };
            let timestamp: DateTime<Local> = ts.with_timezone(&Local);

            let uuid = line
                .uuid
                .clone()
                .or_else(|| line.session_id.clone())
                .unwrap_or_else(|| tool_id.clone());

            let Some(snapshot_path) =
                write_snapshot(&session_cache, &uuid, &tool_id, state.get(&file_path))
            else {
                continue;
            };
            let base_path = before.as_ref().and_then(|b| {
                write_snapshot(&session_cache, &uuid, &format!("{tool_id}-base"), Some(b))
            });

            if seen_keys.contains(&snapshot_path) {
                continue;
            }

            let project_roots = chain_cache
                .entry(file_path.clone())
                .or_insert_with(|| find_project_chain(&file_path))
                .clone();

            seen_keys.insert(snapshot_path.clone());
            out.push(HistoryEntry {
                timestamp,
                file_path,
                snapshot_path,
                base_path,
                project_roots,
                source: EditSource::ClaudeCode,
                kind: EntryKind::FileChange,
                session_id: Some(session_id.clone()),
                session_cwd: session_cwd.clone(),
            });
        }
    }
    close_turn(open_prompt.take(), candidate.take(), out, seen_keys);

    Ok(())
}

/// What a `type=="user"` line is.
enum UserLine {
    /// A prompt the human typed.
    Prompt,
    /// The harness's note that the human interrupted the turn. It ends the
    /// turn without a response.
    Interrupt,
    /// A tool result or a message the harness wrote (a task notification, a
    /// system reminder, command output). It does not end the turn.
    Other,
}

/// Classify a `type=="user"` line by its text. A human prompt is a string or
/// an array of text-only items. A harness message starts with a tag such as
/// `<task-notification>`; an interrupt starts with `[Request interrupted`.
fn classify_user_line(line: &Line) -> UserLine {
    let Some(text) = user_text(line) else {
        return UserLine::Other;
    };
    let text = text.trim_start();
    if text.starts_with("[Request interrupted") {
        return UserLine::Interrupt;
    }
    if is_harness_tag(text) {
        return UserLine::Other;
    }
    UserLine::Prompt
}

/// True when `text` opens with a lowercase tag such as `<system-reminder>`.
/// The harness writes these; a human prompt does not start this way.
fn is_harness_tag(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let name_len = rest
        .chars()
        .take_while(|c| c.is_ascii_lowercase() || *c == '-' || *c == '_')
        .count();
    name_len > 0 && rest[name_len..].starts_with(['>', ' ', '\n'])
}

/// The typed text of a `type=="user"` line, or `None` when the line is a
/// tool result (any non-text item) or has no text.
fn user_text(line: &Line) -> Option<String> {
    let msg = line.message.as_ref()?;
    if msg.role.as_deref() != Some("user") {
        return None;
    }
    let content = msg.content.as_ref()?;
    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else if let Some(arr) = content.as_array() {
        let mut buf = String::new();
        for item in arr {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                        if !buf.is_empty() {
                            buf.push('\n');
                        }
                        buf.push_str(t);
                    }
                }
                // Any non-text item (most importantly tool_result) means this
                // isn't a typed prompt.
                _ => return None,
            }
        }
        buf
    } else {
        return None;
    };
    (!text.trim().is_empty()).then_some(text)
}

/// Pull a user-typed prompt out of a `type=="user"` line. Returns None when
/// the line is a tool result (content is an array of tool_result items),
/// when the prompt text is empty, or when required fields are missing.
fn extract_prompt(
    line: &Line,
    session_id: &str,
    chain_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
) -> Option<HistoryEntry> {
    let text = user_text(line)?;

    let ts_str = line.timestamp.as_ref()?;
    let ts = DateTime::parse_from_rfc3339(ts_str).ok()?;
    let timestamp: DateTime<Local> = ts.with_timezone(&Local);

    // `find_project_chain` walks ancestors of a *file*, so feed it a
    // pseudo-child of cwd so cwd itself can show up in the chain.
    let cwd = line.cwd.as_deref().unwrap_or("");
    let project_roots: Vec<PathBuf> = if cwd.is_empty() {
        Vec::new()
    } else {
        let cwd_path = PathBuf::from(cwd);
        let probe = cwd_path.join(".");
        chain_cache
            .entry(probe.clone())
            .or_insert_with(|| find_project_chain(&probe))
            .clone()
    };

    let uuid = line.uuid.clone().unwrap_or_default();
    let synthetic = PathBuf::from(format!("claude:{session_id}:{uuid}"));

    Some(HistoryEntry {
        timestamp,
        file_path: synthetic,
        snapshot_path: PathBuf::new(),
        base_path: None,
        project_roots,
        source: EditSource::ClaudeCode,
        kind: EntryKind::Prompt {
            text,
            response: None,
        },
        session_id: Some(session_id.to_string()),
        session_cwd: (!cwd.is_empty()).then(|| PathBuf::from(cwd)),
    })
}

/// Replay one `Edit` on the in-memory file state. Returns the content the
/// edit started from, or `None` when the edit changes nothing.
///
/// The replayed state can be stale: a Bash command, another session or the
/// user can change the file on disk between two tool calls, and then
/// `old_string` is not in the state. In that case the function seeds the
/// state again from the best source that holds `old_string`: a `Read` result
/// of the session, then the file on disk, then a stub made of `old_string`
/// alone. The stub keeps the edit itself exact, because the entry diffs
/// against its own base, and the base is the seeded content.
fn apply_edit(
    state: &mut HashMap<PathBuf, String>,
    read_results: &HashMap<String, String>,
    file_path: &Path,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Option<String> {
    if old.is_empty() {
        // Edit semantics for empty old_string is "create file" — match Write.
        return Some(state.insert(file_path.to_path_buf(), new.to_string()).unwrap_or_default());
    }
    let stale = !state.get(file_path).is_some_and(|c| c.contains(old));
    if stale {
        let seed = read_results
            .values()
            .find(|c| c.contains(old))
            .cloned()
            // macOS can ask the user before the server reads a file in a
            // protected folder, and the read waits for the answer.
            .or_else(|| {
                (!crate::paths::in_protected_folder(file_path))
                    .then(|| fs::read_to_string(file_path).ok())
                    .flatten()
                    .filter(|c| c.contains(old))
            })
            .unwrap_or_else(|| old.to_string());
        state.insert(file_path.to_path_buf(), seed);
    }

    let current = state.get_mut(file_path).expect("the state holds the file");
    let before = current.clone();
    if replace_all {
        *current = current.replace(old, new);
    } else {
        let pos = current.find(old).expect("the state holds old_string");
        current.replace_range(pos..pos + old.len(), new);
    }
    if *current == before {
        return None;
    }
    Some(before)
}

fn write_snapshot(
    session_cache: &Path,
    message_uuid: &str,
    tool_id: &str,
    content: Option<&String>,
) -> Option<PathBuf> {
    let content = content?;
    let _ = fs::create_dir_all(session_cache);
    // Combine uuid + tool_id so multiple tool_uses inside one assistant
    // message land in distinct files.
    let name = if tool_id.is_empty() {
        message_uuid.to_string()
    } else {
        format!("{message_uuid}-{tool_id}")
    };
    let path = session_cache.join(name);
    if !path.exists()
        && let Err(e) = crate::cache::write_atomic(&path, content.as_bytes())
    {
        log::warn!("snapshot write failed: {e}");
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use recap_api::EntryFields;

    #[test]
    fn a_harness_tag_is_not_a_prompt() {
        assert!(is_harness_tag("<task-notification>\nfoo"));
        assert!(is_harness_tag("<system-reminder>foo"));
        assert!(is_harness_tag("<command-name>/clear</command-name>"));
        assert!(!is_harness_tag("fix the bug"));
        assert!(!is_harness_tag("< 5 items"));
        assert!(!is_harness_tag("<Foo> is a type"));
    }

    fn line(kind: &str, uuid: &str, content: serde_json::Value) -> String {
        serde_json::json!({
            "type": kind,
            "uuid": uuid,
            "timestamp": "2026-09-07T10:00:00.000Z",
            "cwd": "/tmp/proj",
            "message": { "role": kind, "content": content },
        })
        .to_string()
    }

    fn user(uuid: &str, text: &str) -> String {
        line("user", uuid, serde_json::json!(text))
    }

    fn assistant_text(uuid: &str, text: &str) -> String {
        line("assistant", uuid, serde_json::json!([{ "type": "text", "text": text }]))
    }

    fn assistant_tool(uuid: &str) -> String {
        line(
            "assistant",
            uuid,
            serde_json::json!([{ "type": "tool_use", "id": "t1", "name": "Bash", "input": {} }]),
        )
    }

    fn tool_result(uuid: &str) -> String {
        line(
            "user",
            uuid,
            serde_json::json!([{ "type": "tool_result", "tool_use_id": "t1", "content": "ok" }]),
        )
    }

    /// The session title that Claude Code writes. The app keeps the last
    /// one, because a rename appends a new line.
    #[test]
    fn the_last_ai_title_line_names_the_session() {
        let lines = vec![
            serde_json::json!({ "type": "ai-title", "aiTitle": "First name" }).to_string(),
            user("u1", "hello"),
            serde_json::json!({ "type": "ai-title", "aiTitle": "Page embed argument handling" })
                .to_string(),
        ];
        let mut out = Vec::new();
        let mut chain_cache = HashMap::new();
        let mut titles = HashMap::new();
        let mut summaries = HashMap::new();
        let mut seen = HashSet::new();
        load_session("titled", &lines.join("\n"), &mut out, &mut chain_cache, &mut seen, &mut titles, &mut summaries)
            .unwrap();
        assert_eq!(
            titles.get("titled").map(String::as_str),
            Some("Page embed argument handling")
        );
    }

    /// The summary that Claude Code shows while the session is idle. The
    /// app keeps the last one, because each idle period appends a new line.
    #[test]
    fn the_last_away_summary_line_summarizes_the_session() {
        let summary = |text: &str| {
            serde_json::json!({ "type": "system", "subtype": "away_summary", "content": text }).to_string()
        };
        let lines = vec![
            summary("I read the loader."),
            user("u1", "hello"),
            serde_json::json!({ "type": "attachment", "content": { "kind": "file" } }).to_string(),
            summary("I added the archive."),
        ];
        let mut summaries = HashMap::new();
        load_session(
            "summarized",
            &lines.join("\n"),
            &mut Vec::new(),
            &mut HashMap::new(),
            &mut HashSet::new(),
            &mut HashMap::new(),
            &mut summaries,
        )
        .unwrap();
        assert_eq!(summaries.get("summarized").map(String::as_str), Some("I added the archive."));
    }

    /// Load `lines` as the session `name`. Each test uses its own name,
    /// because the tests run in parallel.
    fn load_lines(name: &str, lines: &[String], seen: &mut HashSet<PathBuf>) -> Vec<HistoryEntry> {
        let mut out = Vec::new();
        let mut chain_cache = HashMap::new();
        let mut titles = HashMap::new();
        let mut summaries = HashMap::new();
        load_session(name, &lines.join("\n"), &mut out, &mut chain_cache, seen, &mut titles, &mut summaries).unwrap();
        out
    }

    #[test]
    fn the_final_text_after_the_last_tool_use_is_the_response() {
        let lines = vec![
            user("u1", "fix the bug"),
            assistant_text("a1", "I will look."),
            assistant_tool("a2"),
            tool_result("r1"),
            user("n1", "<task-notification>\nagent done"),
            assistant_text("a3", "Done. The bug is fixed."),
            user("u2", "thanks, now the tests"),
            assistant_tool("a4"),
            user("i1", "[Request interrupted by user for tool use]"),
            user("u3", "never mind"),
        ];
        let out = load_lines("final-text", &lines, &mut HashSet::new());
        let texts: Vec<(&str, Option<&str>)> = out
            .iter()
            .map(|e| (e.prompt_text().unwrap(), e.response_text()))
            .collect();
        assert_eq!(
            texts,
            vec![
                ("fix the bug", Some("Done. The bug is fixed.")),
                ("thanks, now the tests", None),
                ("never mind", None),
            ]
        );
    }

    #[test]
    fn a_rescan_emits_the_prompt_again_once_its_response_exists() {
        let mut seen = HashSet::new();
        let mut lines = vec![user("u1", "fix the bug"), assistant_tool("a1")];
        let first = load_lines("rescan", &lines, &mut seen);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].response_text(), None);

        // The session grows: the same prompt, now with a response.
        lines.push(tool_result("r1"));
        lines.push(assistant_text("a2", "All good."));
        let second = load_lines("rescan", &lines, &mut seen);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].file_path, first[0].file_path);
        assert_eq!(second[0].response_text(), Some("All good."));

        // Nothing changed: the prompt is not emitted a third time.
        let third = load_lines("rescan", &lines, &mut seen);
        assert_eq!(third.len(), 1, "a prompt with a response is emitted on every scan");
    }

    fn edit_tool(uuid: &str, id: &str, path: &str, old: &str, new: &str) -> String {
        line(
            "assistant",
            uuid,
            serde_json::json!([{ "type": "tool_use", "id": id, "name": "Edit",
                "input": { "file_path": path, "old_string": old, "new_string": new } }]),
        )
    }

    /// The second edit's `old_string` is not in the replayed content, because
    /// something else changed the file in between. The entry still exists,
    /// and its base holds exactly the text the edit replaced.
    #[test]
    fn an_edit_on_stale_state_keeps_an_exact_base() {
        let dir = std::env::temp_dir().join(format!("recap-claude-stale-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lib.rs");
        fs::write(&file, "fn one() {}\n").unwrap();
        let path = file.to_string_lossy().to_string();
        let lines = vec![
            user("u1", "edit twice"),
            edit_tool("a1", "t1", &path, "fn one() {}", "fn one() { 1 }"),
            edit_tool("a2", "t2", &path, "fn bash_added() {}", "fn bash_added() { 2 }"),
        ];
        let out = load_lines("stale", &lines, &mut HashSet::new());
        let edits: Vec<&HistoryEntry> = out.iter().filter(|e| !e.is_prompt()).collect();
        assert_eq!(edits.len(), 2);
        let base = fs::read_to_string(edits[0].base_path.as_ref().unwrap()).unwrap();
        assert_eq!(base, "fn one() {}\n");
        let base = fs::read_to_string(edits[1].base_path.as_ref().unwrap()).unwrap();
        assert_eq!(base, "fn bash_added() {}");
        let after = fs::read_to_string(&edits[1].snapshot_path).unwrap();
        assert_eq!(after, "fn bash_added() { 2 }");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(cache_root().join("stale"));
    }

    fn bash_tool(uuid: &str, command: &str) -> String {
        line(
            "assistant",
            uuid,
            serde_json::json!([{ "type": "tool_use", "id": "t1", "name": "Bash",
                "input": { "command": command } }]),
        )
    }

    /// Load all the sessions under `projects` and `archive`. Returns the
    /// prompt texts and the archived session ids.
    fn load_prompts(projects: &Path, archive: &Path) -> (Vec<String>, HashSet<String>) {
        let mut prompts = Vec::new();
        let mut on_batch = |batch: Vec<HistoryEntry>, _titles: HashMap<String, String>, _summaries: HashMap<String, String>| -> bool {
            prompts.extend(batch.iter().filter_map(|e| e.prompt_text().map(str::to_string)));
            true
        };
        let archived =
            load(projects, archive, &mut HashMap::new(), &mut HashSet::new(), None, &mut on_batch).unwrap();
        (prompts, archived)
    }

    /// The loader copies a session to the archive. After Claude Code deletes
    /// the session, the loader reads the copy, and the restore writes the
    /// same files back.
    #[test]
    fn a_deleted_session_loads_from_the_archive_and_restores() {
        let root = std::env::temp_dir().join(format!("recap-claude-archive-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let projects = root.join("projects");
        let archive = root.join("archive");
        let project = projects.join("-tmp-proj");
        let transcript = [user("u1", "archive me"), bash_tool("a1", "sed -i s/a/b/ x.rs")].join("\n");
        fs::create_dir_all(project.join("s1/subagents")).unwrap();
        fs::write(project.join("s1.jsonl"), &transcript).unwrap();
        fs::write(project.join("s1/subagents/agent-a.jsonl"), "subagent").unwrap();

        let (prompts, archived) = load_prompts(&projects, &archive);
        assert_eq!(prompts, vec!["archive me"]);
        assert!(archived.is_empty());
        assert!(archive.join("-tmp-proj/s1.jsonl.zst").is_file());
        assert!(archive.join("-tmp-proj/s1/subagents/agent-a.jsonl.zst").is_file());

        // Claude Code deletes the session.
        fs::remove_dir_all(&project).unwrap();
        let (prompts, archived) = load_prompts(&projects, &archive);
        assert_eq!(prompts, vec!["archive me"]);
        assert_eq!(archived, HashSet::from(["s1".to_string()]));
        let bash = bash_calls(&projects, &archive);
        assert_eq!(bash.len(), 1);
        assert_eq!(bash[0].session_id, "s1");

        restore_session(&projects, &archive, "s1").unwrap();
        assert_eq!(fs::read_to_string(project.join("s1.jsonl")).unwrap(), transcript);
        assert_eq!(fs::read_to_string(project.join("s1/subagents/agent-a.jsonl")).unwrap(), "subagent");
        let (_, archived) = load_prompts(&projects, &archive);
        assert!(archived.is_empty());
        assert!(restore_session(&projects, &archive, "s1").is_err(), "a live session is not archived");
        let _ = fs::remove_dir_all(&root);
    }

    /// The restore never replaces a file that Claude Code wrote.
    #[test]
    fn a_restore_keeps_the_files_that_exist() {
        let root = std::env::temp_dir().join(format!("recap-claude-restore-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let projects = root.join("projects");
        let archive = root.join("archive");
        let project = projects.join("-tmp-proj");
        fs::create_dir_all(project.join("s2/tool-results")).unwrap();
        fs::write(project.join("s2.jsonl"), user("u1", "hi")).unwrap();
        fs::write(project.join("s2/tool-results/r.txt"), "old").unwrap();
        archive_session(&projects, &archive, "-tmp-proj", "s2").unwrap();

        fs::remove_file(project.join("s2.jsonl")).unwrap();
        fs::write(project.join("s2/tool-results/r.txt"), "new").unwrap();
        restore_session(&projects, &archive, "s2").unwrap();
        assert_eq!(fs::read_to_string(project.join("s2.jsonl")).unwrap(), user("u1", "hi"));
        assert_eq!(fs::read_to_string(project.join("s2/tool-results/r.txt")).unwrap(), "new");
        let _ = fs::remove_dir_all(&root);
    }

    /// Reads the real transcripts. Prints how many prompts carry a response.
    #[test]
    #[ignore]
    fn smoke_load_real_sessions() {
        let dir = projects_dir().expect("~/.claude/projects");
        let mut mtimes = HashMap::new();
        let mut seen = HashSet::new();
        let mut prompts = 0;
        let mut with_response = 0;
        let mut harness = 0;
        let mut on_batch = |batch: Vec<HistoryEntry>, _titles: HashMap<String, String>, _summaries: HashMap<String, String>| -> bool {
            for e in &batch {
                if e.is_prompt() {
                    prompts += 1;
                    if e.response_text().is_some() {
                        with_response += 1;
                    }
                    if e.prompt_text().is_some_and(|t| t.trim_start().starts_with('<')) {
                        harness += 1;
                    }
                }
            }
            true
        };
        load(&dir, &archive_dir(), &mut mtimes, &mut seen, None, &mut on_batch).unwrap();
        eprintln!("prompts={prompts} with_response={with_response} tag-prefixed={harness}");
        assert!(with_response > 0);
    }
}
