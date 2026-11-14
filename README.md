# recap

recap shows the prompts that you gave to coding agents and the file changes that the agents made.
It reads the local data of Claude Code, Cursor and Neovim.
It puts the prompts and the changes in one history for each project.

recap runs on macOS.

## Parts

| Part | Path | Function |
|---|---|---|
| `recap` | `src/` | The binary. It contains the TUI, the CLI commands and the server. |
| `recap-server` | `crates/recap-server` | The server. It loads the sources, keeps the store and serves the HTTP API. |
| `recap-api` | `crates/recap-api` | The API types, the routes and the `Api` trait. |
| `recap-client` | `crates/recap-client` | A blocking Rust client for the HTTP API. |
| `recap-unidiff` | `crates/recap-unidiff` | A parser for unified diffs. |
| `slicer` | `crates/slicer` | A tool that plans and stages a series of commits from the pending changes. |

The TUI and the CLI commands are clients of the server.
The server runs as a launchd agent, or in the foreground with `recap serve`.

## Sources

| Source | Data that recap reads |
|---|---|
| Claude Code | The session files in `~/.claude/projects`. |
| Cursor | The Local History snapshots, the workspace storage and the composer database. |
| Neovim | The undo files in the `undodir` of Neovim. |

The server loads the sources again each 5 seconds.
Each load reads only the files that changed.
The server keeps its cache in `~/.cache/recap`.

recap does not read files in the folders that macOS protects, for example `~/Documents` and `~/Downloads`.

## Build

```sh
cargo build --release
```

## Install the server

```sh
cargo run --release -- service install
recap service status
```

`recap service install` copies the binary to `~/.local/share/recap/bin/recap`.
Then it writes `~/Library/LaunchAgents/io.membrane.recap.plist` and starts the agent.
launchd starts the server at login, and starts it again if it stops.
A later `cargo build` does not change the installed server.
To upgrade the server, run `recap service install` again from the new build.

The server listens on `http://127.0.0.1:7717`.
The server writes its log to `~/Library/Logs/recap/server.log`.
Use `recap logs -f` to follow the log.

## Commands

```text
recap [options]                    Open the TUI.
recap list [options]               Print the newest entries as JSON.
recap blame [--stdin] [--] [<git diff args>...]
                                   Find the prompts that made the hunks of a diff.
recap serve [options]              Run the server in the foreground.
recap service <command>            Manage the prod server as a launchd agent.
recap logs [-f] [-n <lines>]       Print the end of the prod server log.
```

Run `recap --help` for all the options.

### The TUI

The TUI has five panels: Projects, Files, Agents, History and Diff.

| Key | Action |
|---|---|
| `j` / `k`, `g` / `G` | Move down and up, go to the first and the last row. |
| `Ctrl-d` / `Ctrl-u` | Move a half page down and up. |
| `h` / `l`, `Tab` | Move the focus to a different panel. |
| `/` | Search in the panel that has the focus. |
| `n` / `N` | Go to the next and the previous match. |
| `f` | Search the content of the diffs (History panel). |
| `F` | Do a semantic search (History panel). |
| `p` | Show all the entries, only the prompts, or only the changes (History panel). |
| `<` / `>` | Show fewer or more lines of each prompt (History panel). |
| `m` | Change the mode of the panels. |
| `Q` | Open the triage web UI in the browser (see [Triage](#triage)). |
| `t` | Show relative times or absolute times. |
| `y` | Copy the entry, or the text that you selected with the mouse. |
| `Y` | Copy the response of the agent. |
| `o` | Go to the Claude Code session of the entry. |
| `O` | Open the file of the entry in an editor. |
| `r` | Load the sources now. |
| `S` | Open the settings. |
| `q` | Quit. |

If the session of the entry runs, `o` focuses its terminal window.
If the session does not run, `o` opens a new Ghostty window with `claude --resume <id>`.

### Triage

Triagent (`claude -p --model opus`) reads your Claude Code sessions and keeps a list of the work that you and the agents must still do.
The triage web UI shows that list. Open it at `http://127.0.0.1:7717/`, with `recap --queue`, or with `Q` in the TUI.

The server starts a triage run when a session stops its turn.
A stop is a change of the session from `busy` to `idle`, or a change to `waiting`.
The server reads the status from the files in `~/.claude/sessions/`.
One run occurs at a time. The stops that occur during a run go to the next run.
At the end of a run, triagent writes the delay to the next scheduled run.

The runner is off by default. To turn it on, give the server a triage directory:

```sh
recap service install --triage-dir ~/.local/share/recap/triage
recap serve --triage-dir ~/.local/share/recap/triage
```

A later `recap service install` keeps the triage directory. `recap service install --no-triage` removes it.
`recap serve --triage-paused` serves the triage data and the web UI, but starts no triage run and no check.

#### The triage data

The server keeps the triage data in the SQLite file `<triage dir>.db`, next to the triage directory.
Triagent cannot write this file. It changes the data with `POST /v1/triage/apply`, and the server refuses the changes that only you can make.

| Object | Content |
|---|---|
| Topic | The goal that one or more sessions work on, for example "Worker memory management". It has a title and a summary of its state. |
| Session | The topic of the session, one sentence about what the session needs, and the last turn that triagent read. You can mute a session until its next turn. |
| Item | One piece of work. See [Items and checks](#items-and-checks). |

The triage directory contains the files that triagent writes:

| File | Content |
|---|---|
| `memory.md` | The notes of triagent that apply to many runs. |
| `next-run` | The delay to the next scheduled run (`90s`, `30m`, `2h`, `1d`), and the reason. |
| `checks/<script>` | The check scripts that triagent writes. |

`crates/recap-server/src/triage_prompt.md` contains the instructions of triagent.

When the server finds a triage directory of an earlier version (`items.json`, `agents/`), it imports the data into the database and moves the old files to `<triage dir>.legacy/`.

#### Items and checks

Each item has an owner (the agent or you), an impact (`critical`, `high`, `normal` or `low`), the reason for a critical or a high impact, and a done condition.
It can also have a due time, an event that the item must come before (such as the next prod deploy), a prompt that you can send to the session, and references. A reference names one turn of a session and quotes the sentence of the turn that is about the item: where the work was first named, where an agent said that the work is done, or another fact.
Triagent writes a check script for each done condition that a script can test.
The server runs the check of each open item: every 10 minutes for a critical item, 30 minutes for a high item, 2 hours for a normal item, and 6 hours for a low item.
A check exits with 0 when the item is done, 1 when it is not done, and another code when it cannot decide.
An item is verified only when its check exits with 0.
An item that an agent says is done, but that no check confirms, is claimed.

The server calculates the priority order of the open items. No ranking is stored.
A late item comes first: its event occurred, or its due time passed.
Then the order is the impact, then an item whose session waits for you now, then the due time, then the age.

The server starts a triage run without a session stop in these cases:
the check outcome of a critical or high item changes, the event of an item occurs before the item is done, or the due time of a critical or high item comes near.
For a critical item, the server also sends a notification through Hammerspoon.

The server runs a check script only after you approve it, in the Checks view of the web UI or with this command:

```sh
recap triage approve
```

Each of them shows a new or changed script, as the full text or as a diff.
The approval copies the script to `<triage dir>.approved/`, next to the triage directory, where triagent cannot write.
The server runs the approved copy. A script that changed after the approval gives the outcome `unapproved` until you approve it again.

#### The web UI

| View | Content |
|---|---|
| Items | All the open items in the priority order, one line for each item. You can show your items, the items of the agents, or all. You can group the items by topic or by session, and sort them by priority or by date (the newest item first). The sessions that wait for you now are at the top. |
| Checks | The check scripts, with the colors of their syntax. A script that waits for your approval shows its text or its diff, and an Approve button. The tab shows the number of scripts that wait. |
| Activity | The changes of the triage data, newest first. The top bar shows the number of changes since your last look. |

A click on an item shows its details in place: the reason, the done condition, the last check result, the prompt, and the references. A click on the quote of a reference opens the turn in the session panel and marks the sentence. For an item that an agent said is done, the state is a link to that statement.

The session panel is between the main view and the chat. Move the line at its left side to change its width.
The panel has an input for a prompt. "Send to the session" writes the prompt in the Ghostty terminal of a Claude Code session and presses Enter there.
The server sends a prompt only to an idle session, because the Enter key answers a permission prompt.
For a session that does not run, the server opens a new Ghostty window that resumes the session with the prompt.
"Edit and send" in the details of an item writes the prompt of the item in this input.
From the details you can confirm, reopen, drop or delete the item, change its impact or its owner, copy its prompt, and go to its session.
A click on a session opens a panel with all the turns of the session, as the prompt and the final response of each turn. The list stays in place behind the panel.
A closed item is dim and has a check mark. The closed items are in a group at the end of the Items view.

The page shows each reference in a text as a link:

- The id of an item, and the first 8 characters of the id of a session. The pointer on the link shows the title, the impact, the topic, the session and the age.
- A pull request: `PR 12` and `#12` go to the GitHub repository of the session. `<name> PR 12` goes to the repository `<name>` of another session, and `<owner>/<name>#12` to that repository.

Each topic has its own color.

The address of the page holds the navigation state, so the Back button and a reload work:

| Part | Content |
|---|---|
| `/items`, `/checks`, `/activity` | The view. |
| `?group=topic`, `?group=session` | The grouping of the Items view: one group for each topic, or one group for each session. |
| `?q=<text>` | The text filter of the Items view. |
| `?session=<chain>&turn=<n>` | The session in the session panel, and the turn that the panel marks. With `&ref=<item>/<index>`, the panel marks the quote of that reference of the item. |
| `#<id>` | The object that the view shows and marks: an item in the Items view, a script in the Checks view. `#topic:<id>` shows the group of a topic in the Items view. |

The chat on the right side sends a message to triagent.
Type `@` to name an item, a session or a topic, or use an "Ask triagent" button.
The `@` list finds an id by its letters in order: `@7bf1` finds the item `7b6d7bd0-f1`. The server adds the current data of each named object to the message.
Each conversation is one Claude Code session, so triagent knows the earlier messages of the conversation.
A reply lists the changes that triagent made to the triage data.

| Key | Action |
|---|---|
| `1` to `3` | Show the Items, Checks or Activity view. |
| `j` / `k` | Move the selection. |
| `Enter` | Show or hide the details of the selected item. |
| `a` | Add the selected item to the chat message. |
| `c` | Go to the chat. |
| `/` | Go to the filter. |
| `Esc` | Close the session panel, or leave a text field. |

The routes under `/ui/` do the actions that only you can do, such as the approval of a script. Each request needs a token that the server writes into the page.
The server makes a new token at each start. The page shows a notice when the server started again: load the page again then.
The server listens only on `127.0.0.1`.

A review run reads all the agents of the last 24 hours, and gives a topic to each session that has none.
The server starts a review when the triage data has no session, and when you press "Review" in the web UI.
A review run also completes the open items that have no done condition, no reference or no quote.
The selector next to "Review" sets the model of the review run, and the selector in the chat sets the model of a reply.
Each other run uses `opus`.

`examples/hammerspoon/recap.lua` has the `recapNotify` function that triagent uses for notifications.
The comment at the start of the file tells how to install it.

### `recap list`

`recap list` prints the newest entries as JSON.

```sh
recap list -n 20 --since 12h --filter kind=prompt
```

### `recap blame`

`recap blame` finds the agent session and the prompt that made each hunk of a diff.
It runs `git diff` with the arguments that you give, or it reads a diff from stdin with `--stdin`.
It prints one JSON object for each hunk.

```sh
recap blame -- --cached
git diff main | recap blame --stdin
```

## Semantic search

The semantic search uses a local embedding model in `llama-server` from llama.cpp.
The server starts the model only when a client asks for a semantic search.
The server expects these files:

- `~/.cache/recap/llamacpp/llama-b10830/llama-server`
- `~/.cache/recap/models/Qwen3-Embedding-0.6B-Q8_0.gguf`

To download the model, run this command:

```sh
huggingface-cli download Qwen/Qwen3-Embedding-0.6B-GGUF Qwen3-Embedding-0.6B-Q8_0.gguf --local-dir ~/.cache/recap/models
```

To build the index without the TUI, run `recap --index`.

## HTTP API

The server serves the API under `/v1/`.
The server also serves the triage web UI at its root URL, for example `http://127.0.0.1:7717/` (see [Triage](#triage)).
[`examples/cli/README.md`](examples/cli/README.md) lists all the routes.
The `examples` directory has shell scripts and a web page that use the API.

```sh
curl -s http://127.0.0.1:7717/v1/projects | jq
```

A Rust program can use `recap-client`:

```rust
use recap_api::Api;
let client = recap_client::Client::from_env();
for project in client.projects().unwrap().projects {
    println!("{} {}", project.count, project.name);
}
```

## Test server

```sh
cargo run -- serve --test
RECAP_URL=http://127.0.0.1:7718 cargo run
```

The test server listens on port 7718.
It copies the prod cache to `~/.cache/recap-test` and uses that copy.
The test server never writes to the prod cache.

## slicer

slicer helps you make a series of small commits from a large set of uncommitted changes.
An agent often makes many changes for many prompts before a commit.
slicer asks the recap server which prompt made each hunk.
Then it puts the hunks in groups, with one group for each prompt.
You stage one group or more, and you commit them as one commit.

slicer works in the root repository and in all the initialized submodules.
The recap server must run, because slicer gets the attribution from `POST /v1/blame`.
slicer reads the server URL from `RECAP_URL`.

slicer has no configuration file.
You give all the settings of a repository as flags.

### Build and run

```sh
cargo build --release -p slicer
./target/release/slicer --help
```

You can also run slicer through cargo.
Cargo gives the arguments after `--` to slicer:

```sh
cargo run -p slicer -- plan
```

The examples below use `slicer` without cargo.

### The steps

1. Run `slicer plan` to make the plan.
2. Read the report and the group files.
3. Run `slicer stage` for the hunks of one commit, then run `git commit`.
4. Do step 3 again for each commit.
5. Run `slicer finish` before the push.

### `slicer plan`

```sh
slicer plan
slicer plan --owner my-org --ignore '*.log' -- src/ crates/slicer
```

`slicer plan` reads these changes in each repository:

- the changes to the tracked files since `HEAD`, in the index and in the working tree
- the untracked files that `.gitignore` does not ignore

The plan does not contain these files:

- an untracked file that is larger than 1 MB, or that is binary
- an untracked file that matches an `--ignore` glob
- an untracked file whose name looks like a secret, for example `.env` or `id_rsa`

The report lists these files in "Not in the plan".

slicer gives each hunk an id such as `H12`.
It gives each group an id such as `G3`.
A group contains the hunks of one prompt in one agent session.
Changes that you made by hand in an editor go in one group for each editor.
slicer sorts the groups by the time of the prompt.
A hunk that recap cannot attribute goes in "Without attribution".

The report also shows these facts:

- the state of each submodule: the branch, the dirty files, the pointer change and the unpushed commits
- the links between groups: a hunk of one group that also matches the lines of a different group
- the WIP markers: added lines with `dbg!`, `todo!`, `TODO`, `FIXME` or a log call in a comment

`slicer plan` writes the plan to the plan directory.
The default plan directory is `.git/slice` of the outermost repository.

| File | Contents |
|---|---|
| `plan.json` | The hunks and the groups. `slicer stage` and `slicer show` read this file. |
| `dossier.md` | The report. |
| `groups/G<n>.md` | The prompt and the diff of each hunk of one group. |
| `groups/unattributed.md` | The diff of each hunk without attribution. |

`slicer plan` prints the report.
If the report is long, `slicer plan` prints only the path of `dossier.md`.

| Flag | Function |
|---|---|
| `--out <dir>` | Write the plan to this directory. |
| `--owner <owner>` | A GitHub owner that you can push to. Give the flag one time for each owner. |
| `--fallback-branch <branch>` | The branch for a detached submodule when slicer finds no other branch. |
| `--ignore <glob>` | Leave an untracked path out of the plan. `*`, `**` and `?` work as in `.gitignore`. |
| `--wip <regex>` | An extra WIP marker. The regex tests each added line. |
| `--blame-root <path>` | The checkout where the agents made the changes, if the changes are now in a different checkout. |
| `-- <path>...` | Plan only the changes under these paths. The paths are relative to the root repository. |

### `slicer show`

```sh
slicer show H12 H14
slicer show G3
```

`slicer show` prints the diff of each hunk that you name.
For a group id, it prints the diff of each hunk of the group.
Use it to read one hunk without the full group file.

### `slicer stage`

```sh
slicer stage G3
slicer stage G3,G5 H40
slicer stage G3 --exclude-hunk H80
slicer stage G7 --drop-line 'dbg!'
```

`slicer stage` adds the hunks of the groups and the single hunks that you name to the git index.
Each hunk goes to the index of its own repository.
If git refuses one hunk, `slicer stage` stages no hunk, and it tells you the id of the hunk that git refused.
After the stage, it lists the WIP markers that are now in the index.

| Flag | Function |
|---|---|
| `--plan <dir>` | Read the plan from this directory. |
| `--exclude-hunk <id>` | Keep this hunk of a named group out of the index. |
| `--drop-line <regex>` | Keep the changed lines that match the regex out of the index. The other lines of the hunk go in. |

The ids come from the last `slicer plan`.
Run `slicer plan` again if the working tree changed.

`slicer stage` does not commit.
For a change in a submodule, commit in the submodule first.
Then stage the new submodule pointer in the parent with `git add <path>`.

### `slicer finish`

```sh
slicer finish --owner my-org
```

`slicer finish` fetches each submodule.
Then it reports these problems:

- a submodule commit that is not on the remote
- a submodule pointer in the parent that no remote branch contains
- a detached submodule HEAD that moved
- local submodule commits in a repository whose owner is not an `--owner`

It also prints these facts:

- the uncommitted paths that stay in the working tree
- the root commits that are not pushed
- the subject of each commit after the default branch, oldest first, for the PR body
- the current PR of the branch, if `gh` finds one

The exit code is 1 if `slicer finish` found a problem.

## Environment

| Variable | Function |
|---|---|
| `RECAP_URL` | The server that the clients use. The default is `http://127.0.0.1:7717`. |
| `RECAP_LOG` | The log level of the server: `error`, `warn`, `info`, `debug` or `trace`. |

## Notes

The `memory` directory has notes about the design.
For example, it tells how recap connects a Cursor edit to its agent session, and how `recap blame` works.
