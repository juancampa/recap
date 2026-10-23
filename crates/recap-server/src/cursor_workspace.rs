//! Build a `~/.cursor/plans/<file>.plan.md → project folder` map by walking
//! Cursor's per-workspace storage.
//!
//! Each `~/Library/Application Support/Cursor/User/workspaceStorage/<hash>/`
//! holds:
//! - `workspace.json` — names the workspace target (a `folder` URI, or a
//!   `workspace` URI pointing at a `.code-workspace` file).
//! - `state.vscdb` — SQLite store; the `ItemTable` row keyed
//!   `memento/markdownPlanEditor` lists every plan file URI ever opened in
//!   that workspace.
//!
//! For each `plan_uri` we map the plan's filesystem path to the workspace
//! folder. When a plan appears in multiple workspaces we keep the workspace
//! whose `state.vscdb` was modified most recently, on the assumption that
//! that's where the plan was last touched.

use percent_encoding::percent_decode_str;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Best-effort scan. Errors at any level are swallowed and the entry is
/// skipped — Cursor may have a half-written db, and we never want to block
/// the TUI on this.
pub fn build_plan_project_map() -> HashMap<PathBuf, PathBuf> {
    let Some(storage_root) = workspace_storage_root() else {
        return HashMap::new();
    };
    let Ok(read) = fs::read_dir(&storage_root) else {
        return HashMap::new();
    };

    // plan path → (project folder, db mtime). Keep newest mtime on conflicts.
    let mut map: HashMap<PathBuf, (PathBuf, SystemTime)> = HashMap::new();

    for entry in read.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let workspace_json = dir.join("workspace.json");
        let db = dir.join("state.vscdb");
        if !workspace_json.exists() || !db.exists() {
            continue;
        }

        let Some(project_folder) = parse_workspace_target(&workspace_json) else {
            continue;
        };
        let mtime = fs::metadata(&db)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let plans = read_plan_uris(&db);
        for uri in plans {
            let path = uri_to_path(&uri);
            match map.get(&path) {
                Some((_, prev)) if *prev >= mtime => {}
                _ => {
                    map.insert(path, (project_folder.clone(), mtime));
                }
            }
        }
    }

    map.into_iter().map(|(k, (v, _))| (k, v)).collect()
}

fn workspace_storage_root() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let root = PathBuf::from(home).join("Library/Application Support/Cursor/User/workspaceStorage");
    root.exists().then_some(root)
}

/// Read `workspace.json` and resolve to a filesystem project folder.
/// `folder` URIs map directly. `workspace` URIs point at a `.code-workspace`
/// file; we read its first `folders[].path` (relative to the file's dir),
/// which is the convention Cursor uses when it auto-generates the workspace.
fn parse_workspace_target(workspace_json: &Path) -> Option<PathBuf> {
    let txt = fs::read_to_string(workspace_json).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    if let Some(folder) = v.get("folder").and_then(|f| f.as_str()) {
        return Some(uri_to_path(folder));
    }
    if let Some(ws_uri) = v.get("workspace").and_then(|f| f.as_str()) {
        let ws_path = uri_to_path(ws_uri);
        return resolve_first_workspace_folder(&ws_path);
    }
    None
}

fn resolve_first_workspace_folder(code_workspace: &Path) -> Option<PathBuf> {
    let txt = fs::read_to_string(code_workspace).ok()?;
    // .code-workspace is JSON with comments / trailing commas — try plain
    // serde_json first, fall back to a tolerant strip.
    let v: Value = serde_json::from_str(&txt)
        .or_else(|_| serde_json::from_str(&strip_jsonc(&txt)))
        .ok()?;
    let first = v
        .get("folders")
        .and_then(|f| f.as_array())
        .and_then(|a| a.first())
        .and_then(|o| o.get("path"))
        .and_then(|p| p.as_str())?;
    let base = code_workspace.parent()?;
    let resolved = base.join(first);
    resolved.canonicalize().ok().or(Some(resolved))
}

/// Crude JSONC normalizer: strip `// …` line comments and dangling commas
/// before `}` / `]`. Sufficient for `.code-workspace` files Cursor writes.
fn strip_jsonc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let trimmed = if let Some(idx) = line.find("//") {
            // Only treat as comment if not inside a string. Cursor's
            // .code-workspace files don't use strings containing "//", so
            // a naive split is fine.
            &line[..idx]
        } else {
            line
        };
        out.push_str(trimmed);
        out.push('\n');
    }
    // Strip ", }" / ", ]"
    let mut cleaned = String::with_capacity(out.len());
    let chars: Vec<char> = out.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }
        cleaned.push(chars[i]);
        i += 1;
    }
    cleaned
}

fn read_plan_uris(db: &Path) -> Vec<String> {
    let conn = match Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut uris = Vec::new();
    let keys = ["memento/markdownPlanEditor"];
    for key in keys {
        let Ok(mut stmt) = conn.prepare("SELECT value FROM ItemTable WHERE key = ?") else {
            continue;
        };
        let rows = stmt.query_map([key], |row| row.get::<_, String>(0));
        if let Ok(rows) = rows {
            for row in rows.flatten() {
                extract_plan_uris(&row, &mut uris);
            }
        }
    }
    uris
}

/// Pull every `file://…/.cursor/plans/…plan.md` URI out of an arbitrary
/// JSON blob. The schema we care about (`markdownPlanEditorViewState`) is
/// `[[uri, state], …]`, but using a substring + JSON walk keeps us robust
/// to format changes Cursor might make.
fn extract_plan_uris(blob: &str, out: &mut Vec<String>) {
    let Ok(v): Result<Value, _> = serde_json::from_str(blob) else {
        return;
    };
    walk_value(&v, out);
}

fn walk_value(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            if s.starts_with("file://") && s.contains("/.cursor/plans/") && s.ends_with(".plan.md")
            {
                out.push(s.clone());
            }
        }
        Value::Array(arr) => {
            for item in arr {
                walk_value(item, out);
            }
        }
        Value::Object(obj) => {
            for (_, val) in obj {
                walk_value(val, out);
            }
        }
        _ => {}
    }
}

fn uri_to_path(uri: &str) -> PathBuf {
    let stripped = uri.strip_prefix("file://").unwrap_or(uri);
    let decoded = percent_decode_str(stripped)
        .decode_utf8_lossy()
        .into_owned();
    PathBuf::from(decoded)
}

/// Synthetic project root used when a plan can't be tied to any workspace.
pub fn cursor_plans_root() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".cursor/plans"))
}

/// True when `path` lives directly inside `~/.cursor/plans/`.
pub fn is_plan_file(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(plans) = cursor_plans_root() else {
        return false;
    };
    parent == plans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_jsonc_removes_line_comments() {
        let input = r#"{
  // a comment
  "folders": [
    { "path": "." },
  ],
}"#;
        let cleaned = strip_jsonc(input);
        let v: Value = serde_json::from_str(&cleaned).expect("valid JSON after strip");
        assert_eq!(
            v["folders"][0]["path"].as_str(),
            Some("."),
            "first folder path should round-trip"
        );
    }

    #[test]
    fn extract_plan_uris_picks_only_plan_md_under_cursor_plans() {
        let blob = serde_json::json!({
            "markdownPlanEditorViewState": [
                ["file:///Users/x/.cursor/plans/foo_abc.plan.md", {}],
                ["file:///Users/x/.cursor/plans/bar_xyz.plan.md", {}],
                ["file:///Users/x/git/proj/notes.md", {}],
                ["not a uri", {}]
            ]
        })
        .to_string();
        let mut out = Vec::new();
        extract_plan_uris(&blob, &mut out);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|u| u.ends_with(".plan.md")));
    }

    #[test]
    fn uri_to_path_decodes_percent_escapes() {
        let p = uri_to_path("file:///Users/x/space%20dir/plan.md");
        assert_eq!(p, PathBuf::from("/Users/x/space dir/plan.md"));
    }
}
