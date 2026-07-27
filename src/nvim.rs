//! Loader for Neovim undofiles.
//!
//! Each file in `undodir` is a binary undo tree for one tracked file. We
//! parse the tree, verify the on-disk file's SHA-256 still matches the hash
//! stored in the header, then walk the tree from the current state back
//! along `uh_prev` and emit one [`HistoryEntry`] per save checkpoint
//! (`uh_save_nr` set). For each emitted entry we reconstruct the file's
//! contents at that save point and write it to
//! `~/.cache/recap/nvim/<encoded_path>/<save_nr>`.
//!
//! Known limitations:
//! - Format pinned to UF_VERSION = 3 (current Neovim).
//! - Undo branches (`uh_alt_*`) are ignored — only the linear `uh_prev`
//!   chain is walked.
//! - Files whose on-disk content has diverged from the undofile (hash
//!   mismatch) are skipped.
//! - Files renamed or deleted since the last save are skipped.

use anyhow::{bail, Context, Result};
use chrono::{Local, TimeZone};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::history::{find_project_chain, EditSource, HistoryEntry, HistoryEntryKind};

const UF_START_MAGIC: &[u8] = b"Vim\x9FUnDo\xE5";
const UF_VERSION: u16 = 3;
const UNDO_HASH_SIZE: usize = 32;
const NMARKS: usize = 26;

const UF_HEADER_MAGIC: u16 = 0x5fd0;
const UF_HEADER_END_MAGIC: u16 = 0xe7aa;
const UF_ENTRY_MAGIC: u16 = 0xf518;
const UF_ENTRY_END_MAGIC: u16 = 0x3581;

#[allow(dead_code)]
const UF_LAST_SAVE_NR: u8 = 1;
const UHP_SAVE_NR: u8 = 1;

pub fn undodir() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("NVIM_UNDODIR") {
        let p = PathBuf::from(custom);
        return p.exists().then_some(p);
    }
    if let Some(p) = query_undodir_from_nvim() {
        return Some(p);
    }
    let home = std::env::var("HOME").ok()?;
    let xdg = std::env::var("XDG_DATA_HOME").ok();
    let candidates = [
        xdg.as_ref().map(|x| PathBuf::from(x).join("nvim/undo")),
        Some(PathBuf::from(&home).join(".local/share/nvim/undo")),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}

/// Spawn `nvim --headless` and read back its configured `&undodir`. Honors
/// the user's init.lua / init.vim, so a custom `set undodir=...` is picked
/// up automatically. Returns the first listed entry that exists on disk.
fn query_undodir_from_nvim() -> Option<PathBuf> {
    let output = std::process::Command::new("nvim")
        .args([
            "--headless",
            "-c",
            "echo 'RECAP_UNDODIR<' . &undodir . '>'",
            "-c",
            "qa!",
        ])
        .output()
        .ok()?;
    let mut combined = String::from_utf8_lossy(&output.stderr).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    for entry in parse_undodir_entries(&combined) {
        let expanded = expand_tilde(&entry);
        if expanded.is_dir() {
            return Some(expanded);
        }
    }
    None
}

/// Pull the `RECAP_UNDODIR<...>` marker out of nvim's combined output and
/// split the comma list, stripping trailing slashes (vim treats a trailing
/// `//` as "use full-path encoding" — unrelated to the directory itself)
/// and dropping `.` entries (those mean "next to the file").
fn parse_undodir_entries(combined: &str) -> Vec<String> {
    let Some(start) = combined.find("RECAP_UNDODIR<") else {
        return Vec::new();
    };
    let after = &combined[start + "RECAP_UNDODIR<".len()..];
    let Some(end) = after.find('>') else {
        return Vec::new();
    };
    after[..end]
        .split(',')
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty() && s != ".")
        .collect()
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    if s == "~"
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home);
    }
    PathBuf::from(s)
}

fn cache_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/recap/nvim")
}

/// Scan the Neovim undodir, emitting one batch per undofile via `on_batch`.
/// Undofiles are processed newest-first (sorted by mtime descending).
/// `on_batch` returning `false` aborts the scan.
pub fn load(
    undodir: &Path,
    mtimes: &mut HashMap<PathBuf, SystemTime>,
    seen_keys: &mut HashSet<PathBuf>,
    on_batch: &mut dyn FnMut(Vec<HistoryEntry>) -> bool,
) -> Result<()> {
    let read = match fs::read_dir(undodir) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };

    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    for f in read.flatten() {
        let p = f.path();
        if !p.is_file() {
            continue;
        }
        let Some(mtime) = fs::metadata(&p).ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        if let Some(prev) = mtimes.get(&p).copied()
            && mtime <= prev
        {
            continue;
        }
        files.push((p, mtime));
    }
    files.sort_by(|a, b| b.1.cmp(&a.1));

    let mut chain_cache: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for (path, mtime) in files {
        let mut batch: Vec<HistoryEntry> = Vec::new();
        if let Err(e) = process_undofile(&path, &mut batch, &mut chain_cache, seen_keys) {
            eprintln!("recap: skipped undofile {}: {e}", path.display());
        }
        mtimes.insert(path, mtime);
        if !batch.is_empty() && !on_batch(batch) {
            return Ok(());
        }
    }

    Ok(())
}

fn process_undofile(
    undofile_path: &Path,
    out: &mut Vec<HistoryEntry>,
    chain_cache: &mut HashMap<PathBuf, Vec<PathBuf>>,
    seen_keys: &mut HashSet<PathBuf>,
) -> Result<()> {
    let undofile_name = undofile_path
        .file_name()
        .and_then(|s| s.to_str())
        .context("undofile has non-utf8 name")?;
    let file_path = decode_undofile_name(undofile_name)
        .context("could not decode undofile name to a file path")?;

    let on_disk = match fs::read(&file_path) {
        Ok(b) => b,
        Err(_) => return Ok(()),
    };

    let bytes = fs::read(undofile_path)?;
    let parsed = parse_undofile(&bytes)?;

    let actual_hash = compute_buffer_hash(&on_disk);
    if actual_hash != parsed.file_hash {
        // File diverged from undofile (edited outside neovim) — silently skip.
        return Ok(());
    }

    let seq_to_idx: HashMap<i32, usize> = parsed
        .headers
        .iter()
        .enumerate()
        .map(|(i, h)| (h.seq, i))
        .collect();

    let Some(&head_idx) = seq_to_idx.get(&parsed.cur_seq) else {
        return Ok(());
    };

    let project_roots = chain_cache
        .entry(file_path.clone())
        .or_insert_with(|| find_project_chain(&file_path))
        .clone();

    let trailing_newline = on_disk.last() == Some(&b'\n');
    let mut state: Vec<Vec<u8>> = split_lines(&on_disk, trailing_newline);

    let cache_dir = cache_root().join(undofile_name);

    let mut cur = head_idx;
    loop {
        let header = &parsed.headers[cur];
        if let Some(save_nr) = header.save_nr
            && let Some(snapshot_path) = write_snapshot(&cache_dir, save_nr, &state, trailing_newline)
            && !seen_keys.contains(&snapshot_path)
        {
            let timestamp = Local
                .timestamp_opt(header.timestamp, 0)
                .single()
                .unwrap_or_else(Local::now);
            seen_keys.insert(snapshot_path.clone());
            out.push(HistoryEntry {
                timestamp,
                file_path: file_path.clone(),
                snapshot_path,
                project_roots: project_roots.clone(),
                source: EditSource::Neovim,
                kind: HistoryEntryKind::FileChange,
                session_id: None,
                session_cwd: None,
            });
        }

        // Apply this header's entries to roll the buffer one step backward.
        for entry in &header.entries {
            apply_undo_entry(&mut state, entry);
        }

        let parent = header.parent_seq;
        if parent <= 0 {
            break;
        }
        let Some(&idx) = seq_to_idx.get(&parent) else {
            break;
        };
        cur = idx;
    }

    Ok(())
}

fn apply_undo_entry(state: &mut Vec<Vec<u8>>, entry: &UndoEntry) {
    let start = entry.top.max(0) as usize;
    // `ue_bot == 0` is vim's sentinel for "change extends to end of buffer".
    // Otherwise `bot` is the line BELOW the changed region (1-based), so the
    // 0-based half-open replacement range is `[top, bot-1)`.
    let end_excl = if entry.bot == 0 {
        state.len()
    } else {
        (entry.bot - 1).max(0) as usize
    };
    let end_excl = end_excl.min(state.len());
    let start = start.min(end_excl);
    state.splice(start..end_excl, entry.lines.iter().cloned());
}

fn write_snapshot(
    cache_dir: &Path,
    save_nr: i32,
    state: &[Vec<u8>],
    trailing_newline: bool,
) -> Option<PathBuf> {
    let _ = fs::create_dir_all(cache_dir);
    let path = cache_dir.join(save_nr.to_string());
    if path.exists() {
        return Some(path);
    }
    let mut buf: Vec<u8> = Vec::new();
    for (i, line) in state.iter().enumerate() {
        if i > 0 {
            buf.push(b'\n');
        }
        buf.extend_from_slice(line);
    }
    if trailing_newline && !state.is_empty() {
        buf.push(b'\n');
    }
    if let Err(e) = fs::write(&path, &buf) {
        eprintln!("recap: nvim snapshot write failed: {e}");
        return None;
    }
    Some(path)
}

/// Replicate neovim's `u_compute_hash`: SHA-256 over each buffer line
/// followed by a NUL terminator, NOT over the raw file bytes. An empty
/// buffer still has one empty line (`ml_line_count >= 1`).
fn compute_buffer_hash(file_bytes: &[u8]) -> [u8; UNDO_HASH_SIZE] {
    let mut hasher = Sha256::new();
    if file_bytes.is_empty() {
        hasher.update(b"\0");
    } else {
        let trimmed = if file_bytes.last() == Some(&b'\n') {
            &file_bytes[..file_bytes.len() - 1]
        } else {
            file_bytes
        };
        for line in trimmed.split(|b| *b == b'\n') {
            hasher.update(line);
            hasher.update(b"\0");
        }
    }
    hasher.finalize().into()
}

fn split_lines(bytes: &[u8], trailing_newline: bool) -> Vec<Vec<u8>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let trimmed = if trailing_newline {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    trimmed.split(|b| *b == b'\n').map(|s| s.to_vec()).collect()
}

/// Reverse of neovim's `u_get_undo_file_name`: replaces `%` with `/`.
/// Neovim escapes `%` in source paths to `%%`, so we collapse those first.
fn decode_undofile_name(name: &str) -> Option<PathBuf> {
    // Strip leading dot/percent variants neovim sometimes uses; in the
    // standard undodir layout the filename is the absolute path with `/`
    // replaced by `%` (and a literal `%` in the source path is doubled).
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if chars.peek() == Some(&'%') {
                chars.next();
                out.push('%');
            } else {
                out.push('/');
            }
        } else {
            out.push(c);
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(PathBuf::from(out))
}

// ── Binary parsing ──────────────────────────────────────────────────────────

struct ParsedUndo {
    file_hash: [u8; UNDO_HASH_SIZE],
    cur_seq: i32,
    headers: Vec<UndoHeader>,
}

struct UndoHeader {
    seq: i32,
    /// Sequence number of the previous (older) header along the undo chain.
    /// Despite vim naming the field `uh_next`, in neovim's tree this is the
    /// OLDER neighbor (`u_doit` walks `uh_next` when performing an undo);
    /// `uh_prev` points the other way.
    parent_seq: i32,
    timestamp: i64,
    save_nr: Option<i32>,
    entries: Vec<UndoEntry>,
}

struct UndoEntry {
    top: i32,
    bot: i32,
    #[allow(dead_code)]
    lcount: i32,
    lines: Vec<Vec<u8>>,
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn at_eof(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            bail!("unexpected EOF (need {n}, have {})", self.remaining());
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_bytes(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16> {
        let b = self.read_bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn read_i32(&mut self) -> Result<i32> {
        let b = self.read_bytes(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_i64(&mut self) -> Result<i64> {
        let b = self.read_bytes(8)?;
        Ok(i64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        if self.remaining() < n {
            bail!("unexpected EOF skipping {n}");
        }
        self.pos += n;
        Ok(())
    }
}

fn parse_undofile(bytes: &[u8]) -> Result<ParsedUndo> {
    let mut r = Reader::new(bytes);

    let magic = r.read_bytes(UF_START_MAGIC.len())?;
    if magic != UF_START_MAGIC {
        bail!("not a neovim undofile (bad magic)");
    }
    let version = r.read_u16()?;
    if version != UF_VERSION {
        bail!("unsupported undofile version {version}");
    }

    let mut file_hash = [0u8; UNDO_HASH_SIZE];
    file_hash.copy_from_slice(r.read_bytes(UNDO_HASH_SIZE)?);

    let _line_count = r.read_i32()?;
    let saved_line_len = r.read_i32()?.max(0) as usize;
    r.skip(saved_line_len)?;
    let _saved_lnum = r.read_i32()?;
    let _saved_col = r.read_i32()?;

    let _old_header_seq = r.read_i32()?;
    let new_header_seq = r.read_i32()?;
    let cur_header_seq = r.read_i32()?;
    let _num_headers = r.read_i32()?;
    let _last_seq = r.read_i32()?;
    let seq_cur = r.read_i32()?;
    let _timestamp = r.read_i64()?;

    // Pick the seq corresponding to the current on-disk buffer. `b_u_seq_cur`
    // is the buffer's active sequence; `b_u_curhead` is NULL (= 0) when the
    // buffer is at the tree head, so fall through to `b_u_newhead` as a last
    // resort.
    let cur_seq = if seq_cur != 0 {
        seq_cur
    } else if cur_header_seq != 0 {
        cur_header_seq
    } else {
        new_header_seq
    };

    // Optional fields, terminated by len == 0 (which IS the header end —
    // no separate UF_HEADER_END_MAGIC sits between optional fields and the
    // first u_header in current neovim).
    skip_optional_fields(&mut r)?;

    let mut headers = Vec::new();
    while !r.at_eof() {
        let m = r.read_u16()?;
        if m == UF_HEADER_END_MAGIC {
            // File trailer after all u_headers.
            break;
        }
        if m != UF_HEADER_MAGIC {
            bail!("expected u_header magic, got 0x{m:04x}");
        }
        headers.push(parse_uhp(&mut r)?);
    }

    Ok(ParsedUndo {
        file_hash,
        cur_seq,
        headers,
    })
}

fn skip_optional_fields(r: &mut Reader) -> Result<()> {
    loop {
        let len = r.read_u8()? as usize;
        if len == 0 {
            return Ok(());
        }
        // Skip type + len bytes of value
        let _what = r.read_u8()?;
        r.skip(len)?;
    }
}

/// Read optional fields, capturing UHP_SAVE_NR if present.
///
/// Modern neovim writes a UHP_SAVE_NR record on every header: the value is
/// `0` for headers that are NOT save points (intermediate undo states),
/// non-zero for headers that were the buffer head when `:w` ran. We collapse
/// the zero case to `None` so callers can use `is_some()` to mean "real save".
fn read_uhp_optional(r: &mut Reader) -> Result<Option<i32>> {
    let mut save_nr = None;
    loop {
        let len = r.read_u8()? as usize;
        if len == 0 {
            return Ok(save_nr);
        }
        let what = r.read_u8()?;
        if what == UHP_SAVE_NR && len == 4 {
            let n = r.read_i32()?;
            if n != 0 {
                save_nr = Some(n);
            }
        } else {
            r.skip(len)?;
        }
    }
}

fn parse_uhp(r: &mut Reader) -> Result<UndoHeader> {
    let next_seq = r.read_i32()?;
    let _prev_seq = r.read_i32()?;
    let _alt_next_seq = r.read_i32()?;
    let _alt_prev_seq = r.read_i32()?;
    let seq = r.read_i32()?;

    // Cursor pos: lnum, col, coladd (3 × i32)
    r.skip(12)?;
    // cursor_vcol
    r.skip(4)?;
    // flags (u16)
    r.skip(2)?;
    // Named marks: NMARKS × pos_T (12 bytes)
    r.skip(NMARKS * 12)?;
    // Visual info: vi_start (12), vi_end (12), vi_mode (4), vi_curswant (4)
    r.skip(32)?;
    // Timestamp
    let timestamp = r.read_i64()?;

    let save_nr = read_uhp_optional(r)?;

    // Read u_entries until UF_ENTRY_END_MAGIC
    let mut entries = Vec::new();
    loop {
        let m = r.read_u16()?;
        if m == UF_ENTRY_END_MAGIC {
            break;
        }
        if m != UF_ENTRY_MAGIC {
            bail!("expected u_entry magic, got 0x{m:04x}");
        }
        entries.push(parse_uep(r)?);
    }

    // Read extmarks until UF_ENTRY_END_MAGIC. Each extmark is a raw C
    // struct dump (ExtmarkSplice / ExtmarkSavePos / etc.) whose size we
    // can't safely derive from the on-disk format, so we skip the whole
    // section by scanning forward for the terminator. The empty-extmarks
    // case (just the terminator, no records) is handled by the early peek.
    let m = r.read_u16()?;
    if m == UF_ENTRY_END_MAGIC {
        // No extmarks — section was just the terminator.
    } else if m == UF_ENTRY_MAGIC {
        skip_extmarks_section(r)?;
    } else {
        bail!("expected extmark magic, got 0x{m:04x}");
    }

    Ok(UndoHeader {
        seq,
        parent_seq: next_seq,
        timestamp,
        save_nr,
        entries,
    })
}

fn parse_uep(r: &mut Reader) -> Result<UndoEntry> {
    let top = r.read_i32()?;
    let bot = r.read_i32()?;
    let lcount = r.read_i32()?;
    let size = r.read_i32()?.max(0) as usize;
    let mut lines = Vec::with_capacity(size);
    for _ in 0..size {
        let len = r.read_i32()?.max(0) as usize;
        let bytes = r.read_bytes(len)?.to_vec();
        lines.push(bytes);
    }
    Ok(UndoEntry {
        top,
        bot,
        lcount,
        lines,
    })
}

/// Skip a non-empty extmark section by scanning byte-wise for the
/// `UF_ENTRY_END_MAGIC` terminator. To reduce false positives from struct
/// data that happens to contain `0x35 0x81`, we only accept the terminator
/// when it's followed by a known next-section magic: another u_header
/// (`UF_HEADER_MAGIC`), the file trailer (`UF_HEADER_END_MAGIC`), or EOF.
///
/// Caller has already consumed the first extmark's `UF_ENTRY_MAGIC`.
fn skip_extmarks_section(r: &mut Reader) -> Result<()> {
    let term = UF_ENTRY_END_MAGIC.to_be_bytes();
    while r.pos + 1 < r.buf.len() {
        if r.buf[r.pos] == term[0] && r.buf[r.pos + 1] == term[1] {
            let after = r.pos + 2;
            if after == r.buf.len() {
                r.pos = after;
                return Ok(());
            }
            if after + 1 < r.buf.len() {
                let next = u16::from_be_bytes([r.buf[after], r.buf[after + 1]]);
                if next == UF_HEADER_MAGIC || next == UF_HEADER_END_MAGIC {
                    r.pos = after;
                    return Ok(());
                }
            }
        }
        r.pos += 1;
    }
    bail!("could not find end of extmarks section");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_undodir_extracts_marker() {
        let out = "some plugin spam\nRECAP_UNDODIR<.,~/.cache/nvim_undo//>\nmore";
        let v = parse_undodir_entries(out);
        assert_eq!(v, vec!["~/.cache/nvim_undo".to_string()]);
    }

    #[test]
    fn parse_undodir_handles_single_entry() {
        let out = "RECAP_UNDODIR</tmp/undo>";
        let v = parse_undodir_entries(out);
        assert_eq!(v, vec!["/tmp/undo".to_string()]);
    }

    #[test]
    fn parse_undodir_missing_marker_is_empty() {
        assert!(parse_undodir_entries("nothing here").is_empty());
    }

    #[test]
    fn decode_simple_path() {
        let p = decode_undofile_name("%Users%foo%bar.txt").unwrap();
        assert_eq!(p, PathBuf::from("/Users/foo/bar.txt"));
    }

    #[test]
    fn decode_doubled_percent() {
        let p = decode_undofile_name("%tmp%a%%b").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/a%b"));
    }

    #[test]
    fn split_lines_basic() {
        let s = b"a\nb\nc";
        assert_eq!(
            split_lines(s, false),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
    }

    #[test]
    fn split_lines_trailing_newline() {
        let s = b"a\nb\n";
        assert_eq!(
            split_lines(s, true),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
    }

    #[test]
    fn apply_undo_replaces_range() {
        let mut state: Vec<Vec<u8>> = vec![
            b"A".to_vec(),
            b"B".to_vec(),
            b"X".to_vec(),
            b"Y".to_vec(),
            b"Z".to_vec(),
            b"E".to_vec(),
        ];
        // Original was A,B,C,D,E ; the edit replaced C,D with X,Y,Z.
        // To undo: top=2, bot=6, lines=[C,D]
        let entry = UndoEntry {
            top: 2,
            bot: 6,
            lcount: 6,
            lines: vec![b"C".to_vec(), b"D".to_vec()],
        };
        apply_undo_entry(&mut state, &entry);
        assert_eq!(
            state,
            vec![
                b"A".to_vec(),
                b"B".to_vec(),
                b"C".to_vec(),
                b"D".to_vec(),
                b"E".to_vec(),
            ]
        );
    }

    #[test]
    fn apply_undo_bot_zero_means_end_of_buffer() {
        // Regression: bot=0 is vim's sentinel for "change extends to end".
        // Walking back from ["a","b","c","d"] after appending "d" should
        // yield ["a","b","c"], not insert at position 0.
        let mut state: Vec<Vec<u8>> = vec![
            b"a".to_vec(),
            b"b".to_vec(),
            b"c".to_vec(),
            b"d".to_vec(),
        ];
        // The two-entry pattern neovim writes for a pure append:
        let e1 = UndoEntry {
            top: 3,
            bot: 0,
            lcount: 0,
            lines: vec![b"".to_vec()],
        };
        let e2 = UndoEntry {
            top: 3,
            bot: 0,
            lcount: 0,
            lines: vec![],
        };
        apply_undo_entry(&mut state, &e1);
        apply_undo_entry(&mut state, &e2);
        assert_eq!(state, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn buffer_hash_matches_per_line_with_nul() {
        // Regression: neovim hashes (line + "\0") for each buffer line, not
        // the raw file bytes. Verify our reproduction matches a known value.
        let bytes = b"first\nsecond\n";
        let h = compute_buffer_hash(bytes);
        let mut expected = sha2::Sha256::new();
        expected.update(b"first\0");
        expected.update(b"second\0");
        let expected: [u8; UNDO_HASH_SIZE] = sha2::Digest::finalize(expected).into();
        assert_eq!(h, expected);
    }

    #[test]
    fn buffer_hash_empty_file_is_one_empty_line() {
        // vim treats an empty buffer as having one empty line.
        let h = compute_buffer_hash(b"");
        let mut expected = sha2::Sha256::new();
        expected.update(b"\0");
        let expected: [u8; UNDO_HASH_SIZE] = sha2::Digest::finalize(expected).into();
        assert_eq!(h, expected);
    }

    #[test]
    fn apply_undo_pure_insertion_undo() {
        // After insertion of X between A and B: A, X, B
        // top=1 (line A above), bot=2 (line B below — but wait, after the
        // insert there's a new line in between, so B is at 1-based index 3
        // and bot=3). Re-do this example properly.
        let mut state: Vec<Vec<u8>> = vec![b"A".to_vec(), b"X".to_vec(), b"B".to_vec()];
        let entry = UndoEntry {
            top: 1,
            bot: 3,
            lcount: 3,
            lines: vec![],
        };
        apply_undo_entry(&mut state, &entry);
        assert_eq!(state, vec![b"A".to_vec(), b"B".to_vec()]);
    }
}
