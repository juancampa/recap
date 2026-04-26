//! Loader for Claude Code transcripts (`~/.claude/projects/*/<session>.jsonl`).
//!
//! Walks each session in chronological order, replaying `Write`/`Edit`/
//! `MultiEdit` tool uses against an in-memory file-state map to produce
//! per-edit snapshots in `~/.cache/recap/claude/<session>/<message>`.
//! `Read` tool results seed the initial state for files Claude touched
//! before writing them.

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::history::{find_project_chain, EditSource, HistoryEntry};

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
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/recap/claude")
}

pub fn load(projects_dir: &Path) -> Result<Vec<HistoryEntry>> {
    let mut entries = Vec::new();
    let mut chain_cache: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

    let read = match fs::read_dir(projects_dir) {
        Ok(r) => r,
        Err(_) => return Ok(entries),
    };

    for proj in read.flatten() {
        let proj_path = proj.path();
        if !proj_path.is_dir() {
            continue;
        }
        let inner = match fs::read_dir(&proj_path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for f in inner.flatten() {
            let p = f.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Err(e) = load_session(&p, &mut entries, &mut chain_cache) {
                eprintln!("recap: skipped session {}: {e}", p.display());
            }
        }
    }

    Ok(entries)
}

fn load_session(
    path: &Path,
    out: &mut Vec<HistoryEntry>,
    chain_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
) -> Result<()> {
    let content = fs::read_to_string(path)?;
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
    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session")
        .to_string();
    let session_cache = cache_root().join(&session_id);

    let mut state: HashMap<PathBuf, String> = HashMap::new();

    for line in &raw_lines {
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

            let updated = match name {
                "Write" => {
                    let content = input
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    state.insert(file_path.clone(), content);
                    true
                }
                "Edit" => {
                    let old = input.get("old_string").and_then(|s| s.as_str()).unwrap_or("");
                    let new = input.get("new_string").and_then(|s| s.as_str()).unwrap_or("");
                    let replace_all = input
                        .get("replace_all")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(false);
                    apply_edit(&mut state, &read_results, &tool_id, &file_path, old, new, replace_all)
                }
                "MultiEdit" => {
                    let edits = input
                        .get("edits")
                        .and_then(|e| e.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let mut any = false;
                    for edit in edits {
                        let old = edit.get("old_string").and_then(|s| s.as_str()).unwrap_or("");
                        let new = edit.get("new_string").and_then(|s| s.as_str()).unwrap_or("");
                        let replace_all = edit
                            .get("replace_all")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false);
                        if apply_edit(
                            &mut state,
                            &read_results,
                            &tool_id,
                            &file_path,
                            old,
                            new,
                            replace_all,
                        ) {
                            any = true;
                        }
                    }
                    any
                }
                _ => false,
            };

            if !updated {
                continue;
            }
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

            let project_roots = chain_cache
                .entry(file_path.clone())
                .or_insert_with(|| find_project_chain(&file_path))
                .clone();

            out.push(HistoryEntry {
                timestamp,
                file_path,
                snapshot_path,
                project_roots,
                source: EditSource::ClaudeCode,
            });
        }
    }

    Ok(())
}

fn apply_edit(
    state: &mut HashMap<PathBuf, String>,
    read_results: &HashMap<String, String>,
    _tool_id: &str,
    file_path: &Path,
    old: &str,
    new: &str,
    replace_all: bool,
) -> bool {
    if !state.contains_key(file_path) {
        // Prefer a Read result that contains the `old_string` we need to
        // replace — that's the file Claude was looking at. Otherwise fall
        // back to the current on-disk content (best-effort), then to
        // synthesizing a stub from `old_string`.
        let seed = if !old.is_empty() {
            read_results.values().find(|c| c.contains(old)).cloned()
        } else {
            None
        };
        let base = seed
            .or_else(|| fs::read_to_string(file_path).ok())
            .unwrap_or_else(|| if old.is_empty() { String::new() } else { old.to_string() });
        state.insert(file_path.to_path_buf(), base);
    }

    let current = state.get_mut(file_path).unwrap();
    if old.is_empty() {
        // Edit semantics for empty old_string is "create file" — match Write.
        *current = new.to_string();
        return true;
    }
    if replace_all {
        let replaced = current.replace(old, new);
        if replaced == *current {
            return false;
        }
        *current = replaced;
    } else {
        if let Some(pos) = current.find(old) {
            current.replace_range(pos..pos + old.len(), new);
        } else {
            return false;
        }
    }
    true
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
        && let Err(e) = fs::write(&path, content)
    {
        eprintln!("recap: snapshot write failed: {e}");
        return None;
    }
    Some(path)
}
