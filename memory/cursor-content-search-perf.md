---
name: cursor-content-search-perf
description: Why recap's content (f) search prefilters and runs on all cores, and what it costs
metadata:
  type: project
---

The History panel's `f` search builds a diff for every entry the filter exposes,
so its cost grows with the corpus. After the Cursor blob edit source landed
(see [[cursor-agent-attribution]]) the corpus reached 39699 entries and 2.17 GB
of snapshots, and one cold search needed **85 s** (23 s of file reads, 61 s of
`TextDiff`).

`App::fill_content_cache` fixes this with two rules:

- **Prefilter.** An added line belongs to the new snapshot and a removed line
  belongs to the old one. An entry whose two snapshots hold no match cannot
  produce a matching diff line, so it needs no diff. Build the prefilter regex
  with `multi_line(true)`, or a query that starts with `^` matches only the
  first line of the file and the search loses real hits.
- **All cores.** `std::thread::scope` over chunks of the entry list.

Measured after both: cold search **3.1 s**, same 12 matches. A later query costs
about the same (~3 s), because the cache now holds only the entries that
survived a prefilter (5286, not 39699) — the old 0.57 s rescan came from caching
every diff, which also held the whole corpus in memory.

`content_cache_key` must include `base_path` for a blob entry: the snapshot file
name is a content hash, so two edits that end at one content share it while
their diffs differ.
