# Plan: semantic search in recap

recap gets a third History search mode. The `/` key does a fuzzy path search.
The `f` key does a regex content search. The new `F` key does a semantic
search: recap embeds the query, compares it to a stored vector for each entry,
and ranks the entries by cosine similarity.

The work has two phases. Phase 1 covers prompts. Phase 2 covers diffs. Phase 1
builds all of the infrastructure, so phase 2 only adds a second text source and
a larger index build.

## Decisions that are already made

- **Backend**: `llama-server` from llama.cpp, over HTTP on the loopback
  interface. recap starts the server as a child process when a user first
  needs it, and stops it at exit.
- **Model**: `Qwen3-Embedding-0.6B`, quantized to Q8_0, in GGUF format. One
  model serves both phases. The 32k context holds a complete diff, so recap
  does not chunk a diff.
- **Vector size**: 1024 dimensions from the model, truncated to the first 512
  (Matryoshka) and normalized again. This halves the memory and the disk cost
  and keeps almost all of the quality.
- **One index build**: the first build of the diff index needs 10-20 minutes.
  It runs in a background thread and it does not block the UI. Every later
  build is incremental.
- **No vector index**: recap scores all vectors with a brute-force dot
  product. 40000 entries x 512 dimensions is 20 million multiply-add
  operations, which needs about 10 ms on one core. An HNSW index is not
  necessary and the plan does not add one.

## Open item: one new dependency

The HTTP client is the only new crate. I propose `ureq` (blocking API, rustls,
a small dependency tree). recap uses threads and not tokio, so a blocking
client fits the existing design. `reqwest` would pull in tokio.

The alternative is to run `curl` as a child process for every batch. That adds
no crate, but it needs a temporary file for each request body and it costs a
process spawn per batch. I recommend `ureq`. **This needs your approval before
phase 1 starts.**

## New module: `src/embed.rs`

One module holds the server process, the HTTP client, the sqlite store and the
scorer.

### Configuration

recap reads `~/.config/recap/config.json`. This is a new file. It is separate
from `state.json`, because recap overwrites `state.json` at exit and the user
owns the configuration.

```json
{
  "llama_server_bin": "llama-server",
  "model_path": "~/.cache/recap/models/Qwen3-Embedding-0.6B-Q8_0.gguf",
  "port": 8177,
  "dims": 512
}
```

recap does not download the model. If the model file is absent, the search
prompt shows the exact `huggingface-cli download` command.

### The server process

recap starts the server on the first use, not at startup:

```
llama-server -m <model> --embeddings --pooling last \
             -c 8192 -ub 8192 -b 8192 -ngl 99 \
             --port <port> --host 127.0.0.1
```

Three flags are critical:

- `--pooling last`. Qwen3-Embedding uses the last token, not the mean. A wrong
  pooling type produces vectors that look valid and that rank badly.
- `-ub 8192`. llama-server refuses an embedding input that is longer than the
  physical batch size. The error text is "input is too large to process".
- `-ngl 99` puts all layers on the Metal GPU.

recap polls `/health` until the server answers, with a 30 second limit. recap
kills the child at exit. If the spawn fails, recap holds the error text and
each semantic search reports it.

### The HTTP call

`POST /v1/embeddings` with `{"input": [text, ...]}`. The response holds one
`data[].embedding` per input. recap sends 16 texts per request.

recap truncates each text to 30000 bytes before it sends the text. 30000 bytes
is about 8000 tokens, which is the context that the server runs with.

Qwen3-Embedding is asymmetric. recap prefixes the **query** text and sends the
**document** text unchanged:

```
Instruct: Given a search query, retrieve the developer prompts or code
changes that answer it
Query: <what the user typed>
```

### The store

recap keeps the vectors in a sqlite database at
`~/.cache/recap/embeddings.db`, through the `rusqlite` crate that recap already
uses.

```sql
CREATE TABLE embedding (
  key  TEXT PRIMARY KEY,
  kind INTEGER NOT NULL,   -- 0 = prompt, 1 = diff
  vec  BLOB NOT NULL       -- dims * f32, little-endian, L2-normalized
);
CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT);
```

`key` is the string form of `app::content_cache_key(entry)`. That function is
already the identity of an entry's searchable text, so the plan reuses it and
adds no second identity scheme. For a prompt the key is the synthetic
`file_path`. For a file change the key is the snapshot path, prefixed by the
`base_path` when the entry has one.

`meta` holds the model name and the dimension count. If either value differs
from the configuration, recap deletes every row and builds the index again.
This stops recap from comparing vectors from two different models.

### The in-memory index

A background thread reads the whole table into a flat `Vec<f32>` and a
`HashMap<PathBuf, usize>` from key to offset. 40000 rows x 512 f32 is 82 MB.
The thread sends `LoadEvent::EmbedIndex` when it finishes, which follows the
pattern that `LoadEvent::CursorSessions` already uses.

A query then needs no file read: recap looks up each visible entry's key in the
map and scores the vector that the map points at.

## Shared text extraction

`App::fill_content_cache` builds the searchable text of an entry today, inside
a closure, and it applies a regex prefilter. The indexer needs the same text
and no prefilter.

Extract a free function:

```rust
pub fn entry_text(store: &HistoryStore, idx: usize) -> String
```

It returns the prompt text for a prompt entry, and the `+`/`-` diff lines for a
file change. `fill_content_cache` calls it after its prefilter passes. The
indexer calls it directly. One definition of "the searchable text of an entry"
then serves both searches.

## Phase 1 — prompts

The corpus is 4000 to 8000 prompts. No snapshot reads happen, so the first
index build needs less than one minute. This phase proves the design.

### Work items

1. Add `ureq` to `Cargo.toml` (after your approval).
2. Write `src/embed.rs`: the configuration, the server process, the client, the
   sqlite store, the in-memory index and the cosine scorer.
3. Extract `entry_text` out of `App::fill_content_cache`.
4. Add the indexer thread. It walks `store.entries`, keeps every prompt entry
   whose key the index does not hold, embeds the entries in batches of 16, and
   writes one sqlite transaction per batch. It sends
   `LoadEvent::EmbedProgress { done, total }` for the status line, and it runs
   again after a load batch adds entries.
5. Add `SearchKind::Semantic` and the state that it needs: the ranked results
   and the top score.
6. Add `App::search_open_semantic`, and bind `F` in `src/main.rs` next to the
   existing `f` binding, for the History panel.
7. Add the scorer. On Enter, recap embeds the query, scores every entry that
   the current filter exposes, keeps the 50 best results that score within 0.15
   of the best one, and fills `SearchState::matches` in score order.
8. Add the `SearchKind::Semantic` arms in `src/ui.rs`: the prompt prefix `F `,
   the tag ` [semantic]`, the score in the counter, and the index-build
   progress next to the existing `[loading…]` indicator.

### Behavior that differs from `f`

- **The search runs on Enter, not on a keystroke.** A semantic search needs an
  HTTP round trip, so one call per keystroke is pure cost. While the user
  types, the prompt shows the hint `press Enter to search`.
- **The results are in score order, not in time order.** Every other search in
  recap keeps the newest entry first. The prompt shows the rank and the score,
  for example `F auth retry [semantic] (3/50, 0.62)`, so the order is visible.
- **No highlight.** A semantic match has no character positions and no regex,
  so the History row and the Diff panel show no highlighted text. The row
  keeps the match marker that a content match already uses.
- **`n` and `N` work unchanged**, because the plan reuses `SearchState::matches`
  and the existing reveal logic.

### Tests

- Encode a vector to a blob and decode it again.
- Cosine of a vector with itself is 1.0.
- A `meta` row that names another model clears the table.
- Truncation cuts a long text at the byte limit and keeps valid UTF-8.
- An `#[ignore]`d smoke test that hits a running server, which follows the
  `smoke_load_and_attribute` convention in `cursor_sessions.rs`.

## Phase 2 — diffs

### Work items

1. Extend the indexer to file-change entries. It reads the two snapshots,
   calls `entry_text`, truncates and embeds.
2. Add a progress line, because this build needs 10 to 20 minutes. It shows
   `[embedding 12480/39699]`.
3. Make the build resumable. Each batch commits, so a restart continues where
   the last run stopped.
4. Give the indexer a low priority. It reads about 2.2 GB of snapshots, so it
   uses one thread and not all cores, and the interactive `f` search keeps the
   cores that it needs today.

### Scope of the `F` search

`F` searches whatever the current filter exposes, and it mixes prompt rows and
diff rows in one ranked list. The Projects panel and the Files panel already
narrow the corpus, so a separate corpus toggle adds a control that the user
does not need. The prompt shows the split, for example `(50 results: 12
prompts, 38 edits)`.

## Risks

- **Score floor**. Qwen3 cosine scores cluster between 0.3 and 0.8, so an
  absolute floor is fragile. The plan uses top-K plus a relative floor
  (`best - 0.15`). This value needs a test against your real corpus and it may
  change.
- **Memory**. The server holds about 700 MB while recap runs. recap starts the
  server only when the user needs it, so a session that never searches pays
  nothing.
- **A stale key**. If a snapshot file disappears, its row stays in the
  database. The rows are small, so the plan ignores this. A `recap --reindex`
  flag deletes the database if you want a clean build.
- **The first build blocks nothing, but it costs battery**. The build runs on
  the GPU for 10 to 20 minutes. The status line reports the progress, and the
  build stops when recap exits.

## Files that change

| File | Change |
|---|---|
| `Cargo.toml` | Add `ureq`. |
| `src/embed.rs` | New. The configuration, the server, the client, the store, the scorer. |
| `src/main.rs` | Add the `mod embed;` line and the `F` key binding. |
| `src/app.rs` | Add `SearchKind::Semantic`, `search_open_semantic`, the scorer call, `LoadEvent::EmbedIndex` and `LoadEvent::EmbedProgress`, the indexer thread, and the `entry_text` extraction. |
| `src/ui.rs` | Add the `SearchKind::Semantic` arms and the progress indicator. |
| `docs/semantic-search-plan.md` | This file. |

---

# As built (2026-09-06)

Both phases are complete. The measurements below replace the estimates above.

## What changed against the plan

- **`--parallel 1` is required.** The plan named `-ub` as the critical batch
  flag. That is true but not sufficient. `llama-server` starts four slots and
  runs them with `kv_unified = true`, so the slots **share** the `-c` context.
  Four inputs of 4700 tokens each overflow an 8192-token context and the whole
  request fails with `decode() failed: Context size has been exceeded`, even
  though each input fits on its own. One slot costs no speed, because the
  server is token-bound.
- **`MAX_TEXT_BYTES` is 4000, not 30000.** The server embeds about 7000 tokens
  per second, and the batch size does not change that number. So the build time
  follows the text length, not the entry count. 16000 bytes per entry needs
  about four hours for this corpus; 4000 bytes needs 25 minutes. 4000 bytes
  holds about 80 changed lines.
- **The text bound is part of `space_id`.** It changes what a document says, so
  a vector built under another bound is not comparable.
- **The indexer embeds prompts first.** Prompts are short and they carry the
  user's own words, so the useful half of the index is ready in three minutes.
- **`recap --index [--query …]` exists.** It builds the index without the TUI
  and prints the ranked results. `--query` may repeat.

## Measurements on the real corpus

| | |
|---|---|
| Entries | 43301 |
| History load | 58 s |
| Vectors | 39751 (5110 prompts, 34641 edits) |
| Index build | 25.5 minutes |
| Prompt rate | 31 entries per second |
| Diff rate | 15.3 entries per second |
| Database | 82 MB |
| Load into memory | 2 s |
| One query | 41-78 ms |

The vector count is below the entry count because entries that share a
`content_cache_key` collapse to one row. That is the deduplication the key was
chosen for.

## Ranking quality

The model answers questions that a regex cannot. "handle list rows that have
different heights when scrolling" returns the `ratatui-tui` skill file at rank 3
and the layout tests at ranks 1, 2 and 7. "why does the terminal UI flicker or
redraw too often" returns six prompts about flicker that do not share the
query's wording. A correct hit scores 0.70 to 0.74; the next result sits near
0.60.

## Two open tuning points

1. **The floor keeps weak results.** A query with no good answer still fills
   the list, because the floor is relative to the best score. "retry a failed
   network request with exponential backoff" has a best score of 0.579, against
   0.70 for a real hit. An absolute floor near 0.60 would report no match
   instead.
2. **A repeated prompt appears twice.** Two entries whose text is identical
   both rank. A hash of the text at display time would collapse them.

## A short query gives a bad rank

A query of one or two words does not work. The measurement below uses the
prompt "Crazy idea, could the wbg_cast function bodies … be generated with
enough slack …", which the store holds.

| query | rank of that prompt | score |
|---|---|---|
| `Crazy idea` | 670 | 0.43 |
| `crazy idea about wbg_cast nop slack rewriting offsets` | 1 | 0.83 |

The top ten for `Crazy idea` hold short generic prompts: "Nothing to do", "Go
for it", "Hmm doesn't work. Any other ideas?". The median prompt length in the
top 50 is 59 characters against 109 characters for the long query. A dense
vector of two words carries too little information, so the model ranks the
documents that are most similar to a prompt in general.

The `f` regex search finds this prompt at once, because the query is a literal
string. `F` is for a description, `f` is for a string.

Two remedies failed a measurement. Corpus centering moved the prompt only to
rank 250 and made a good query worse. A shorter instruction gave rank 1242.

A third remedy is open: fuse the cosine score with a literal match. recap holds
every prompt text in memory, so a bonus for an entry whose text contains the
query costs nothing for a prompt. A file change would need a file read, so the
bonus would apply to prompts only. This needs a decision, because it makes the
two corpora score by different rules.
