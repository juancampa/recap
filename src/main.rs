mod app;
mod blame;
mod claude;
mod cursor_sessions;
mod cursor_workspace;
mod diff;
mod embed;
mod history;
mod log;
mod nvim;
mod persist;
mod prompt;
mod search_history;
mod session;
mod settings;
mod ui;

use anyhow::Result;
use app::{App, KindFilter, Sources};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use history::{EditSource, HistoryStore};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use serde::Serialize;
use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

fn copy_to_clipboard(text: &str) {
    // `pbcopy` inherits the terminal unless the caller says otherwise. Its
    // output would paint over the TUI, so send it to /dev/null.
    let Ok(mut child) = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let _ = child.wait();
}

/// Tear down raw mode + alternate screen so a child process can take over the
/// terminal, run `f`, then re-initialize the TUI. Used for editors that draw
/// to the terminal directly (vim, nvim, etc.).
fn with_terminal_handoff<F>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    f: F,
) -> Result<()>
where
    F: FnOnce(),
{
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    f();

    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )?;
    terminal.clear()?;
    Ok(())
}

/// Open `path` with the appropriate editor for the entry's source. GUI
/// editors (Cursor) are spawned detached. Terminal editors (nvim, $EDITOR)
/// take over the screen via `with_terminal_handoff`.
fn open_entry(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    path: &Path,
    source: EditSource,
) -> Result<()> {
    match source {
        EditSource::Cursor => {
            let _ = Command::new("open")
                .arg(path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        EditSource::Neovim => {
            with_terminal_handoff(terminal, || {
                let _ = Command::new("nvim").arg(path).status();
            })?;
        }
        EditSource::ClaudeCode => {
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
            // EDITOR may include flags (e.g. `code --wait`); split on whitespace.
            let mut parts = editor.split_whitespace();
            let Some(cmd) = parts.next() else { return Ok(()); };
            let args: Vec<&str> = parts.collect();
            with_terminal_handoff(terminal, || {
                let _ = Command::new(cmd).args(&args).arg(path).status();
            })?;
        }
    }
    Ok(())
}

/// Absolute path to the `claude` binary. The `-e` command Ghostty runs does
/// not go through a login shell, so `~/.local/bin` may not be on PATH; prefer
/// the well-known install location and fall back to the bare name.
fn claude_bin() -> String {
    if let Ok(home) = std::env::var("HOME") {
        let p = format!("{home}/.local/bin/claude");
        if Path::new(&p).exists() {
            return p;
        }
    }
    "claude".to_string()
}

/// Path to the `findterm` window-focus helper (`~/bin/findterm`).
fn findterm_bin() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let p = PathBuf::from(home).join("bin/findterm");
    p.exists().then_some(p)
}

/// `o` action: jump to the Claude Code session an entry came from. If the
/// session is still running, focus its terminal window via `findterm --pid`.
/// Otherwise open a fresh Ghostty window resuming it (`claude --resume <id>`)
/// in the session's working directory (see [`open_resume_window`]). No-op for
/// entries without a session id
/// (Cursor / Neovim) — there is no session to resume.
fn focus_or_resume_session(entry: &history::HistoryEntry) {
    // Only Claude Code sessions are resumable via `claude --resume`. Cursor
    // entries also carry a session id (their composerId) for grouping, but it
    // is not a Claude session — never feed it to the resume path.
    if entry.source != history::EditSource::ClaudeCode {
        return;
    }
    let Some(session_id) = entry.session_id.clone() else {
        return;
    };

    // Consult a fresh scan rather than the cached snapshot so a session that
    // started/stopped since the last refresh is handled correctly.
    let running = session::RunningSessions::scan();
    if let Some(rs) = running.get(&session_id) {
        if let Some(findterm) = findterm_bin() {
            let _ = Command::new(findterm)
                .arg("--pid")
                .arg(rs.pid.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        return;
    }

    // Not running — resume in a new Ghostty window. Preferred working
    // directory: the session's own recorded cwd (most accurate), then the
    // entry's innermost project root, then the file's parent. Filter empties
    // (prompt entries have a synthetic, parent-less `file_path`).
    let cwd = entry
        .session_cwd
        .clone()
        .or_else(|| entry.project_root().map(Path::to_path_buf))
        .or_else(|| entry.file_path.parent().map(Path::to_path_buf))
        .filter(|dir| !dir.as_os_str().is_empty());

    open_resume_window(&session_id, cwd.as_deref());
}

/// Open a fresh Ghostty window running `claude --resume <id>` in `cwd`.
///
/// Driven through Ghostty's AppleScript interface rather than
/// `open -na Ghostty.app --args -e …`. The `open` route is unusable here:
/// macOS ignores `--args` for an already-running app, so it requires `-n` (a
/// second app *instance*) to deliver the command, which then restores the
/// user's saved windows (recap's included) as extra tabs and pops a
/// per-command "Allow Ghostty to execute" security prompt. `new window with
/// configuration` instead opens a single window in the *existing* instance,
/// honors an explicit working directory, and is a trusted automation path (no
/// prompt). If Ghostty isn't running, the `tell` block launches it.
fn open_resume_window(session_id: &str, cwd: Option<&Path>) {
    // Build `claude --resume <id>`. Ghostty's `command` runs via `/bin/sh -c`,
    // so quote the binary path and the (UUID) session id defensively.
    let command = format!("{} --resume {}", shell_quote(&claude_bin()), shell_quote(session_id));
    let workdir = cwd.map(|d| d.to_string_lossy().into_owned());

    let mut script = String::from("tell application \"Ghostty\"\n");
    script.push_str("  set cfg to new surface configuration\n");
    script.push_str(&format!("  set command of cfg to {}\n", applescript_quote(&command)));
    if let Some(dir) = &workdir {
        script.push_str(&format!(
            "  set initial working directory of cfg to {}\n",
            applescript_quote(dir)
        ));
    }
    script.push_str("  new window with configuration cfg\n");
    script.push_str("end tell\n");

    let _ = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Wrap a value in single quotes for `/bin/sh`, escaping embedded quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Quote a string as an AppleScript string literal (escaping `\` and `"`).
fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

/// Suspend the current process (Ctrl-Z behavior). Crossterm's raw mode
/// disables the terminal driver's signal generation, so SIGTSTP must be
/// raised manually. We tear the TUI down first so the user sees a clean
/// shell prompt, then re-initialize on resume.
fn suspend_self(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    with_terminal_handoff(terminal, || {
        // Synchronous self-signal — the syscall returns only after the kernel
        // resumes us via SIGCONT, so the helper's restore code runs post-fg.
        unsafe {
            libc::raise(libc::SIGTSTP);
        }
    })
}

/// Best-effort terminal restore. Safe to call from a panic hook: errors are
/// swallowed because there is nothing useful to do with them at that point,
/// and we want to fall through to the default panic printer regardless.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}

/// Build the embedding index without the TUI, then run one query against it.
/// The first build of the diff index needs 10 to 20 minutes, so it helps to
/// run it in a plain terminal and watch the progress.
fn run_index_cli(app: &mut App, queries: &[String]) -> Result<()> {
    use app::EmbedStatus;
    use std::time::Instant;

    load_store(app);

    app.start_embedding(true);
    let build = Instant::now();
    let mut last_done = usize::MAX;
    loop {
        app.drain_embed_events();
        match app.embed.status.clone() {
            EmbedStatus::Ready => break,
            EmbedStatus::Failed(e) => return Err(anyhow::anyhow!(e)),
            EmbedStatus::Off => break,
            EmbedStatus::Starting | EmbedStatus::Indexing => {}
        }
        if app.embed.done != last_done {
            last_done = app.embed.done;
            let rate = app.embed.done as f32 / build.elapsed().as_secs_f32().max(0.001);
            eprint!(
                "\rembedded {}/{} ({rate:.0}/s)      ",
                app.embed.done, app.embed.total
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    eprintln!(
        "\nindex ready: {} vectors ({} prompts, {} edits) in {:.0}s",
        app.embed.index.len(),
        app.embed.index.count_of(embed::EmbedKind::Prompt),
        app.embed.index.count_of(embed::EmbedKind::Diff),
        build.elapsed().as_secs_f32()
    );

    for q in queries {
        let asked = Instant::now();
        app.semantic_search(q, 50);
        println!("\nquery: {q}   ({:.0} ms)", asked.elapsed().as_secs_f32() * 1000.0);
        for (rank, m) in app.search.matches.iter().take(15).enumerate() {
            let entry = &app.store.entries[m.entry_idx];
            let kind = if entry.is_prompt() { "prompt" } else { "edit  " };
            println!(
                "{:>2}. {:.3} {kind} {} {}",
                rank + 1,
                m.score as f32 / 10_000.0,
                entry.timestamp.format("%Y-%m-%d"),
                entry.display_path(&app.store.project_names)
            );
        }
    }
    Ok(())
}

/// Load the whole history without the TUI. Progress goes to stderr.
fn load_store(app: &mut App) {
    let started = std::time::Instant::now();
    // `recap list` calls this again for each widening of its period. Only the
    // first call starts from an empty store, so only it prints the header.
    if app.store.entries.is_empty() {
        eprintln!("loading history…");
    }
    app.start_background_load();
    while app.loading {
        app.drain_load_events(50);
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!(
        "{} entries in {:.1}s",
        app.store.entries.len(),
        started.elapsed().as_secs_f32()
    );
}

/// `recap blame [--stdin] [--] [<git diff args>...]`: attribute the hunks of
/// a diff to the sessions and prompts that produced them, as JSON on stdout.
/// The diff comes from stdin with `--stdin`, else from `git diff <args>`
/// (default `HEAD`) in the current directory. A test of "is stdin a
/// terminal" is not enough: an agent harness runs the command with a stdin
/// that is not a terminal and never closes, and the read then never ends.
fn run_blame_cli(app: &mut App, git_args: &[String], stdin: bool) -> Result<()> {
    let root = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| anyhow::anyhow!("cannot run git: {e}"))?;
    if !root.status.success() {
        anyhow::bail!("recap blame: the current directory is not in a git repository");
    }
    let repo_root = PathBuf::from(String::from_utf8_lossy(&root.stdout).trim_end());

    let diff_text = if stdin {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        text
    } else {
        let mut cmd = Command::new("git");
        cmd.args(["diff", "--no-color"]);
        if git_args.is_empty() {
            cmd.arg("HEAD");
        } else {
            cmd.args(git_args);
        }
        let out = cmd.output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git diff failed: {}",
                String::from_utf8_lossy(&out.stderr).trim_end()
            );
        }
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let files = blame::parse_unified(&diff_text);
    load_store(app);
    let bash = match claude::projects_dir() {
        Some(dir) => claude::bash_calls(&dir),
        None => Vec::new(),
    };
    let blames = blame::attribute(&app.store, &files, &repo_root, &bash);
    let unattributed = blames.iter().filter(|b| b.winner.is_none()).count();
    eprintln!(
        "{} hunks in {} files, {unattributed} unattributed",
        blames.len(),
        files.len()
    );
    println!("{}", serde_json::to_string_pretty(&blames)?);
    Ok(())
}

/// One row of the `recap list` JSON output. A field that belongs to one kind
/// only stays absent for the other kind, so `jq` can select on `kind`.
#[derive(Serialize)]
struct ListRow<'a> {
    timestamp: String,
    /// "prompt" or "diff".
    kind: &'a str,
    source: EditSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<&'a Path>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_cwd: Option<&'a Path>,
    /// The prompt text. Present for a prompt entry only.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<&'a str>,
    /// The final text of the agent for this prompt. Absent when the turn has
    /// no final text.
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<&'a str>,
    /// The edited file. Present for a file change only.
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<&'a Path>,
    /// The file that holds the content after the edit.
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_path: Option<&'a Path>,
    /// The file that holds the content before the edit. Absent for the first
    /// snapshot of a file.
    #[serde(skip_serializing_if = "Option::is_none")]
    base_path: Option<&'a Path>,
}

/// The longest period that `recap list` reads before it gives up on the
/// count. A load of one year already reads almost every file.
const LIST_WINDOW_CAP: Duration = Duration::from_secs(365 * 24 * 3600);

/// Parse a period such as `30m`, `12h`, `7d` or `2w`. A plain number means
/// days. Returns `None` for `all`, which reads the full history.
fn parse_window(text: &str) -> Result<Option<Duration>> {
    if text == "all" {
        return Ok(None);
    }
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => (&text[..i], &text[i..]),
        None => (text, "d"),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("--since: `{text}` is not a period such as 7d or 12h"))?;
    let secs = match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 24 * 3600,
        "w" => 7 * 24 * 3600,
        _ => anyhow::bail!("--since: `{unit}` is not one of m, h, d, w"),
    };
    Ok(Some(Duration::from_secs(n * secs)))
}

/// Parse a `--filter` argument of `recap list`. The only key is `kind`, and
/// its values are the `kind` values of the JSON output.
fn parse_list_filter(text: &str) -> Result<KindFilter> {
    match text.split_once('=') {
        Some(("kind", "prompt")) => Ok(KindFilter::Prompts),
        Some(("kind", "diff")) => Ok(KindFilter::Changes),
        Some(("kind", value)) => {
            anyhow::bail!("--filter: `{value}` is not one of prompt, diff")
        }
        _ => anyhow::bail!("--filter: `{text}` is not a filter such as kind=prompt"),
    }
}

/// `recap list [-n <count>] [--since <period>] [--filter kind=<kind>]`: print
/// the newest entries as JSON on stdout. The filter applies before the count,
/// so `-n 5 --filter kind=prompt` prints the 5 newest prompts. The store sorts the entries newest first, so the command
/// takes a prefix. Progress goes to stderr, which keeps stdout parsable.
///
/// `window` limits the files that the loaders read, which is what makes the
/// command fast. A short period can hold fewer entries than `limit`, so the
/// command doubles the period and loads again until it has enough matching
/// entries or
/// it reaches [`LIST_WINDOW_CAP`]. The second load is incremental: the
/// loaders remember the files that they already read.
fn run_list_cli(
    app: &mut App,
    limit: usize,
    window: Option<Duration>,
    filter: KindFilter,
) -> Result<()> {
    let mut window = window;
    loop {
        app.load_since = window.map(|w| SystemTime::now() - w);
        load_store(app);
        if app.store.entries.iter().filter(|e| filter.keeps(e)).count() >= limit {
            break;
        }
        match window {
            None => break,
            Some(w) if w >= LIST_WINDOW_CAP => break,
            Some(w) => window = Some(w.saturating_mul(2).min(LIST_WINDOW_CAP)),
        }
    }
    let store = &app.store;
    let rows: Vec<ListRow> = store
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| filter.keeps(e))
        .take(limit)
        .map(|(i, e)| {
            let is_prompt = e.is_prompt();
            let project = e.project_root();
            ListRow {
                timestamp: e.timestamp.to_rfc3339(),
                kind: if is_prompt { "prompt" } else { "diff" },
                source: e.source,
                project,
                project_name: project
                    .and_then(|p| store.project_names.get(p))
                    .map(String::as_str),
                session_id: e.session_id.as_deref(),
                session_title: e
                    .session_id
                    .as_deref()
                    .and_then(|s| store.session_titles.get(s))
                    .map(String::as_str),
                session_cwd: e.session_cwd.as_deref(),
                prompt: e.prompt_text(),
                response: e.response_text(),
                file: (!is_prompt).then(|| e.file_path.as_path()),
                snapshot_path: (!is_prompt).then(|| e.snapshot_path.as_path()),
                base_path: if is_prompt {
                    None
                } else {
                    store.prev_snapshot(i)
                },
            }
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

fn main() -> Result<()> {
    let absolute_time = std::env::args().any(|a| a == "--absolute" || a == "-a");
    let args: Vec<String> = std::env::args().collect();
    let index_only = args.iter().any(|a| a == "--index");
    let blame_cmd = args.get(1).map(String::as_str) == Some("blame");
    let list_cmd = args.get(1).map(String::as_str) == Some("list");
    // `recap list [-n <count>]`. The default keeps the output small enough to
    // read, and a caller that wants more passes a larger count.
    let list_limit = args
        .iter()
        .position(|a| a == "-n" || a == "--limit")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50);
    // The loaders read only the files that changed within this period. The
    // default keeps the command fast; `--since all` reads everything.
    let list_window = match args.iter().position(|a| a == "--since") {
        Some(i) => parse_window(args.get(i + 1).map(String::as_str).unwrap_or("7d"))?,
        None => Some(Duration::from_secs(7 * 24 * 3600)),
    };
    let list_filter = match args.iter().position(|a| a == "--filter") {
        Some(i) => parse_list_filter(args.get(i + 1).map(String::as_str).unwrap_or(""))?,
        None => KindFilter::All,
    };
    let blame_stdin = blame_cmd && args[2..].iter().any(|a| a == "--stdin");
    // `recap blame [--stdin] [--] <git diff args>`: the rest goes to git.
    let git_args: Vec<String> = if blame_cmd {
        args[2..]
            .iter()
            .filter(|a| *a != "--" && *a != "--stdin")
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    // `--query` may repeat, so one index run can answer several questions.
    let queries: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--query")
        .filter_map(|(i, _)| args.get(i + 1).cloned())
        .collect();

    // Chain a terminal-restore step in front of the default panic handler so
    // backtraces aren't mangled by raw mode / the alternate screen.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    // Start with an empty store and kick the background loader off
    // immediately so the TUI can render within the first frame. Batches
    // stream in newest-first via `App::drain_load_events`.
    let sources = Sources::detect();
    let store = HistoryStore::from_entries(Vec::new());

    let mut app = App::new(store, sources);
    let persisted = persist::PersistedState::load();
    // The index command searches the whole corpus. A persisted project or file
    // selection would narrow the groups that the query ranks, so skip it.
    if !index_only && !blame_cmd && !list_cmd {
        app.apply_persisted(&persisted);
    }
    // CLI flag forces absolute time on; otherwise honor the persisted toggle.
    app.absolute_time = absolute_time || persisted.absolute_time;

    if index_only {
        return run_index_cli(&mut app, &queries);
    }
    if blame_cmd {
        return run_blame_cli(&mut app, &git_args, blame_stdin);
    }
    if list_cmd {
        return run_list_cli(&mut app, list_limit, list_window, list_filter);
    }
    app.start_background_load();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &mut app);

    app.persisted_state().save();

    restore_terminal();
    terminal.show_cursor()?;
    result
}

/// Compute the panel rectangles using the same layout the renderer uses.
/// The bottom row is reserved for the help bar; we strip it before splitting.
fn current_layout(
    size: Rect,
    projects_active: bool,
    files_active: bool,
    agent_mode: bool,
) -> ui::LayoutRects {
    let content = Rect {
        x: size.x,
        y: size.y,
        width: size.width,
        height: size.height.saturating_sub(1),
    };
    ui::compute_layout(content, projects_active, files_active, agent_mode)
}

/// Translate a click inside a panel's outer rect to a 0-based row index in
/// that panel's visible list (None if the click landed on the border or
/// outside the inner area).
fn click_to_row(panel: Rect, mx: u16, my: u16) -> Option<usize> {
    let inner_x = panel.x + 1;
    let inner_y = panel.y + 1;
    let inner_w = panel.width.saturating_sub(2);
    let inner_h = panel.height.saturating_sub(2);
    if my < inner_y || my >= inner_y + inner_h {
        return None;
    }
    if mx < inner_x || mx >= inner_x + inner_w {
        return None;
    }
    Some((my - inner_y) as usize)
}

/// Map a mouse position to a cell of `panel`'s inner rect, as
/// `(row, col)`. The position clamps to the inner rect, so a drag that leaves
/// the panel still names its nearest cell instead of ending the selection.
fn click_to_cell(panel: Rect, mx: u16, my: u16) -> (usize, usize) {
    let inner_x = panel.x + 1;
    let inner_y = panel.y + 1;
    let last_col = panel.width.saturating_sub(3);
    let last_row = panel.height.saturating_sub(3);
    let row = my.saturating_sub(inner_y).min(last_row);
    let col = mx.saturating_sub(inner_x).min(last_col);
    (row as usize, col as usize)
}

fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

#[allow(clippy::too_many_arguments)]
fn handle_mouse(
    app: &mut App,
    me: MouseEvent,
    rects: ui::LayoutRects,
    visible_history: usize,
    visible_projects: usize,
    visible_files: usize,
    visible_diff: usize,
    visible_agents: usize,
) {
    // Roughly emulate a 3-line scroll wheel step.
    const SCROLL_STEP: usize = 3;

    match me.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(projects) = rects.projects
                && rect_contains(projects, me.column, me.row)
            {
                app.active_panel = app::Panel::Projects;
                if let Some(row) = click_to_row(projects, me.column, me.row) {
                    let target = app.project_scroll + row;
                    select_project(app, target, visible_projects);
                }
            } else if let Some(files) = rects.files
                && rect_contains(files, me.column, me.row)
            {
                app.active_panel = app::Panel::Files;
                if let Some(row) = click_to_row(files, me.column, me.row) {
                    let target = app.file_scroll + row;
                    select_file(app, target, visible_files);
                }
            } else if let Some(agents) = rects.agents
                && rect_contains(agents, me.column, me.row)
            {
                app.active_panel = app::Panel::Agents;
                if let Some(row) = click_to_row(agents, me.column, me.row) {
                    app.select_agent_row(app.agent_scroll + row, visible_agents);
                }
            } else if rect_contains(rects.history, me.column, me.row) {
                app.active_panel = app::Panel::History;
                if let Some(row) = click_to_row(rects.history, me.column, me.row) {
                    if let Some(target) = app.history_item_at_line(row) {
                        select_history_row(app, target, visible_history);
                    }
                }
            } else if let Some(diff) = rects.diff
                && rect_contains(diff, me.column, me.row)
            {
                app.active_panel = app::Panel::Diff;
                let (row, col) = click_to_cell(diff, me.column, me.row);
                app.diff_select_start(row, col);
            }
            // A click outside the Diff panel drops the selection it held.
            if app.active_panel != app::Panel::Diff {
                app.diff_select_clear();
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(diff) = rects.diff {
                let (row, col) = click_to_cell(diff, me.column, me.row);
                app.diff_select_to(row, col);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            // The drag ended. Put the selected text on the clipboard, so the
            // user needs no second key to copy it.
            if let Some(diff) = rects.diff {
                let inner_width = (diff.width as usize).saturating_sub(2);
                if let Some(text) = ui::diff_selection_text(app, inner_width, visible_diff) {
                    copy_to_clipboard(&text);
                }
            }
        }
        MouseEventKind::ScrollDown => {
            let target = panel_under(rects, me.column, me.row);
            for _ in 0..SCROLL_STEP {
                match target {
                    Some(app::Panel::Projects) => app.project_down(visible_projects),
                    Some(app::Panel::Files) => app.file_down(visible_files),
                    Some(app::Panel::Agents) => app.agent_down(visible_agents),
                    Some(app::Panel::History) => app.history_down(visible_history),
                    Some(app::Panel::Diff) => app.diff_down(),
                    None => {}
                }
            }
            let _ = visible_diff;
        }
        MouseEventKind::ScrollUp => {
            let target = panel_under(rects, me.column, me.row);
            for _ in 0..SCROLL_STEP {
                match target {
                    Some(app::Panel::Projects) => app.project_up(visible_projects),
                    Some(app::Panel::Files) => app.file_up(visible_files),
                    Some(app::Panel::Agents) => app.agent_up(visible_agents),
                    Some(app::Panel::History) => app.history_up(visible_history),
                    Some(app::Panel::Diff) => app.diff_up(),
                    None => {}
                }
            }
        }
        _ => {}
    }
}

fn panel_under(rects: ui::LayoutRects, x: u16, y: u16) -> Option<app::Panel> {
    if rects.projects.is_some_and(|r| rect_contains(r, x, y)) {
        Some(app::Panel::Projects)
    } else if rects.files.is_some_and(|r| rect_contains(r, x, y)) {
        Some(app::Panel::Files)
    } else if rects.agents.is_some_and(|r| rect_contains(r, x, y)) {
        Some(app::Panel::Agents)
    } else if rect_contains(rects.history, x, y) {
        Some(app::Panel::History)
    } else if rects.diff.is_some_and(|r| rect_contains(r, x, y)) {
        Some(app::Panel::Diff)
    } else {
        None
    }
}

/// Move the project selection to the row index `target` in the projects
/// list (where row 0 = "All projects"). No-op for out-of-range targets.
fn select_project(app: &mut App, target: usize, visible: usize) {
    let total_rows = app.store.projects.len() + 1;
    if target >= total_rows {
        return;
    }
    let new_idx = if target == 0 { None } else { Some(target - 1) };
    if app.selected_project_idx != new_idx {
        app.selected_project_idx = new_idx;
        // Keep the row visible.
        if target < app.project_scroll {
            app.project_scroll = target;
        } else if target >= app.project_scroll + visible {
            app.project_scroll = target + 1 - visible;
        }
        // Different project = different files list; any active file_search
        // would otherwise hold indices into the old Vec.
        app.file_search.clear();
        app.refilter();
    }
}

/// Move the file selection to display row `target` (row 0 = "All files",
/// row i+1 = `app.files[i]`). No-op for out-of-range targets.
fn select_file(app: &mut App, target: usize, visible: usize) {
    let total_rows = app.files.len() + 1;
    if target >= total_rows {
        return;
    }
    let new_path = if target == 0 {
        None
    } else {
        app.files.get(target - 1).cloned()
    };
    if app.selected_file_path != new_path {
        app.selected_file_path = new_path;
        if target < app.file_scroll {
            app.file_scroll = target;
        } else if target >= app.file_scroll + visible {
            app.file_scroll = target + 1 - visible;
        }
        app.refilter();
    }
}

/// Move the history selection to display-row `target`. Group rows select
/// the group; child rows select the specific child within their group.
fn select_history_row(app: &mut App, target: usize, visible: usize) {
    use app::DisplayRow;
    let Some(row) = app.display_rows.get(target).cloned() else {
        return;
    };
    let (new_group, new_child) = match row {
        DisplayRow::Group(gi) => (gi, 0usize),
        DisplayRow::Child { group_idx, child_local } => (group_idx, child_local),
    };
    if (app.sel_group, app.sel_child) == (new_group, new_child) {
        return;
    }
    if app.effective_auto_expand() && app.sel_group != new_group {
        if let Some(g) = app.groups.get_mut(app.sel_group) {
            g.expanded = false;
        }
        if let Some(g) = app.groups.get_mut(new_group) {
            g.expanded = true;
        }
    }
    app.sel_group = new_group;
    app.sel_child = new_child;
    app.rebuild_display_after_external_mutation(visible);
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        let size = terminal.size()?;
        let area = Rect {
            x: 0,
            y: 0,
            width: size.width,
            height: size.height,
        };
        let projects_active = app.active_panel == app::Panel::Projects;
        let files_active = app.active_panel == app::Panel::Files;
        let rects = current_layout(
            area,
            projects_active,
            files_active,
            app.mode == app::Mode::Agent,
        );

        // History has a footer carved off its bottom, so its list height is
        // smaller than the other panels'; derive each from its actual rect.
        let visible_history = (rects.history.height as usize).saturating_sub(2);
        let visible_diff = rects
            .diff
            .map(|r| (r.height as usize).saturating_sub(2))
            .unwrap_or(0);
        let full = (size.height as usize).saturating_sub(4);
        let visible_projects = rects
            .projects
            .map(|r| (r.height as usize).saturating_sub(2))
            .unwrap_or(full);
        let visible_files = rects
            .files
            .map(|r| (r.height as usize).saturating_sub(2))
            .unwrap_or(full);
        let visible_agents = rects
            .agents
            .map(|r| (r.height as usize).saturating_sub(2))
            .unwrap_or(full);

        app.drain_load_events(visible_history);
        app.drain_embed_events();
        app.maybe_run_pending_search(visible_history);
        app.maybe_rescan_running();
        app.ensure_summaries_visible(visible_history);
        let list_width = (rects.history.width as usize).saturating_sub(2);
        app.recompute_row_heights(list_width, visible_history);
        app.set_diff_height(visible_diff);
        terminal.draw(|f| ui::draw(f, app))?;

        // While the background loader is running, poll more aggressively so
        // freshly-arrived batches surface within ~30ms instead of 100ms. Same
        // for a debounced content search: the loop has to come back around for
        // `maybe_run_pending_search` to fire it.
        let poll_ms = if app.loading || app.search.scan_pending() {
            30
        } else {
            100
        };
        if event::poll(Duration::from_millis(poll_ms))? {
            let ev = event::read()?;
            match ev {
                Event::Mouse(me) => {
                    handle_mouse(
                        app,
                        me,
                        rects,
                        visible_history,
                        visible_projects,
                        visible_files,
                        visible_diff,
                        visible_agents,
                    );
                }
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }

                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

                    // Search input mode swallows almost every key — handle first.
                    // At most one search prompt is in Typing phase at a time;
                    // dispatch to whichever it is.
                    if app.search.phase == app::SearchPhase::Typing {
                        match key.code {
                            KeyCode::Esc => app.search_cancel(visible_history),
                            KeyCode::Enter => app.search_commit(visible_history),
                            KeyCode::Backspace => app.search_pop_char(visible_history),
                            KeyCode::Char('c') if ctrl => app.search_cancel(visible_history),
                            // Vim's command line: Up and Down walk the queries
                            // this prompt remembers.
                            KeyCode::Up => app.search_history_older(visible_history),
                            KeyCode::Down => app.search_history_newer(visible_history),
                            KeyCode::Char(c) => app.search_push_char(c, visible_history),
                            _ => {}
                        }
                        continue;
                    }
                    if app.project_search.phase == app::SearchPhase::Typing {
                        match key.code {
                            KeyCode::Esc => app.project_search_cancel(visible_projects),
                            KeyCode::Enter => app.project_search_commit(),
                            KeyCode::Backspace => app.project_search_pop_char(visible_projects),
                            KeyCode::Char('c') if ctrl => {
                                app.project_search_cancel(visible_projects)
                            }
                            KeyCode::Up => app.project_search_history_older(visible_projects),
                            KeyCode::Down => app.project_search_history_newer(visible_projects),
                            KeyCode::Char(c) => app.project_search_push_char(c, visible_projects),
                            _ => {}
                        }
                        continue;
                    }
                    if app.file_search.phase == app::SearchPhase::Typing {
                        match key.code {
                            KeyCode::Esc => app.file_search_cancel(visible_files),
                            KeyCode::Enter => app.file_search_commit(),
                            KeyCode::Backspace => app.file_search_pop_char(visible_files),
                            KeyCode::Char('c') if ctrl => app.file_search_cancel(visible_files),
                            KeyCode::Up => app.file_search_history_older(visible_files),
                            KeyCode::Down => app.file_search_history_newer(visible_files),
                            KeyCode::Char(c) => app.file_search_push_char(c, visible_files),
                            _ => {}
                        }
                        continue;
                    }
                    if app.diff_search.phase == app::SearchPhase::Typing {
                        match key.code {
                            KeyCode::Esc => app.diff_search_cancel(),
                            KeyCode::Enter => app.diff_search_commit(),
                            KeyCode::Backspace => app.diff_search_pop_char(),
                            KeyCode::Char('c') if ctrl => app.diff_search_cancel(),
                            KeyCode::Up => app.diff_search_history_older(),
                            KeyCode::Down => app.diff_search_history_newer(),
                            KeyCode::Char(c) => app.diff_search_push_char(c),
                            _ => {}
                        }
                        continue;
                    }

                    // The settings menu takes every key while it is open.
                    if app.settings_menu.open {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('S') => {
                                app.settings_menu.open = false;
                            }
                            KeyCode::Char('c') if ctrl => break,
                            KeyCode::Char('j') | KeyCode::Down => app.settings_menu.down(),
                            KeyCode::Char('k') | KeyCode::Up => app.settings_menu.up(),
                            KeyCode::Char(' ') | KeyCode::Enter => {
                                app.toggle_selected_setting(visible_history);
                            }
                            _ => {}
                        }
                        continue;
                    }

                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('c') if ctrl => break,
                        KeyCode::Char('S') => app.toggle_settings_menu(),
                        KeyCode::Char('z') if ctrl => suspend_self(terminal)?,
                        KeyCode::Esc => {
                            // Vim's `:noh`: hide highlights but keep the search
                            // so n/N can resume. Each panel's search hides
                            // independently; we toggle whichever the user is
                            // currently focused on.
                            match app.active_panel {
                                app::Panel::History => app.search_hide(),
                                app::Panel::Projects => app.project_search_hide(),
                                app::Panel::Files => app.file_search_hide(),
                                app::Panel::Diff => {
                                    app.diff_select_clear();
                                    app.diff_search_hide();
                                }
                                app::Panel::Agents => {}
                            }
                        }
                        KeyCode::Tab => {
                            app.search_clear();
                            app.toggle_panel();
                        }
                        KeyCode::BackTab => {
                            app.search_clear();
                            app.toggle_panel_backward();
                        }
                        KeyCode::Char('l') => app.focus_right(),
                        KeyCode::Char('h') => app.focus_left(),
                        KeyCode::Char('x')
                            if app.active_panel == app::Panel::History
                                && app.can_toggle_auto_expand() =>
                        {
                            app.toggle_auto_expand(visible_history);
                        }
                        KeyCode::Char('>') if app.active_panel == app::Panel::History => {
                            app.inc_prompt_lines();
                        }
                        KeyCode::Char('<') if app.active_panel == app::Panel::History => {
                            app.dec_prompt_lines();
                        }
                        KeyCode::Char('p') if app.active_panel == app::Panel::History => {
                            app.cycle_kind_filter(visible_history);
                        }
                        KeyCode::Char('m') => app.toggle_mode(visible_history, visible_agents),
                        KeyCode::Char('t') => app.toggle_time(),
                        KeyCode::Char('y') => {
                            // A mouse selection in the Diff panel wins over
                            // the whole-entry copy, because the user marked
                            // the exact text they want.
                            let selected = rects.diff.and_then(|d| {
                                let w = (d.width as usize).saturating_sub(2);
                                ui::diff_selection_text(app, w, visible_diff)
                            });
                            if let Some(text) = selected {
                                copy_to_clipboard(&text);
                            } else if let Some((text, _)) = app.copy_target() {
                                copy_to_clipboard(&text);
                            }
                        }
                        KeyCode::Char('Y') => {
                            if let Some(text) = app.copy_response_target() {
                                copy_to_clipboard(&text);
                            }
                        }
                        KeyCode::Char('O') => {
                            if app.can_open_file()
                                && let Some(entry) = app.current_entry()
                            {
                                let path = entry.file_path.clone();
                                let source = entry.source;
                                open_entry(terminal, &path, source)?;
                            }
                        }
                        KeyCode::Char('o') => {
                            if let Some(entry) = app.current_entry() {
                                focus_or_resume_session(entry);
                            }
                        }
                        KeyCode::Char('r') if !ctrl => app.reload(visible_history),

                        KeyCode::Char('/') => match app.active_panel {
                            app::Panel::History => app.search_open(),
                            app::Panel::Projects => app.project_search_open(),
                            app::Panel::Files => app.file_search_open(),
                            app::Panel::Diff => app.diff_search_open(),
                            app::Panel::Agents => {}
                        },
                        KeyCode::Char('F') if app.active_panel == app::Panel::History => {
                            app.search_open_semantic();
                        }
                        KeyCode::Char('f') if app.active_panel == app::Panel::History => {
                            app.search_open_content();
                        }
                        KeyCode::Char('n') if !ctrl && !shift => match app.active_panel {
                            // The History panel runs the last search again,
                            // so `n` works after Esc dropped the matches.
                            app::Panel::History => app.search_next(visible_history),
                            app::Panel::Projects
                                if app.project_search.phase == app::SearchPhase::Active =>
                            {
                                app.project_search_next(visible_projects)
                            }
                            app::Panel::Files
                                if app.file_search.phase == app::SearchPhase::Active =>
                            {
                                app.file_search_next(visible_files)
                            }
                            app::Panel::Diff
                                if app.diff_search.phase == app::SearchPhase::Active =>
                            {
                                app.diff_search_next()
                            }
                            _ => {}
                        },
                        KeyCode::Char('N') => match app.active_panel {
                            // The History panel runs the last search again,
                            // so `n` works after Esc dropped the matches.
                            app::Panel::History => app.search_prev(visible_history),
                            app::Panel::Projects
                                if app.project_search.phase == app::SearchPhase::Active =>
                            {
                                app.project_search_prev(visible_projects)
                            }
                            app::Panel::Files
                                if app.file_search.phase == app::SearchPhase::Active =>
                            {
                                app.file_search_prev(visible_files)
                            }
                            app::Panel::Diff
                                if app.diff_search.phase == app::SearchPhase::Active =>
                            {
                                app.diff_search_prev()
                            }
                            _ => {}
                        },

                        KeyCode::Char('j') | KeyCode::Down => match app.active_panel {
                            app::Panel::History => app.history_down(visible_history),
                            app::Panel::Projects => app.project_down(visible_projects),
                            app::Panel::Files => app.file_down(visible_files),
                            app::Panel::Agents => app.agent_down(visible_agents),
                            app::Panel::Diff => app.diff_down(),
                        },
                        KeyCode::Char('k') | KeyCode::Up => match app.active_panel {
                            app::Panel::History => app.history_up(visible_history),
                            app::Panel::Projects => app.project_up(visible_projects),
                            app::Panel::Files => app.file_up(visible_files),
                            app::Panel::Agents => app.agent_up(visible_agents),
                            app::Panel::Diff => app.diff_up(),
                        },

                        KeyCode::Char('g') if !shift => match app.active_panel {
                            app::Panel::History => app.history_top(visible_history),
                            app::Panel::Projects => app.project_top(),
                            app::Panel::Files => app.file_top(),
                            app::Panel::Agents => app.agent_top(visible_agents),
                            app::Panel::Diff => app.diff_top(),
                        },
                        KeyCode::Char('G') => match app.active_panel {
                            app::Panel::History => app.history_bottom(visible_history),
                            app::Panel::Projects => app.project_bottom(visible_projects),
                            app::Panel::Files => app.file_bottom(visible_files),
                            app::Panel::Agents => app.agent_bottom(visible_agents),
                            app::Panel::Diff => app.diff_bottom(visible_diff),
                        },

                        // Ctrl+D / Ctrl+U — page navigation
                        KeyCode::Char('d') if ctrl => match app.active_panel {
                            app::Panel::History => app.history_page_down(visible_history),
                            app::Panel::Projects => app.project_page_down(visible_projects),
                            app::Panel::Files => app.file_page_down(visible_files),
                            app::Panel::Agents => app.agent_page_down(visible_agents),
                            app::Panel::Diff => app.diff_page_down(visible_diff),
                        },
                        KeyCode::Char('u') if ctrl => match app.active_panel {
                            app::Panel::History => app.history_page_up(visible_history),
                            app::Panel::Projects => app.project_page_up(visible_projects),
                            app::Panel::Files => app.file_page_up(visible_files),
                            app::Panel::Agents => app.agent_page_up(visible_agents),
                            app::Panel::Diff => app.diff_page_up(visible_diff),
                        },

                        KeyCode::PageDown => app.diff_page_down(visible_diff),
                        KeyCode::PageUp => app.diff_page_up(visible_diff),

                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}
