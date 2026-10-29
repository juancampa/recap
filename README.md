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
| `Q` | Go to Queue mode (see [The follow-up queue](#the-follow-up-queue)). |
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

### The follow-up queue

The follow-up queue shows the Claude Code sessions that wait for you.
A triage agent (`claude -p --model opus`) keeps notes about each session in a directory of text files.

The server starts a triage run when a session stops its turn.
A stop is a change of the session from `busy` to `idle`, or a change to `waiting`.
The server reads the status from the files in `~/.claude/sessions/`.
One run occurs at a time. The stops that occur during a run go to the next run.
At the end of a run, the triage agent writes the delay to the next scheduled run.

The runner is off by default. To turn it on, give the server a triage directory:

```sh
recap service install --triage-dir ~/.local/share/recap/triage
recap serve --triage-dir ~/.local/share/recap/triage
```

A later `recap service install` keeps the triage directory. `recap service install --no-triage` removes it.

The triage directory contains these files:

| File | Content |
|---|---|
| `memory.md` | The notes of the triage agent that apply to many runs. |
| `top.md` | The chain ids of rank 1, 2 and 3, one on each line. |
| `next-run` | The delay to the next scheduled run (`90s`, `30m`, `2h`, `1d`), and the reason. |
| `agents/<chain-id>.md` | The follow-ups, the agent TODO list and your TODO list for one session. |

The server stores none of these data.
`crates/recap-server/src/triage_prompt.md` contains the instructions and the file formats of the triage agent.

`recap --queue`, or `Q` in the TUI, shows Queue mode.
Queue mode has three panels: Queue, History and Triage.
The Queue panel shows the sessions that have notes.
A badge marks the sessions of rank 1, 2 and 3, and a resolved session is dim.
The Triage panel shows the notes of the selected session.

| Key | Panel | Action |
|---|---|---|
| `1`, `2`, `3` | All | Select the session of that rank. |
| `s` | All | Change the order: creation time, last message, rank. |
| `R` | All | Start a review of the agents of the last 24 hours. |
| `Enter` | Queue | Go to the session (the same action as `o`). |
| `x` | Queue | Mark the current turn as resolved, or clear the mark. |
| `d` | Queue | Hide the session until its next turn. |
| `D` | Queue | Hide all the resolved sessions. |
| `j` / `k` | Triage | Move between the follow-ups and the TODO items. |
| `Enter` | Triage | Copy the follow-up, then go to the session. |
| `Space` | Triage | Mark a TODO item as done, or clear the mark. |

A new turn in a session clears the resolved mark and the hidden mark.
In the Triage panel, a click selects an item, a click on a check box toggles the item, and the mouse wheel moves the selection.

The first row of the Queue panel is the current or last run of the triage agent.
For that row, the Triage panel shows the state of the runner, the current action of the run, the end of the last run, and the next scheduled run.
The History panel shows the prompt of the run.

A review run reads all the agents of the last 24 hours, and writes notes for the agents that need you.
The server starts a review when the triage directory has no agent files, and when you press `R`.

`examples/hammerspoon/recap.lua` adds a global hotkey that focuses the queue window, and the `recapNotify` function that the triage agent uses for notifications.
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
The server also serves a web page at its root URL, for example `http://127.0.0.1:7717/`.
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
