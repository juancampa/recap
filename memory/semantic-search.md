---
name: semantic-search
description: How recap's `F` semantic search works — the local embedding server, the vector store, and the numbers it runs at
metadata:
  type: project
---

`F` in the History panel ranks entries by cosine similarity. `src/embed.rs`
holds the model server, the HTTP client, the sqlite store and the scorer.

**The model runs locally.** recap starts `llama-server` (llama.cpp) as a child
process on 127.0.0.1:8177 when the user first presses `F`, and kills it on
drop. The model is `Qwen3-Embedding-0.6B-Q8_0.gguf`. Neither the binary nor the
model ships with recap; both live under `~/.cache/recap/`
(`llamacpp/llama-b10830/` and `models/`), and `~/.config/recap/config.json`
overrides the paths, the port and the dimensions. See the
[[llama-cpp-embeddings]] skill for the server flags, which have traps.

**The store key is `content_cache_key(entry)`** — the same identity the `f`
content search uses, so the vector store and the content cache never disagree
about what one entry is. The rows live in `~/.cache/recap/embeddings.db`, and
the `meta.space` row holds `<model>@<dims>/<MAX_TEXT_BYTES>`. A change to any of
those three wipes the table, because a vector built under other settings is not
comparable.

**`MAX_TEXT_BYTES` decides the build time, not the batch size.** The server is
token-bound at about 7000 tokens per second. The corpus of 43301 entries needs
25 minutes at 4000 bytes per entry and about four hours at 16000.

**Search runs on Enter, not per keystroke**, because each query costs an HTTP
round trip. Results come back in score order, which no other recap search does,
so the help bar prints the score and the prompt/edit split.

`recap --index --query "…"` builds the index and ranks a query without the TUI.
`--query` may repeat. A pty test with `pyte` is the way to check the TUI path;
stripping ANSI codes by hand does not reconstruct a ratatui screen.

**`F` needs a phrase; `f` finds a string.** A query of one or two words ranks
short generic prompts at the top and puts the correct prompt near rank 670. See
`docs/semantic-search-plan.md` for the measurement.
