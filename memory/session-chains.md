---
name: session-chains
description: How Claude Code and Cursor compactions appear in the source data, and how recap joins a continued session or a subagent to its parent (measured 2026-10-16)
metadata:
  type: project
---

**Compaction in the same file.** Claude Code writes a `system` line with `subtype: "compact_boundary"`, then a `user` line with `isCompactSummary: true`.
The summary line holds the summary text, and the session id does not change.
On 2026-10-16, 75 of 78 compactions stayed in the same file.

**Compaction in a new file.** A background session (`sessionKind: "bg"`) can continue in a new session file.
The new file starts with a copy of several hundred lines of the old file, with the same `uuid` values.
A copied line keeps the id of the old session in the field `session_id` (snake case).
The field `sessionId` holds the id of the new file.
Some copied lines have no `session_id`, the typed prompts for example.
So `claude::copied_prefix` marks as copied every line up to the last line that names another session.
The loader replays the edits of the copied lines, but emits no entry for them.

**A tree, not a line.** One parent can have more than one child session, and the parent can get new prompts after the split.
The store keeps the links (child → parent), and `Store::chain_id` follows them to the first session.
The Agents panel shows one agent for each session chain, and the History panel lists the prompts of the chain by time.

See [[session-titles]] for the other session facts that the loader reads.

**Cursor.** A Cursor compaction stays in the same composer, so it makes no new session.
Until 2026-01, Cursor wrote the summary into an agent bubble as `conversationSummary` (a JSON string with a `summary` field), and the loader makes a compaction entry from it.
A newer client writes the summary into a binary `agentKv:blob:<sha256>` row, which names no composer. The loader does not read it.

**Subagents.** A Cursor subagent is a separate composer. Its header has `subagentInfo.parentComposerId`.
The store keeps these links apart from the continuation links (`Store::subagent_parents`), and `Store::chain_id` follows both.
The parent agent wrote the prompts of a subagent, so they get no ordinal, do not count in the agent's prompts, and show the `[sub]` label.
recap does not read the Claude Code subagent transcripts (`<session>/subagents/*.jsonl`).
