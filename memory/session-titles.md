---
name: session-titles
description: Where the title of an agent session comes from for Claude Code and for Cursor, and how recap holds both in one map (measured 2026-09-20)
metadata:
  type: project
---

`HistoryStore::session_titles` maps an agent session id to the title that the
History panel prints in its title column. Two sources fill the one map.

**Claude Code.** The session file `~/.claude/projects/*/<session>.jsonl` holds
lines of `type == "ai-title"` with the fields `aiTitle` and `sessionId`.
Claude Code appends a new line each time it renames the session, so
`claude::load_session` keeps the last one. On 2026-09-20, 89 of 101 session
files carried a title. No file carried a `type == "summary"` line, so the
summary format that older transcripts used is not a source here.

**Cursor.** The composer name, read by `cursor_sessions` from the
`composerHeaders` table (`name`, else `subtitle`).

The Claude loader thread sends `LoadEvent::SessionTitles` and the Cursor
thread sends `LoadEvent::CursorSessions`. Both **extend** the map; neither
replaces it, or one source erases the titles of the other.

See [[agent-responses]] for the other data that these same files hold.
