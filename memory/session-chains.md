---
name: session-chains
description: How a Claude Code compaction appears in the session files, and how recap joins a continued session to its parent (measured 2026-10-16)
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
