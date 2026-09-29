---
name: agent-responses
description: Where the final agent response of each turn lives in the Claude Code JSONL and the Cursor DB, the traps in finding it, and the corpus size (measured 2026-09-07)
metadata:
  type: project
---

recap loads the final response of each turn into
`HistoryEntryKind::Prompt.response` (`claude.rs` turn tracking,
`cursor_sessions::attach_responses`). `App::merge_batch` replaces a prompt
that arrives again with a response, so a session that still runs updates its
last prompt. This note records what the raw data holds.

**Claude Code (`~/.claude/projects/*/*.jsonl`).** The final response is
the last `type=="assistant"` line after the prompt that has `text` blocks and
no `tool_use` block. Two traps:

- Claude Code writes one API message as several lines that share
  `message.id`. A `text` line and a `tool_use` line of the same message are
  separate lines (859 of 5253 messages on 2026-09-07). Group by `message.id`
  and reset the candidate when a `tool_use` line arrives.
- Some `type=="user"` lines are not human prompts: `<task-notification>`,
  `<system-reminder>`, `<local-command-stdout>`, `<command-name>`. They must
  not end the turn. The response after a task notification belongs to the
  human prompt before it. Lines with `isSidechain: true` are sub-agent lines.

The response line has its own `timestamp`, so the turn duration is derivable.
About 20% of prompts have no final response: interrupted turns, queued
prompts, a `tool_use` with no result, or a session that still runs.

**Cursor (`state.vscdb`).** Order the bubbles of a composer with
`composerData.fullConversationHeadersOnly` (1382 composers) or, for old
composers, the `conversation` array or `createdAt` (549). The final response
is the last type-2 bubble with non-empty `text` and no `toolFormerData`
before the next type-1 bubble. Fetch bubbles by exact key
(`bubbleId:<composer>:<bubble>`); a full scan of the 165k bubbles takes 15 s.
About 14% of prompts have no final response.

**Corpus on 2026-09-07:** Claude 263 responses, 0.7 MB, median 2.5 KB.
Cursor 3924 responses, 8.4 MB, median 1.3 KB. See [[semantic-search]] for the
index cost per entry.
