//! Semantic search support.
//!
//! recap embeds each prompt and each diff with a local model, stores the
//! vectors in sqlite, and ranks entries by cosine similarity against an
//! embedded query. The `F` key in the History panel drives this.
//!
//! The model runs in `llama-server` from llama.cpp. recap starts the server as
//! a child process on the loopback interface when a user first needs it, and
//! stops the server at exit. The server exposes the OpenAI-compatible
//! `/v1/embeddings` route.
//!
//! Four server flags are critical:
//! - `--pooling last`, because Qwen3-Embedding reads the last token. Mean
//!   pooling produces vectors that look valid and that rank badly.
//! - `-ub` and `-b` at least as large as the longest input, because the server
//!   refuses an input that is longer than the physical batch.
//! - `--parallel 1`. The server shares one context across its slots, so four
//!   slots that each hold 4700 tokens overflow an 8192-token context and the
//!   whole request fails with "Context size has been exceeded". One slot gets
//!   the whole context. This costs no speed: measurement shows the server is
//!   token-bound at about 7000 tokens per second, whatever the slot count.
//! - `-ngl 99`, which puts all layers on the Metal GPU.

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Longest text that recap sends for one embedding.
///
/// The bound is a speed decision, not a context limit. The server embeds about
/// 7000 tokens per second, and source code reaches about 3.4 bytes per token,
/// so every 4000 bytes of one entry costs about 0.17 s. A corpus of 40000
/// entries needs about one hour at this bound and about four hours without it.
/// 4000 bytes holds roughly 80 changed lines, which covers most edits
/// completely and covers the start of a large edit.
const MAX_TEXT_BYTES: usize = 4_000;

/// Texts per HTTP request.
pub const BATCH: usize = 16;

/// Prefix that Qwen3-Embedding expects on the query side. The document side
/// stays unchanged; the model is asymmetric.
const QUERY_INSTRUCT: &str =
    "Instruct: Given a search query, retrieve the developer prompts or code changes that answer it\nQuery: ";

/// Which corpus a stored vector belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedKind {
    Prompt = 0,
    Diff = 1,
}

impl EmbedKind {
    fn from_i64(v: i64) -> EmbedKind {
        match v {
            1 => EmbedKind::Diff,
            _ => EmbedKind::Prompt,
        }
    }
}

// ── Configuration ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedConfig {
    /// Path to the `llama-server` binary.
    pub llama_server_bin: PathBuf,
    /// Path to the GGUF model file.
    pub model_path: PathBuf,
    /// Loopback port that recap starts the server on.
    pub port: u16,
    /// Dimensions that recap keeps after Matryoshka truncation.
    pub dims: usize,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            llama_server_bin: PathBuf::from(&home)
                .join(".cache/recap/llamacpp/llama-b10830/llama-server"),
            model_path: PathBuf::from(&home)
                .join(".cache/recap/models/Qwen3-Embedding-0.6B-Q8_0.gguf"),
            port: 8177,
            dims: 512,
        }
    }
}

fn config_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("recap").join("config.json"))
}

impl EmbedConfig {
    /// Read the configuration. Any failure yields the defaults, so a corrupt
    /// file never blocks startup.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let Ok(data) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&data).unwrap_or_default()
    }

    /// Identity of the vector space. The store wipes itself when this string
    /// changes, so recap never compares vectors from two different models.
    /// The text bound belongs here too: it changes what a document says, so a
    /// vector built under another bound is not comparable.
    pub fn space_id(&self) -> String {
        let model = self
            .model_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?");
        format!("{model}@{}/{MAX_TEXT_BYTES}", self.dims)
    }

    /// Message that tells the user how to install a missing part.
    pub fn missing_parts(&self) -> Option<String> {
        if !self.model_path.exists() {
            return Some(format!(
                "model not found at {}. Download it with: huggingface-cli download Qwen/Qwen3-Embedding-0.6B-GGUF Qwen3-Embedding-0.6B-Q8_0.gguf --local-dir {}",
                self.model_path.display(),
                self.model_path.parent().unwrap_or(Path::new(".")).display()
            ));
        }
        if which(&self.llama_server_bin).is_none() {
            return Some(format!(
                "llama-server not found at {}",
                self.llama_server_bin.display()
            ));
        }
        None
    }
}

/// Resolve the server binary. A configured path wins. If that path is absent,
/// this falls back to `llama-server` on PATH, which is where a package manager
/// installs it.
fn which(bin: &Path) -> Option<PathBuf> {
    if bin.components().count() > 1 && bin.exists() {
        return Some(bin.to_path_buf());
    }
    let name = if bin.components().count() > 1 {
        Path::new("llama-server")
    } else {
        bin
    };
    let path = std::env::var("PATH").ok()?;
    path.split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.exists())
}

// ── The server process ───────────────────────────────────────────────────────

/// The `llama-server` child process. Dropping this kills the server, so recap
/// never leaves an orphan that holds the GPU.
pub struct EmbedServer {
    child: Child,
}

impl EmbedServer {
    /// Start the server. Returns as soon as the process spawns; the caller
    /// polls [`EmbedClient::wait_ready`] for the model load.
    pub fn spawn(cfg: &EmbedConfig) -> Result<EmbedServer> {
        let bin = which(&cfg.llama_server_bin)
            .ok_or_else(|| anyhow!("llama-server not found at {}", cfg.llama_server_bin.display()))?;
        let child = Command::new(bin)
            .arg("-m")
            .arg(&cfg.model_path)
            .args(["--embeddings", "--pooling", "last"])
            .args(["-c", "8192", "-ub", "8192", "-b", "8192"])
            .args(["--parallel", "1", "-ngl", "99"])
            .args(["--host", "127.0.0.1", "--port", &cfg.port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("cannot start llama-server")?;
        Ok(EmbedServer { child })
    }
}

impl Drop for EmbedServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── The HTTP client ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedDatum>,
}

#[derive(Deserialize)]
struct EmbedDatum {
    index: usize,
    embedding: Vec<f32>,
}

/// A handle on the running server. Cloneable, so the indexer thread and the
/// UI thread each hold one.
#[derive(Clone)]
pub struct EmbedClient {
    agent: ureq::Agent,
    base: String,
    dims: usize,
}

impl EmbedClient {
    pub fn new(cfg: &EmbedConfig) -> EmbedClient {
        EmbedClient {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(600))
                .build(),
            base: format!("http://127.0.0.1:{}", cfg.port),
            dims: cfg.dims,
        }
    }

    /// Poll `/health` until the server answers or `limit` passes.
    pub fn wait_ready(&self, limit: Duration) -> Result<()> {
        let start = Instant::now();
        let mut last: Option<String> = None;
        while start.elapsed() < limit {
            match self.agent.get(&format!("{}/health", self.base)).call() {
                Ok(r) if r.status() == 200 => return Ok(()),
                Ok(r) => last = Some(format!("health returned {}", r.status())),
                Err(e) => last = Some(e.to_string()),
            }
            std::thread::sleep(Duration::from_millis(400));
        }
        Err(anyhow!(
            "llama-server did not become ready: {}",
            last.unwrap_or_else(|| "no answer".into())
        ))
    }

    /// Embed a batch of document texts. The returned vectors are truncated to
    /// `dims` and normalized.
    pub fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let payload: Vec<&str> = texts.iter().map(|t| truncate(t)).collect();
        self.post(&payload)
    }

    /// Embed one query. Qwen3-Embedding is asymmetric, so the query carries an
    /// instruction prefix that the documents do not.
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let text = format!("{QUERY_INSTRUCT}{query}");
        let mut out = self.post(&[truncate(&text)])?;
        out.pop().ok_or_else(|| anyhow!("empty embedding response"))
    }

    fn post(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({ "input": texts });
        let answer = self
            .agent
            .post(&format!("{}/v1/embeddings", self.base))
            .send_json(body);
        // Report the cause. A bare "request failed" tells the user nothing
        // about whether the server is absent, slow or unhappy with the input.
        let answer = match answer {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(anyhow!("server answered {code}: {}", server_message(&body)));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(anyhow!("cannot reach the server: {t}"));
            }
        };
        let resp: EmbedResponse = answer
            .into_json()
            .context("cannot parse embedding response")?;

        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        for datum in resp.data {
            if datum.index >= out.len() {
                return Err(anyhow!("server returned index {} out of range", datum.index));
            }
            if datum.embedding.len() < self.dims {
                return Err(anyhow!(
                    "server returned {} dimensions, need at least {}",
                    datum.embedding.len(),
                    self.dims
                ));
            }
            let mut v = datum.embedding;
            v.truncate(self.dims);
            normalize(&mut v);
            out[datum.index] = v;
        }
        if out.iter().any(|v| v.is_empty()) {
            return Err(anyhow!("server skipped an input"));
        }
        Ok(out)
    }
}

/// Cut a text at [`MAX_TEXT_BYTES`], on a char boundary.
fn truncate(s: &str) -> &str {
    if s.len() <= MAX_TEXT_BYTES {
        return s;
    }
    let mut end = MAX_TEXT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Pull the message out of an error body from llama-server. The server sends
/// `{"error":{"message":"…"}}`. A body in another shape comes back cut short.
fn server_message(body: &str) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let msg = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());
    let text = msg.unwrap_or_else(|| body.trim().to_string());
    let mut end = 200.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end < text.len() {
        format!("{}…", &text[..end])
    } else {
        text
    }
}

/// Scale a vector to unit length. A zero vector stays as it is.
fn normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine of two normalized vectors, which is their dot product.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

// ── The sqlite store ─────────────────────────────────────────────────────────

/// The vector table at `~/.cache/recap/embeddings.db`.
pub struct EmbedDb {
    conn: Connection,
    dims: usize,
}

fn db_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".cache/recap/embeddings.db"))
}

impl EmbedDb {
    /// Open the store for one vector space. If the stored space differs from
    /// `cfg.space_id()`, this deletes every row first.
    pub fn open(cfg: &EmbedConfig) -> Result<EmbedDb> {
        let path = db_path().ok_or_else(|| anyhow!("no HOME"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS embedding (
                 key  TEXT PRIMARY KEY,
                 kind INTEGER NOT NULL,
                 vec  BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);",
        )?;

        let want = cfg.space_id();
        let have: Option<String> = conn
            .query_row("SELECT v FROM meta WHERE k = 'space'", [], |r| r.get(0))
            .ok();
        if have.as_deref() != Some(want.as_str()) {
            conn.execute("DELETE FROM embedding", [])?;
            conn.execute(
                "INSERT OR REPLACE INTO meta (k, v) VALUES ('space', ?1)",
                [&want],
            )?;
        }
        Ok(EmbedDb { conn, dims: cfg.dims })
    }

    /// Read every vector into memory.
    pub fn load_index(&self) -> Result<EmbedIndex> {
        let mut stmt = self.conn.prepare("SELECT key, kind, vec FROM embedding")?;
        let mut index = EmbedIndex::new(self.dims);
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        for row in rows {
            let (key, kind, blob) = row?;
            if let Some(vec) = decode_vec(&blob, self.dims) {
                index.insert(key, EmbedKind::from_i64(kind), &vec);
            }
        }
        Ok(index)
    }

    /// Keys that the store already holds, so the indexer skips them.
    pub fn keys(&self) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare("SELECT key FROM embedding")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    /// Write one batch inside one transaction, so a restart resumes at the
    /// last complete batch.
    pub fn insert_batch(&mut self, rows: &[(String, EmbedKind, Vec<f32>)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO embedding (key, kind, vec) VALUES (?1, ?2, ?3)",
            )?;
            for (key, kind, vec) in rows {
                stmt.execute(rusqlite::params![key, *kind as i64, encode_vec(vec)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn encode_vec(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn decode_vec(blob: &[u8], dims: usize) -> Option<Vec<f32>> {
    if blob.len() != dims * 4 {
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

// ── The in-memory index ──────────────────────────────────────────────────────

/// Every vector in one flat buffer, with a map from entry key to its offset.
/// 40000 entries at 512 dimensions need 82 MB.
pub struct EmbedIndex {
    dims: usize,
    data: Vec<f32>,
    slots: HashMap<String, (usize, EmbedKind)>,
}

impl EmbedIndex {
    pub fn new(dims: usize) -> EmbedIndex {
        EmbedIndex {
            dims,
            data: Vec::new(),
            slots: HashMap::new(),
        }
    }

    pub fn insert(&mut self, key: String, kind: EmbedKind, vec: &[f32]) {
        if vec.len() != self.dims {
            return;
        }
        match self.slots.get(&key) {
            Some(&(offset, _)) => {
                self.data[offset..offset + self.dims].copy_from_slice(vec);
            }
            None => {
                let offset = self.data.len();
                self.data.extend_from_slice(vec);
                self.slots.insert(key, (offset, kind));
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&[f32]> {
        let &(offset, _) = self.slots.get(key)?;
        Some(&self.data[offset..offset + self.dims])
    }

    pub fn contains(&self, key: &str) -> bool {
        self.slots.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Vectors of one corpus, for the `--index` report.
    pub fn count_of(&self, kind: EmbedKind) -> usize {
        self.slots.values().filter(|(_, k)| *k == kind).count()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_round_trips_through_a_blob() {
        let v: Vec<f32> = (0..8).map(|i| i as f32 * 0.25).collect();
        let blob = encode_vec(&v);
        assert_eq!(decode_vec(&blob, 8), Some(v));
    }

    #[test]
    fn decode_rejects_a_wrong_length() {
        assert_eq!(decode_vec(&[0u8; 12], 8), None);
    }

    #[test]
    fn cosine_of_a_normalized_vector_with_itself_is_one() {
        let mut v = vec![3.0f32, 4.0, 0.0, 12.0];
        normalize(&mut v);
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn truncate_keeps_a_char_boundary() {
        let s = "é".repeat(MAX_TEXT_BYTES);
        let cut = truncate(&s);
        assert!(cut.len() <= MAX_TEXT_BYTES);
        // The slice is valid UTF-8, so this call cannot panic.
        assert!(cut.chars().all(|c| c == 'é'));
    }

    #[test]
    fn index_overwrites_a_key_in_place() {
        let mut idx = EmbedIndex::new(3);
        idx.insert("a".into(), EmbedKind::Prompt, &[1.0, 0.0, 0.0]);
        idx.insert("b".into(), EmbedKind::Diff, &[0.0, 1.0, 0.0]);
        idx.insert("a".into(), EmbedKind::Prompt, &[0.0, 0.0, 1.0]);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.get("a"), Some(&[0.0, 0.0, 1.0][..]));
        assert_eq!(idx.get("b"), Some(&[0.0, 1.0, 0.0][..]));
        assert_eq!(idx.count_of(EmbedKind::Diff), 1);
    }

    #[test]
    fn index_rejects_a_wrong_width() {
        let mut idx = EmbedIndex::new(3);
        idx.insert("a".into(), EmbedKind::Prompt, &[1.0, 0.0]);
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn a_closed_port_reports_the_transport_error() {
        // Port 9 is the discard port. Nothing answers there.
        let cfg = EmbedConfig { port: 9, ..EmbedConfig::default() };
        let err = EmbedClient::new(&cfg).embed_query("x").unwrap_err().to_string();
        assert!(err.contains("cannot reach the server"), "{err}");
    }

    #[test]
    fn server_message_reads_the_error_field() {
        let body = r#"{"error":{"message":"Context size has been exceeded","code":500}}"#;
        assert_eq!(server_message(body), "Context size has been exceeded");
        assert_eq!(server_message("plain text"), "plain text");
    }

    /// Needs a running llama-server. Run it with
    /// `cargo test -- --ignored smoke_embed`.
    #[test]
    #[ignore]
    fn smoke_embed() {
        let cfg = EmbedConfig::load();
        let client = EmbedClient::new(&cfg);
        client.wait_ready(Duration::from_secs(5)).unwrap();
        let docs = client
            .embed_documents(&[
                "the worker retries the HTTP request with exponential backoff".to_string(),
                "recipe for sourdough bread".to_string(),
            ])
            .unwrap();
        let q = client.embed_query("how do i retry a failed network call").unwrap();
        assert_eq!(docs[0].len(), cfg.dims);
        assert!(cosine(&q, &docs[0]) > cosine(&q, &docs[1]));
    }
}
