//! The History searches: a fuzzy match on the display path, a regex on the
//! content, and a rank by the embedding similarity. Each search reads only
//! the entries that pass the filter of the client.

use crate::embed::{EmbedIndex, cosine};
use crate::embedder::{diff_text, entry_snapshots, turn_text};
use crate::store::{Store, entry_key};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use recap_api::{EntryFields, EntryId, EntryKind, Filter, SEMANTIC_SCORE_SCALE, SearchMatch};
use std::collections::HashMap;

/// The number of results that a semantic search keeps. The list is ranked,
/// so a long list of weak matches adds noise and no information.
const SEMANTIC_TOP_K: usize = 50;

/// How far below the best score a result can be and still appear. Qwen3
/// cosine scores are mostly between 0.2 and 0.8, so an absolute floor does
/// not work well. The distance to the best result is stable.
const SEMANTIC_REL_FLOOR: f32 = 0.15;

/// The searchable text of each entry, keyed by entry id. A prompt stores its
/// text and its response. A file change stores its `+`/`-` diff lines. The
/// server removes the text of an entry that changes (see
/// [`crate::server`]).
pub type ContentCache = HashMap<EntryId, String>;

/// What one search read, for the log.
#[derive(Debug, Default)]
pub struct SearchScan {
    /// The entries that pass the filter.
    pub candidates: usize,
    /// Content search: the candidates whose text the cache held.
    pub cached: usize,
    /// Content search: the file changes whose two snapshots the search read.
    pub snapshots_read: usize,
    /// Content search: the bytes of those snapshots.
    pub bytes_read: u64,
    /// Semantic search: the candidates without a vector.
    pub without_vector: usize,
}

/// Fuzzy-match the display path of each entry. For a prompt, match only the
/// project chain: the display path of a prompt ends with the whole prompt
/// text, and a short query matches almost every long text. The content
/// search finds prompt text.
pub fn path_matches(store: &Store, filter: &Filter, query: &str) -> (Vec<SearchMatch>, SearchScan) {
    // nucleo's raw `Matcher` needs a lowercase needle when `ignore_case` is
    // on: an uppercase char fails a debug assertion inside the matcher.
    let query = query.to_lowercase();
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut needle_buf: Vec<char> = Vec::new();
    let needle = Utf32Str::new(&query, &mut needle_buf);
    let names = &store.history.project_names;

    let mut hay_buf: Vec<char> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut results: Vec<SearchMatch> = Vec::new();
    let candidates = store.filtered(filter);
    let scan = SearchScan {
        candidates: candidates.len(),
        ..SearchScan::default()
    };
    for idx in candidates {
        let entry = &store.history.entries[idx];
        let s = if !entry.is_file_change() {
            let (chain, _) = entry.display_path_segments(names);
            chain.join(" → ")
        } else {
            entry.display_path(names)
        };
        hay_buf.clear();
        let haystack = Utf32Str::new(&s, &mut hay_buf);
        indices.clear();
        if let Some(score) = matcher.fuzzy_indices(haystack, needle, &mut indices) {
            results.push(SearchMatch {
                id: store.id_at(idx).clone(),
                indices: indices.clone(),
                score,
            });
        }
    }
    // The best score first. A stable sort keeps the newest first on a tie.
    results.sort_by(|a, b| b.score.cmp(&a.score));
    (results, scan)
}

/// Match the query as a case-insensitive regex against the diff body of a
/// file change and the text and response of a prompt. An invalid regex
/// gives no match. The result is newest first.
pub fn content_matches(
    store: &Store,
    cache: &mut ContentCache,
    filter: &Filter,
    query: &str,
) -> (Vec<SearchMatch>, SearchScan) {
    let indices = store.filtered(filter);
    let mut scan = SearchScan {
        candidates: indices.len(),
        ..SearchScan::default()
    };
    let Ok(regex) = recap_api::content_regex(query) else {
        return (Vec::new(), scan);
    };
    fill_content_cache(store, cache, &indices, &mut scan);

    let matches = indices
        .into_iter()
        .filter(|&idx| {
            cache
                .get(store.id_at(idx))
                .is_some_and(|text| regex.is_match(text))
        })
        .map(|idx| SearchMatch {
            id: store.id_at(idx).clone(),
            indices: Vec::new(),
            score: 0,
        })
        .collect();
    (matches, scan)
}

/// Calculate the searchable text of each entry in `indices` that the cache
/// does not hold yet.
///
/// The cache keeps the text of every entry, also of an entry that does not
/// match the query. The text of all entries is approximately 70 MB, but the
/// snapshots that a file change diffs are approximately 2 GB. Thus only the
/// first search reads the snapshots, and the next searches read only memory.
/// The scan runs on all cores.
fn fill_content_cache(
    store: &Store,
    cache: &mut ContentCache,
    indices: &[usize],
    scan: &mut SearchScan,
) {
    let todo: Vec<usize> = indices
        .iter()
        .copied()
        .filter(|&i| !cache.contains_key(store.id_at(i)))
        .collect();
    scan.cached = indices.len() - todo.len();
    if todo.is_empty() {
        return;
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(todo.len());
    let chunk = todo.len().div_ceil(threads);

    let parts: Vec<(Vec<(EntryId, String)>, usize, u64)> = std::thread::scope(|scope| {
        let handles: Vec<_> = todo
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    let mut out: Vec<(EntryId, String)> = Vec::new();
                    let (mut read, mut bytes) = (0usize, 0u64);
                    for &idx in part {
                        let entry = &store.history.entries[idx];
                        let id = store.id_at(idx).clone();
                        match &entry.kind {
                            EntryKind::Prompt { .. } => out.push((id, turn_text(entry))),
                            EntryKind::FileChange => {
                                let (old_content, new_content) = entry_snapshots(store, idx);
                                read += 1;
                                bytes += (old_content.len() + new_content.len()) as u64;
                                out.push((id, diff_text(&old_content, &new_content)));
                            }
                            // The summary of a compaction is not searchable
                            // text (see the embedder jobs).
                            EntryKind::Compaction { .. } => {}
                        }
                    }
                    (out, read, bytes)
                })
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    for (out, read, bytes) in parts {
        cache.extend(out);
        scan.snapshots_read += read;
        scan.bytes_read += bytes;
    }
}

/// Rank the entries that pass the filter by the cosine similarity of their
/// vector to `query`, the embedded query.
pub fn semantic_matches(
    store: &Store,
    index: &EmbedIndex,
    filter: &Filter,
    query: &[f32],
) -> (Vec<SearchMatch>, SearchScan) {
    let mut scored: Vec<(f32, usize)> = Vec::new();
    let candidates = store.filtered(filter);
    let mut scan = SearchScan {
        candidates: candidates.len(),
        ..SearchScan::default()
    };
    for idx in candidates {
        let Some(vec) = index.get(&entry_key(&store.history.entries[idx])) else {
            scan.without_vector += 1;
            continue;
        };
        scored.push((cosine(query, vec), idx));
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));

    let Some(&(best, _)) = scored.first() else {
        return (Vec::new(), scan);
    };
    let matches = scored
        .into_iter()
        .take(SEMANTIC_TOP_K)
        .filter(|(score, _)| *score >= best - SEMANTIC_REL_FLOOR)
        .map(|(score, idx)| SearchMatch {
            id: store.id_at(idx).clone(),
            indices: Vec::new(),
            score: (score.clamp(0.0, 1.0) * SEMANTIC_SCORE_SCALE) as u16,
        })
        .collect();
    (matches, scan)
}
