# recap from the shell

These scripts use `curl` and `jq` to read the recap server.
Each script reads the server URL from `$RECAP`.
If `RECAP` is not set, the scripts use the prod server at `http://127.0.0.1:7717`.

```sh
export RECAP=http://127.0.0.1:7718   # the test server
./projects.sh
./recent-prompts.sh ~/git/recap 5
./diff.sh <entry id>
./search.sh 'fn main'
./follow.sh
```

The API:

| Route | Result |
|---|---|
| `GET /v1/status` | The store version, the load state, the embedding state. |
| `GET /v1/projects` | The projects, newest first. |
| `GET /v1/files?project=` | The edited files of a project. |
| `GET /v1/agents?project=` | The agent sessions of a project. |
| `GET /v1/history?project=&file=&agent=&kind=&after=&since=&limit=` | The entries, newest first, in groups. `kind` is `all`, `prompts` or `changes`. |
| `GET /v1/entries/{id}` | One entry. |
| `GET /v1/diff?ids=a,b[&format=unified]` | The diff of one entry, or of several entries. |
| `GET /v1/summaries?ids=a,b` | The added and removed line counts. |
| `GET /v1/search/{path,content,semantic}?q=` | The matches. The history filter parameters also apply. |
| `GET /v1/sessions/running` | The running Claude Code sessions. |
| `GET /v1/events[?format=ndjson]` | The change events: SSE, or one JSON object on each line. |
| `POST /v1/reload` | Start a load now. |
| `POST /v1/embed/start` | Start the semantic search index. |
| `POST /v1/sessions/{id}/restore` | Write the files of an archived Claude Code session back to `~/.claude/projects`. An entry of such a session has `"session_archived": true`. |
| `POST /v1/blame` | The body is `{"diff": "<unified diff>", "repo_root": "<path>"}`. |
