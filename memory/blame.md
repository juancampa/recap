---
name: blame
description: How `recap blame` attributes the hunks of a git diff to sessions and prompts, the vote rule, and its limits
metadata:
  type: project
---

`recap blame [--stdin] [--] [<git diff args>]` (`src/blame.rs`,
`run_blame_cli` in `src/main.rs`) prints one JSON object per hunk of a diff.
The diff comes from stdin with `--stdin`, else from `git diff --no-color
<args>` (default `HEAD`). Paths are resolved against `git rev-parse
--show-toplevel`. **Do not test "is stdin a terminal" to pick the source**:
an agent harness runs the command with a stdin that is not a terminal and
never closes, and the read blocked for 20 minutes before the flag existed.

**Vote rule.** For each file in the diff, the candidates are the edit entries
with that absolute `file_path`, newest first. Each candidate gets two sets of
trimmed lines, added and removed, from a **multiset difference** of the two
snapshots (`line_sets_from`), not from a diff: a `similar` diff of two
4700-line files with many changes needs seconds, and 150 of them made the
first run take minutes. A `+` line of the hunk votes for the newest candidate
whose added set holds it; a `-` line votes through the removed set. A line
shorter than 4 characters after trim (`}`, `)`) does not vote. Votes group by
`(session_id, prompt)`, and the group with the most votes is the `winner`.
Ties go to the group with the newest edit.

**Bash fallback.** An agent in bypass-permissions mode edits files with
`python3 - <<EOF` or `sed`, and recap has no snapshot of that. The 2026-09-11
measurement: 122 of 224 hunks in this repo's pending diff had no snapshot
line. `claude::bash_calls` reads every `Bash` tool call of every transcript
(4636 calls, 2.8 MB, under one second), and a line that no snapshot holds
votes for the newest call whose command names the file and holds the line.
The `bash_lines` field of an attribution counts these weaker votes. Two
guards against generic lines (`continue;`, `let mut out = Vec::new();`):
a call must name the file, and a line that calls of two sessions hold does
not vote. A winner also needs a quarter of the voting lines
(`MIN_WINNER_SHARE`); a script that removes code by line number leaves a
hunk with no winner, which is better than a wrong one.

**Exact Claude bases (2026-09-11).** `claude.rs` now writes the content
before each tool call to `<message>-<tool>-base` and sets `base_path`, like
a Cursor blob edit. Before that, an `Edit` whose `old_string` was not in the
replayed state was dropped (10 of 18 edits in one session, because Bash
edits in between made the state stale). Now `apply_edit` seeds the state
again from a `Read` result, the disk, or a stub of `old_string`, and the
diff of the entry stays exact.

**Prompt link.** No edit entry stores its prompt. The prompt of an edit is the
last prompt entry of the same session whose timestamp is not after the edit
(`prompt_of`). This holds for Claude and for Cursor composers alike.

**Limits.**
- A manual edit (Neovim, unclaimed Local History) wins with `session_id: null`.
  A hunk that no entry explains has `winner: null`.
- Untracked files never appear in `git diff`; the caller runs `git add -N`.
- The command loads the whole store first, like `--index`: 45k entries in
  about 7 s, and the whole run of 224 hunks in 11 files took 9 s.
- `Cargo.lock` hunks stay unattributed: cargo writes the file, not a tool
  call.

The slice-commits skill in `mnode/.agents/skills/slice-commits/SKILL.md`
consumes this JSON to group hunks by prompt and to stage partial files with
`git apply --cached` from the `patch` field.
