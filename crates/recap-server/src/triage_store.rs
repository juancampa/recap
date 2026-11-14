//! The database of the triage data: the topics, the session notes, the
//! items, the check results, the changes and the chat with triagent.
//!
//! The database file is next to the triage directory and not in it, so
//! triagent cannot write to the file. It changes the data with
//! `POST /v1/triage/apply`, and the server validates each operation (see
//! [`TriageStore::apply`]).
//!
//! Each object is one row that holds the JSON of its wire type from
//! `recap_api::triage`. The data is small (hundreds of items), so each read
//! parses all the rows of a kind.

use crate::checks::valid_script_name;
use anyhow::Context as _;
use chrono::{DateTime, Local};
use recap_api::triage::{
    Actor, Batch, BatchResult, Change, ChatMessage, ChatThread, Impact, Item, ItemResults, Op, Owner, Results, SessionNote,
    Status, Topic,
};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

const ITEM: &str = "item";
const SESSION: &str = "session";
const TOPIC: &str = "topic";
const RESULT: &str = "result";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS objects (
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (kind, id)
);
CREATE TABLE IF NOT EXISTS changes (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    actor TEXT NOT NULL,
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    field TEXT NOT NULL,
    old TEXT,
    new TEXT
);
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS chat_threads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    session_id TEXT NOT NULL,
    created TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS chat_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread INTEGER NOT NULL,
    role TEXT NOT NULL,
    text TEXT NOT NULL,
    at TEXT NOT NULL,
    done INTEGER NOT NULL,
    changes_from INTEGER NOT NULL DEFAULT 0,
    changes_to INTEGER NOT NULL DEFAULT 0
);
";

/// The path of the database of the triage directory `dir`.
pub fn db_path(dir: &Path) -> PathBuf {
    sibling(dir, "db")
}

/// `<dir>.<suffix>`, next to `dir`.
fn sibling(dir: &Path, suffix: &str) -> PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "triage".to_string());
    dir.with_file_name(format!("{name}.{suffix}"))
}

/// The triage database. Each method takes the lock for one short
/// transaction.
pub struct TriageStore {
    conn: Mutex<Connection>,
}

impl TriageStore {
    /// Open the database of the triage directory `dir`, and make it if it
    /// does not exist. A new database gets the data of the files of the
    /// earlier format (see [`import_legacy`]).
    pub fn open(dir: &Path) -> anyhow::Result<TriageStore> {
        let path = db_path(dir);
        let new = !path.exists();
        let store = TriageStore::from_connection(Connection::open(&path)?)?;
        if new {
            let report = import_legacy(&store, dir)?;
            if let Some(report) = report {
                log::info!("triage: {report}");
            }
        }
        Ok(store)
    }

    /// A database in memory, for tests.
    pub fn in_memory() -> anyhow::Result<TriageStore> {
        TriageStore::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> anyhow::Result<TriageStore> {
        conn.execute_batch(SCHEMA)?;
        // The earlier name of the actor `triagent` was `agent`.
        conn.execute("UPDATE changes SET actor = 'triagent' WHERE actor = 'agent'", [])?;
        Ok(TriageStore { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The version of the data. Each change gives a new version.
    pub fn version(&self) -> u64 {
        version(&self.conn())
    }

    pub fn items(&self) -> Vec<Item> {
        all(&self.conn(), ITEM)
    }

    pub fn topics(&self) -> Vec<Topic> {
        all(&self.conn(), TOPIC)
    }

    pub fn sessions(&self) -> Vec<SessionNote> {
        all(&self.conn(), SESSION)
    }

    /// The check results of all the items.
    pub fn results(&self) -> Results {
        let conn = self.conn();
        let mut results = Results::new();
        for (id, json) in rows(&conn, RESULT) {
            match serde_json::from_str::<ItemResults>(&json) {
                Ok(r) => {
                    results.insert(id, r);
                }
                Err(e) => log::warn!("triage store: the result of {id} is not valid: {e}"),
            }
        }
        results
    }

    /// Write the check results of the item `id`. Only the check runner
    /// calls it.
    pub fn put_results(&self, id: &str, results: &ItemResults) -> anyhow::Result<()> {
        let conn = self.conn();
        put(&conn, RESULT, id, &serde_json::to_value(results)?)?;
        bump(&conn)?;
        Ok(())
    }

    /// Do each operation of `batch` for `actor`. An operation that is not
    /// valid changes nothing and gives an error. The other operations of
    /// the batch are not affected.
    pub fn apply(&self, actor: Actor, batch: &Batch) -> BatchResult {
        let conn = self.conn();
        let now = Local::now();
        let errors: Vec<Option<String>> = batch
            .ops
            .iter()
            .map(|op| apply_op(&conn, actor, op, now).err().map(|e| e.to_string()))
            .collect();
        if errors.iter().any(Option::is_none) {
            if let Err(e) = bump(&conn) {
                log::error!("triage store: cannot write the version: {e}");
            }
        }
        BatchResult { version: version(&conn), errors }
    }

    /// The changes with a sequence number after `after`, oldest first, at
    /// most `limit`.
    pub fn changes_after(&self, after: u64, limit: usize) -> Vec<Change> {
        self.changes("seq > ?1 ORDER BY seq LIMIT ?2", params![after as i64, limit as i64])
    }

    /// The changes of `actor` with a sequence number after `from` and not
    /// after `to`, oldest first.
    pub fn changes_between(&self, actor: Actor, from: u64, to: u64) -> Vec<Change> {
        self.changes(
            "actor = ?1 AND seq > ?2 AND seq <= ?3 ORDER BY seq",
            params![actor_word(actor), from as i64, to as i64],
        )
    }

    /// The sequence number of the newest change.
    pub fn last_change(&self) -> u64 {
        self.conn()
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM changes", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as u64
    }

    fn changes(&self, filter: &str, params: impl rusqlite::Params) -> Vec<Change> {
        let conn = self.conn();
        let sql = format!("SELECT seq, at, actor, kind, id, field, old, new FROM changes WHERE {filter}");
        let Ok(mut statement) = conn.prepare(&sql) else {
            return Vec::new();
        };
        let value = |s: Option<String>| s.and_then(|s| serde_json::from_str(&s).ok());
        let rows = statement.query_map(params, |r| {
            Ok(Change {
                seq: r.get::<_, i64>(0)? as u64,
                at: time(&r.get::<_, String>(1)?),
                actor: match r.get::<_, String>(2)?.as_str() {
                    "user" => Actor::User,
                    "runner" => Actor::Runner,
                    // `agent` is the earlier name of this actor.
                    _ => Actor::Triagent,
                },
                kind: r.get(3)?,
                id: r.get(4)?,
                field: r.get(5)?,
                old: value(r.get(6)?),
                new: value(r.get(7)?),
            })
        });
        rows.map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    // ── The chat with triagent ──────────────────────────────────────

    /// Make a chat thread for the Claude Code session `session_id`.
    pub fn new_thread(&self, title: &str, session_id: &str) -> anyhow::Result<ChatThread> {
        let conn = self.conn();
        let created = Local::now();
        conn.execute(
            "INSERT INTO chat_threads (title, session_id, created) VALUES (?1, ?2, ?3)",
            params![title, session_id, created.to_rfc3339()],
        )?;
        Ok(ChatThread {
            id: conn.last_insert_rowid(),
            title: title.to_string(),
            session_id: session_id.to_string(),
            created,
            messages: Vec::new(),
        })
    }

    /// All the threads without their messages, newest first.
    pub fn threads(&self) -> Vec<ChatThread> {
        let conn = self.conn();
        let Ok(mut statement) =
            conn.prepare("SELECT id, title, session_id, created FROM chat_threads ORDER BY id DESC")
        else {
            return Vec::new();
        };
        let rows = statement.query_map([], |r| {
            Ok(ChatThread {
                id: r.get(0)?,
                title: r.get(1)?,
                session_id: r.get(2)?,
                created: time(&r.get::<_, String>(3)?),
                messages: Vec::new(),
            })
        });
        rows.map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    /// The thread `id` with its messages, oldest first.
    pub fn thread(&self, id: i64) -> Option<ChatThread> {
        let mut thread = self.threads().into_iter().find(|t| t.id == id)?;
        let conn = self.conn();
        let mut statement = conn
            .prepare("SELECT id, role, text, at, done, changes_from, changes_to FROM chat_messages WHERE thread = ?1 ORDER BY id")
            .ok()?;
        let rows = statement
            .query_map([id], |r| {
                Ok(ChatMessage {
                    id: r.get(0)?,
                    thread: id,
                    role: r.get(1)?,
                    text: r.get(2)?,
                    at: time(&r.get::<_, String>(3)?),
                    done: r.get::<_, i64>(4)? != 0,
                    activity: None,
                    changes_from: r.get::<_, i64>(5)? as u64,
                    changes_to: r.get::<_, i64>(6)? as u64,
                })
            })
            .ok()?;
        thread.messages = rows.flatten().collect();
        Some(thread)
    }

    /// Add a message to the thread `thread`.
    pub fn add_message(&self, thread: i64, role: &str, text: &str, done: bool) -> anyhow::Result<ChatMessage> {
        let from = self.last_change();
        let conn = self.conn();
        let at = Local::now();
        conn.execute(
            "INSERT INTO chat_messages (thread, role, text, at, done, changes_from, changes_to) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![thread, role, text, at.to_rfc3339(), done as i64, from as i64],
        )?;
        Ok(ChatMessage {
            id: conn.last_insert_rowid(),
            thread,
            role: role.to_string(),
            text: text.to_string(),
            at,
            done,
            activity: None,
            changes_from: from,
            changes_to: from,
        })
    }

    /// Write the text of `message`. A complete message also gets the
    /// sequence number of the newest change.
    pub fn set_message(&self, message: &mut ChatMessage) -> anyhow::Result<()> {
        if message.done {
            message.changes_to = self.last_change();
        }
        self.conn().execute(
            "UPDATE chat_messages SET text = ?2, done = ?3, changes_to = ?4 WHERE id = ?1",
            params![message.id, message.text, message.done as i64, message.changes_to as i64],
        )?;
        Ok(())
    }

    /// Mark each message that is not complete as complete. A message stays
    /// incomplete when the server stops during a reply.
    pub fn end_open_messages(&self) -> anyhow::Result<()> {
        self.conn().execute("UPDATE chat_messages SET done = 1 WHERE done = 0", [])?;
        Ok(())
    }
}

fn time(text: &str) -> DateTime<Local> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Local))
        .unwrap_or_else(|_| Local::now())
}

fn actor_word(actor: Actor) -> &'static str {
    match actor {
        Actor::User => "user",
        Actor::Triagent => "triagent",
        Actor::Runner => "runner",
    }
}

fn version(conn: &Connection) -> u64 {
    conn.query_row("SELECT value FROM meta WHERE key = 'version'", [], |r| r.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn bump(conn: &Connection) -> rusqlite::Result<()> {
    let next = version(conn) + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [next.to_string()],
    )?;
    Ok(())
}

fn rows(conn: &Connection, kind: &str) -> Vec<(String, String)> {
    let Ok(mut statement) = conn.prepare("SELECT id, json FROM objects WHERE kind = ?1 ORDER BY rowid") else {
        return Vec::new();
    };
    let rows = statement.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?)));
    rows.map(|rows| rows.flatten().collect()).unwrap_or_default()
}

fn all<T: DeserializeOwned>(conn: &Connection, kind: &str) -> Vec<T> {
    rows(conn, kind)
        .into_iter()
        .filter_map(|(id, json)| match serde_json::from_str(&json) {
            Ok(v) => Some(v),
            Err(e) => {
                log::warn!("triage store: the {kind} {id} is not valid: {e}");
                None
            }
        })
        .collect()
}

fn get(conn: &Connection, kind: &str, id: &str) -> rusqlite::Result<Option<Map<String, Value>>> {
    let json: Option<String> = conn
        .query_row("SELECT json FROM objects WHERE kind = ?1 AND id = ?2", [kind, id], |r| r.get(0))
        .optional()?;
    Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
}

fn put(conn: &Connection, kind: &str, id: &str, value: &Value) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO objects (kind, id, json) VALUES (?1, ?2, ?3)
         ON CONFLICT(kind, id) DO UPDATE SET json = excluded.json",
        params![kind, id, value.to_string()],
    )?;
    Ok(())
}

fn delete(conn: &Connection, kind: &str, id: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM objects WHERE kind = ?1 AND id = ?2", [kind, id])?;
    Ok(())
}

fn exists(conn: &Connection, kind: &str, id: &str) -> bool {
    get(conn, kind, id).ok().flatten().is_some()
}

/// True for an id of letters, digits, `.`, `_` and `-`.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 80 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The object `old` with the fields of `set`. A `null` removes the field.
fn merged(old: Option<&Map<String, Value>>, key: &str, id: &str, set: &Map<String, Value>) -> anyhow::Result<Map<String, Value>> {
    let mut new = old.cloned().unwrap_or_default();
    new.insert(key.to_string(), Value::String(id.to_string()));
    for (field, value) in set {
        anyhow::ensure!(field != key, "`set` cannot change `{key}`");
        if value.is_null() {
            new.remove(field);
        } else {
            new.insert(field.clone(), value.clone());
        }
    }
    Ok(new)
}

/// Write the object and one change row for each field that differs.
fn save<T: serde::Serialize>(
    conn: &Connection,
    actor: Actor,
    kind: &str,
    id: &str,
    old: Option<&Map<String, Value>>,
    new: &T,
    now: DateTime<Local>,
) -> anyhow::Result<()> {
    // The JSON of the type, so that the row has no field with a default
    // value.
    let value = serde_json::to_value(new)?;
    let fields = value.as_object().cloned().unwrap_or_default();
    put(conn, kind, id, &value)?;
    let record = |field: &str, old: Option<&Value>, new: Option<&Value>| {
        conn.execute(
            "INSERT INTO changes (at, actor, kind, id, field, old, new) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                now.to_rfc3339(),
                actor_word(actor),
                kind,
                id,
                field,
                old.map(Value::to_string),
                new.map(Value::to_string)
            ],
        )
    };
    match old {
        None => {
            record("", None, Some(&value))?;
        }
        Some(old) => {
            let names: std::collections::BTreeSet<&String> = old.keys().chain(fields.keys()).collect();
            for name in names {
                if old.get(name) != fields.get(name) {
                    record(name, old.get(name), fields.get(name))?;
                }
            }
        }
    }
    Ok(())
}

fn apply_op(conn: &Connection, actor: Actor, op: &Op, now: DateTime<Local>) -> anyhow::Result<()> {
    let triagent = actor == Actor::Triagent;
    match op {
        Op::Item { id, set } => {
            anyhow::ensure!(valid_id(id), "the item id `{id}` is not valid: use letters, digits, `.`, `_` and `-`");
            let old = get(conn, ITEM, id)?;
            let mut fields = merged(old.as_ref(), "id", id, set)?;
            if old.is_none() && !fields.contains_key("created") {
                fields.insert("created".to_string(), Value::String(now.to_rfc3339()));
            }
            let item: Item = serde_json::from_value(Value::Object(fields)).context("the item is not valid")?;
            anyhow::ensure!(!item.text.trim().is_empty(), "`text` is empty");
            if item.impact <= Impact::High && item.status == Status::Open {
                anyhow::ensure!(!item.why.trim().is_empty(), "a critical or high item needs `why`");
            }
            if let Some(topic) = &item.topic {
                anyhow::ensure!(item.chain.is_none(), "an item with `chain` takes the topic of its session: remove `topic`");
                anyhow::ensure!(exists(conn, TOPIC, topic), "no topic `{topic}`: make the topic first");
            }
            let specs = [item.check.as_ref(), item.before.as_ref().and_then(|b| b.check.as_ref())];
            for spec in specs.into_iter().flatten() {
                anyhow::ensure!(valid_script_name(&spec.script), "`{}` is not a valid script name", spec.script);
            }
            if triagent {
                let was = old.as_ref().and_then(|o| o.get("status")).and_then(Value::as_str);
                let confirmed = item.status == Status::Confirmed;
                anyhow::ensure!(
                    confirmed == (was == Some("confirmed")),
                    "only the user sets or clears the status `confirmed`: use `claimed` and write the evidence in `note`"
                );
                anyhow::ensure!(
                    !(item.owner == Owner::User && item.status == Status::Claimed),
                    "an item of the user cannot be `claimed`: give it a `check`, or the user confirms it"
                );
            }
            save(conn, actor, ITEM, id, old.as_ref(), &item, now)
        }
        Op::DeleteItem { id } => {
            anyhow::ensure!(!triagent, "only the user deletes an item: set the status `dropped` and give the reason in `note`");
            let old = get(conn, ITEM, id)?.with_context(|| format!("no item `{id}`"))?;
            delete(conn, ITEM, id)?;
            delete(conn, RESULT, id)?;
            conn.execute(
                "INSERT INTO changes (at, actor, kind, id, field, old) VALUES (?1, ?2, ?3, ?4, '', ?5)",
                params![now.to_rfc3339(), actor_word(actor), ITEM, id, Value::Object(old).to_string()],
            )?;
            Ok(())
        }
        Op::Session { chain, set } => {
            anyhow::ensure!(!chain.trim().is_empty(), "`chain` is empty");
            let old = get(conn, SESSION, chain)?;
            let fields = merged(old.as_ref(), "chain", chain, set)?;
            let note: SessionNote = serde_json::from_value(Value::Object(fields)).context("the session note is not valid")?;
            if let Some(topic) = &note.topic {
                anyhow::ensure!(exists(conn, TOPIC, topic), "no topic `{topic}`: make the topic first");
            }
            if triagent {
                let was = old.as_ref().and_then(|o| o.get("muted_turn")).and_then(Value::as_u64);
                anyhow::ensure!(note.muted_turn.map(|t| t as u64) == was, "only the user changes `muted_turn`");
            }
            save(conn, actor, SESSION, chain, old.as_ref(), &note, now)
        }
        Op::Topic { id, set } => {
            anyhow::ensure!(
                valid_id(id) && !id.chars().any(|c| c.is_ascii_uppercase()),
                "the topic id `{id}` is not valid: use lower case letters, digits and `-`"
            );
            let old = get(conn, TOPIC, id)?;
            let fields = merged(old.as_ref(), "id", id, set)?;
            let topic: Topic = serde_json::from_value(Value::Object(fields)).context("the topic is not valid")?;
            anyhow::ensure!(!topic.title.trim().is_empty(), "`title` is empty");
            save(conn, actor, TOPIC, id, old.as_ref(), &topic, now)
        }
        Op::DeleteTopic { id } => {
            let old = get(conn, TOPIC, id)?.with_context(|| format!("no topic `{id}`"))?;
            let used = all::<SessionNote>(conn, SESSION).iter().any(|s| s.topic.as_deref() == Some(id))
                || all::<Item>(conn, ITEM).iter().any(|i| i.topic.as_deref() == Some(id));
            anyhow::ensure!(!used, "a session or an item uses the topic `{id}`");
            delete(conn, TOPIC, id)?;
            conn.execute(
                "INSERT INTO changes (at, actor, kind, id, field, old) VALUES (?1, ?2, ?3, ?4, '', ?5)",
                params![now.to_rfc3339(), actor_word(actor), TOPIC, id, Value::Object(old).to_string()],
            )?;
            Ok(())
        }
    }
}

// ── The files of the earlier format ─────────────────────────────────────────

/// Read the files of the earlier format in `dir` into the new database:
/// `items.json`, `results.json` and `agents/<chain>.md`. Then move the files
/// to `<dir>.legacy`, so triagent does not read them. It returns a
/// report, or None when `dir` has no such file.
///
/// A follow-up of an agent file becomes an item of the user with a
/// `prompt`. `hidden` becomes `muted_turn`. `top.md` and `resolved` have no
/// equivalent: the priority order replaces them.
fn import_legacy(store: &TriageStore, dir: &Path) -> anyhow::Result<Option<String>> {
    let items_path = dir.join("items.json");
    let agents_dir = dir.join("agents");
    if !items_path.exists() && !agents_dir.exists() {
        return Ok(None);
    }
    // The import writes no change rows: the user did not see the old data
    // as changes.
    let conn = store.conn();
    let now = Local::now();
    let mut items = 0;
    let mut skipped = Vec::new();
    let mut imported = std::collections::HashMap::new();
    if let Ok(bytes) = std::fs::read(&items_path) {
        let file: Value = serde_json::from_slice(&bytes).context("items.json")?;
        for value in file["items"].as_array().into_iter().flatten() {
            let id = value["id"].as_str().unwrap_or_default().to_string();
            match serde_json::from_value::<Item>(value.clone()) {
                Ok(item) if valid_id(&id) => {
                    put(&conn, ITEM, &id, &serde_json::to_value(&item)?)?;
                    imported.insert(id.clone(), item);
                    items += 1;
                }
                Ok(_) => skipped.push(format!("{id}: the id is not valid")),
                Err(e) => skipped.push(format!("{id}: {e}")),
            }
        }
    }
    let mut results = 0;
    if let Ok(bytes) = std::fs::read(dir.join("results.json")) {
        let file: Results = serde_json::from_slice(&bytes).context("results.json")?;
        for (id, result) in file {
            put(&conn, RESULT, &id, &serde_json::to_value(result)?)?;
            results += 1;
        }
    }
    let mut sessions = 0;
    let mut follow_ups = 0;
    let mut names: Vec<PathBuf> = std::fs::read_dir(&agents_dir)
        .map(|read| read.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    names.sort();
    for path in names {
        if path.extension().is_none_or(|x| x != "md") {
            continue;
        }
        let Some(chain) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let created = std::fs::metadata(&path).and_then(|m| m.modified()).map(DateTime::<Local>::from).unwrap_or(now);
        let agent = parse_agent_file(&text);
        let note = SessionNote {
            chain: chain.clone(),
            topic: None,
            turn: agent.turn,
            summary: String::new(),
            muted_turn: agent.hidden,
        };
        put(&conn, SESSION, &chain, &serde_json::to_value(&note)?)?;
        sessions += 1;
        for (number, description, prompt) in agent.follow_ups {
            let id = format!("{}-f{number}", &chain[..8.min(chain.len())]);
            if !valid_id(&id) || exists(&conn, ITEM, &id) {
                continue;
            }
            // `<title> (<item id>) — <reason>`
            let (title, reason) = description.split_once(" — ").unwrap_or((&description, ""));
            let related = title
                .rsplit_once('(')
                .and_then(|(_, rest)| rest.strip_suffix(')'))
                .filter(|id| imported.contains_key(*id));
            // A follow-up for an open item of the user in the same session
            // is the same work. The item gets the prompt of the follow-up.
            let same = related.and_then(|id| imported.get_mut(id)).filter(|i| {
                i.owner == Owner::User
                    && i.status == Status::Open
                    && i.prompt.is_empty()
                    && i.chain.as_deref() == Some(chain.as_str())
            });
            if let Some(item) = same {
                item.prompt = prompt;
                put(&conn, ITEM, &item.id, &serde_json::to_value(&*item)?)?;
                follow_ups += 1;
                continue;
            }
            let impact = related.and_then(|id| imported.get(id)).map_or(Impact::Normal, |i| i.impact);
            let mut why = reason.to_string();
            if why.is_empty() && impact <= Impact::High {
                why = format!("See the item {}.", related.unwrap_or_default());
            }
            let item = Item {
                id: id.clone(),
                chain: Some(chain.clone()),
                topic: None,
                owner: Owner::User,
                text: title.trim().to_string(),
                impact,
                why,
                done_when: String::new(),
                check: None,
                due: None,
                before: None,
                status: Status::Open,
                note: String::new(),
                prompt,
                refs: Vec::new(),
                created: Some(created),
            };
            put(&conn, ITEM, &id, &serde_json::to_value(&item)?)?;
            follow_ups += 1;
        }
    }
    bump(&conn)?;
    drop(conn);
    let legacy = sibling(dir, "legacy");
    std::fs::create_dir_all(&legacy)?;
    for name in ["items.json", "results.json", "top.md", "agents"] {
        let from = dir.join(name);
        if from.exists() {
            std::fs::rename(&from, legacy.join(name)).with_context(|| format!("cannot move {name}"))?;
        }
    }
    for line in &skipped {
        log::warn!("triage import: skipped the item {line}");
    }
    Ok(Some(format!(
        "imported {items} items, {results} check results, {sessions} sessions and {follow_ups} follow-ups. \
         {} items were not valid. The old files are in {}",
        skipped.len(),
        legacy.display()
    )))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct AgentFile {
    turn: Option<usize>,
    hidden: Option<usize>,
    /// The number, the description and the prompt of each follow-up.
    follow_ups: Vec<(usize, String, String)>,
}

/// Read an agent file of the earlier format: a front matter with `turn` and
/// `hidden`, then a numbered list of follow-ups. The lines that start with
/// `>` after a follow-up are its prompt.
fn parse_agent_file(text: &str) -> AgentFile {
    let mut file = AgentFile::default();
    let mut lines = text.lines();
    if lines.next().map(str::trim) == Some("---") {
        for line in lines.by_ref() {
            if line.trim() == "---" {
                break;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().parse().ok();
            match key.trim() {
                "turn" => file.turn = value,
                "hidden" => file.hidden = value,
                _ => {}
            }
        }
    } else {
        lines = text.lines();
    }
    for line in lines {
        let trimmed = line.trim_start();
        if let Some(prompt) = trimmed.strip_prefix('>') {
            if let Some(last) = file.follow_ups.last_mut() {
                if !last.2.is_empty() {
                    last.2.push('\n');
                }
                last.2.push_str(prompt.strip_prefix(' ').unwrap_or(prompt));
            }
            continue;
        }
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 {
            if let Some(description) = trimmed[digits..].strip_prefix(". ") {
                if let Ok(number) = trimmed[..digits].parse() {
                    file.follow_ups.push((number, description.trim().to_string(), String::new()));
                }
            }
        }
    }
    file
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn batch(ops: Value) -> Batch {
        serde_json::from_value(json!({ "ops": ops })).unwrap()
    }

    fn item_op(id: &str, set: Value) -> Value {
        json!({"op": "item", "id": id, "set": set})
    }

    #[test]
    fn an_operation_that_is_not_valid_changes_nothing() {
        let store = TriageStore::in_memory().unwrap();
        let result = store.apply(
            Actor::Triagent,
            &batch(json!([
                item_op("a", json!({"owner": "agent", "text": "Fix it", "impact": "normal"})),
                item_op("b", json!({"owner": "agent", "text": "Fix it", "impact": "critical"})),
                item_op("c", json!({"owner": "agent", "text": "Fix it", "impact": "low", "colour": "red"})),
            ])),
        );
        assert_eq!(result.errors[0], None);
        assert!(result.errors[1].as_ref().unwrap().contains("why"));
        assert!(result.errors[2].as_ref().unwrap().contains("not valid"));
        let ids: Vec<String> = store.items().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, ["a"]);
        assert_eq!(result.version, 1);
    }

    #[test]
    fn only_the_user_confirms_mutes_and_deletes() {
        let store = TriageStore::in_memory().unwrap();
        let make = batch(json!([
            item_op("a", json!({"owner": "user", "text": "Deploy", "impact": "normal"})),
            {"op": "session", "chain": "c1", "set": {"turn": 3}},
        ]));
        assert_eq!(store.apply(Actor::Triagent, &make).errors, [None, None]);
        let user_only = batch(json!([
            item_op("a", json!({"status": "confirmed"})),
            {"op": "session", "chain": "c1", "set": {"muted_turn": 3}},
            {"op": "delete_item", "id": "a"},
        ]));
        let refused = store.apply(Actor::Triagent, &user_only);
        assert!(refused.errors.iter().all(Option::is_some), "{:?}", refused.errors);
        assert_eq!(store.apply(Actor::User, &batch(json!([item_op("a", json!({"status": "confirmed"}))]))).errors, [None]);
        // The agent can change other fields of a confirmed item, but not
        // its status.
        assert_eq!(store.apply(Actor::Triagent, &batch(json!([item_op("a", json!({"note": "n"}))]))).errors, [None]);
        assert!(store.apply(Actor::Triagent, &batch(json!([item_op("a", json!({"status": "open"}))]))).errors[0].is_some());
        assert_eq!(store.apply(Actor::User, &user_only).errors[1..], [None, None]);
        assert!(store.items().is_empty());
    }

    #[test]
    fn a_change_row_names_each_field_that_changed() {
        let store = TriageStore::in_memory().unwrap();
        store.apply(Actor::Triagent, &batch(json!([item_op("a", json!({"owner": "agent", "text": "Fix it", "impact": "low"}))])));
        let before = store.last_change();
        store.apply(Actor::User, &batch(json!([item_op("a", json!({"text": "Fix it now", "impact": "low", "note": "n"}))])));
        let changes = store.changes_after(before, 10);
        let fields: Vec<&str> = changes.iter().map(|c| c.field.as_str()).collect();
        assert_eq!(fields, ["note", "text"]);
        assert_eq!(changes[1].old, Some(json!("Fix it")));
        assert_eq!(changes[1].actor, Actor::User);
        // A `null` removes the field.
        store.apply(Actor::User, &batch(json!([item_op("a", json!({"note": null}))])));
        assert_eq!(store.items()[0].note, "");
    }

    #[test]
    fn a_topic_must_exist_and_cannot_go_while_in_use() {
        let store = TriageStore::in_memory().unwrap();
        let session = json!({"op": "session", "chain": "c1", "set": {"topic": "deploy"}});
        assert!(store.apply(Actor::Triagent, &batch(json!([session.clone()]))).errors[0].is_some());
        let topic = json!({"op": "topic", "id": "deploy", "set": {"title": "Deploy"}});
        assert_eq!(store.apply(Actor::Triagent, &batch(json!([topic, session]))).errors, [None, None]);
        let delete = batch(json!([{"op": "delete_topic", "id": "deploy"}]));
        assert!(store.apply(Actor::Triagent, &delete).errors[0].is_some());
        store.apply(Actor::Triagent, &batch(json!([{"op": "session", "chain": "c1", "set": {"topic": null}}])));
        assert_eq!(store.apply(Actor::Triagent, &delete).errors, [None]);
    }

    #[test]
    fn the_agent_file_of_the_earlier_format() {
        let text = "---\nturn: 7\nresolved:\nhidden: 7\n---\n## Follow-ups\n\
            1. Enter the password (blob-fix) — Thomas deployed prod\n   > I entered the password.\n   > Finish the build.\n\
            2. Check #1044\n   > Check the tables.\n";
        let file = parse_agent_file(text);
        assert_eq!(file.turn, Some(7));
        assert_eq!(file.hidden, Some(7));
        assert_eq!(
            file.follow_ups,
            [
                (1, "Enter the password (blob-fix) — Thomas deployed prod".to_string(), "I entered the password.\nFinish the build.".to_string()),
                (2, "Check #1044".to_string(), "Check the tables.".to_string()),
            ]
        );
    }

    #[test]
    fn the_import_reads_the_files_once_and_moves_them() {
        let root = std::env::temp_dir().join(format!("recap-triage-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("triage");
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        std::fs::write(
            dir.join("items.json"),
            r#"{"items":[{"id":"blob-fix","chain":"c1","owner":"agent","text":"Fix blob","impact":"critical","why":"Prod aborts","status":"open"}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("agents/c1.md"),
            "---\nturn: 7\nresolved:\nhidden:\n---\n## Follow-ups\n1. Enter the password (blob-fix)\n   > Done.\n",
        )
        .unwrap();
        let store = TriageStore::open(&dir).unwrap();
        let items = store.items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].id, "c1-f1");
        assert_eq!(items[1].impact, Impact::Critical);
        assert_eq!(items[1].prompt, "Done.");
        assert_eq!(store.sessions()[0].turn, Some(7));
        assert!(!dir.join("items.json").exists());
        assert!(root.join("triage.legacy/agents/c1.md").exists());
        drop(store);
        // A second open reads the database and imports nothing.
        assert_eq!(TriageStore::open(&dir).unwrap().items().len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }
}
