mod app;
mod claude;
mod cursor_sessions;
mod cursor_workspace;
mod diff;
mod history;
mod nvim;
mod persist;
mod session;
mod ui;

use anyhow::Result;
use app::{App, Sources};
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
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

fn copy_to_clipboard(path: &Path) {
    let s = path.to_string_lossy();
    let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() else {
        return;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(s.as_bytes());
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

fn main() -> Result<()> {
    let absolute_time = std::env::args().any(|a| a == "--absolute" || a == "-a");

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
    app.apply_persisted(&persisted);
    // CLI flag forces absolute time on; otherwise honor the persisted toggle.
    app.absolute_time = absolute_time || persisted.absolute_time;
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
fn current_layout(size: Rect, projects_active: bool, files_active: bool) -> ui::LayoutRects {
    let content = Rect {
        x: size.x,
        y: size.y,
        width: size.width,
        height: size.height.saturating_sub(1),
    };
    ui::compute_layout(content, projects_active, files_active)
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

fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

fn handle_mouse(
    app: &mut App,
    me: MouseEvent,
    rects: ui::LayoutRects,
    visible_history: usize,
    visible_projects: usize,
    visible_files: usize,
    visible_diff: usize,
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
            } else if rect_contains(rects.history, me.column, me.row) {
                app.active_panel = app::Panel::History;
                if let Some(row) = click_to_row(rects.history, me.column, me.row) {
                    if let Some(target) = app.history_item_at_line(row) {
                        select_history_row(app, target, visible_history);
                    }
                }
            } else if let Some(diff) = rects.diff {
                if rect_contains(diff, me.column, me.row) {
                    app.active_panel = app::Panel::Diff;
                }
            }
        }
        MouseEventKind::ScrollDown => {
            let target = panel_under(rects, me.column, me.row);
            for _ in 0..SCROLL_STEP {
                match target {
                    Some(app::Panel::Projects) => app.project_down(visible_projects),
                    Some(app::Panel::Files) => app.file_down(visible_files),
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
        let rects = current_layout(area, projects_active, files_active);

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

        app.drain_load_events(visible_history);
        app.maybe_rescan_running();
        app.ensure_summaries_visible(visible_history);
        let list_width = (rects.history.width as usize).saturating_sub(2);
        app.recompute_row_heights(list_width, visible_history);
        terminal.draw(|f| ui::draw(f, app))?;

        // While the background loader is running, poll more aggressively so
        // freshly-arrived batches surface within ~30ms instead of 100ms.
        let poll_ms = if app.loading { 30 } else { 100 };
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
                            KeyCode::Enter => app.search_commit(),
                            KeyCode::Backspace => app.search_pop_char(visible_history),
                            KeyCode::Char('c') if ctrl => app.search_cancel(visible_history),
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
                            KeyCode::Char(c) => app.diff_search_push_char(c),
                            _ => {}
                        }
                        continue;
                    }

                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('c') if ctrl => break,
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
                                app::Panel::Diff => app.diff_search_hide(),
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
                        KeyCode::Char('t') => app.toggle_time(),
                        KeyCode::Char('y') => {
                            if let Some(p) = app.selected_path() {
                                copy_to_clipboard(&p);
                            }
                        }
                        KeyCode::Char('O') => {
                            if let Some(entry) = app.current_entry() {
                                if !entry.is_prompt() {
                                    let path = entry.file_path.clone();
                                    let source = entry.source;
                                    open_entry(terminal, &path, source)?;
                                }
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
                        },
                        KeyCode::Char('f') if app.active_panel == app::Panel::History => {
                            app.search_open_content();
                        }
                        KeyCode::Char('n') if !ctrl && !shift => match app.active_panel {
                            app::Panel::History
                                if app.search.phase == app::SearchPhase::Active =>
                            {
                                app.search_next(visible_history)
                            }
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
                            app::Panel::History
                                if app.search.phase == app::SearchPhase::Active =>
                            {
                                app.search_prev(visible_history)
                            }
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
                            app::Panel::Diff => app.diff_down(),
                        },
                        KeyCode::Char('k') | KeyCode::Up => match app.active_panel {
                            app::Panel::History => app.history_up(visible_history),
                            app::Panel::Projects => app.project_up(visible_projects),
                            app::Panel::Files => app.file_up(visible_files),
                            app::Panel::Diff => app.diff_up(),
                        },

                        KeyCode::Char('g') if !shift => match app.active_panel {
                            app::Panel::History => app.history_top(visible_history),
                            app::Panel::Projects => app.project_top(),
                            app::Panel::Files => app.file_top(),
                            app::Panel::Diff => app.diff_top(),
                        },
                        KeyCode::Char('G') => match app.active_panel {
                            app::Panel::History => app.history_bottom(visible_history),
                            app::Panel::Projects => app.project_bottom(visible_projects),
                            app::Panel::Files => app.file_bottom(visible_files),
                            app::Panel::Diff => app.diff_bottom(visible_diff),
                        },

                        // Ctrl+D / Ctrl+U — page navigation
                        KeyCode::Char('d') if ctrl => match app.active_panel {
                            app::Panel::History => app.history_page_down(visible_history),
                            app::Panel::Projects => app.project_page_down(visible_projects),
                            app::Panel::Files => app.file_page_down(visible_files),
                            app::Panel::Diff => app.diff_page_down(visible_diff),
                        },
                        KeyCode::Char('u') if ctrl => match app.active_panel {
                            app::Panel::History => app.history_page_up(visible_history),
                            app::Panel::Projects => app.project_page_up(visible_projects),
                            app::Panel::Files => app.file_page_up(visible_files),
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
