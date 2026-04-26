mod app;
mod claude;
mod diff;
mod history;
mod ui;

use anyhow::Result;
use app::App;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use history::HistoryStore;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, path::PathBuf, time::Duration};

fn cursor_history_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    for candidate in [
        "Library/Application Support/Cursor/User/History",
        "Library/Application Support/Code/User/History",
        ".config/Cursor/User/History",
        ".config/Code/User/History",
    ] {
        let p = PathBuf::from(&home).join(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn main() -> Result<()> {
    let absolute_time = std::env::args().any(|a| a == "--absolute" || a == "-a");

    let mut entries = Vec::new();
    if let Some(dir) = cursor_history_dir() {
        match HistoryStore::load_cursor(&dir) {
            Ok(mut e) => entries.append(&mut e),
            Err(e) => eprintln!("recap: cursor history failed: {e}"),
        }
    }
    if let Some(dir) = claude::projects_dir() {
        match claude::load(&dir) {
            Ok(mut e) => entries.append(&mut e),
            Err(e) => eprintln!("recap: claude history failed: {e}"),
        }
    }
    let store = HistoryStore::from_entries(entries);

    let mut app = App::new(store);
    app.absolute_time = absolute_time;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        let size = terminal.size()?;
        let visible_history = (size.height as usize).saturating_sub(4);
        let visible_projects = (size.height as usize).saturating_sub(4);
        let visible_diff = (size.height as usize).saturating_sub(4);

        app.ensure_summaries_visible(visible_history);
        terminal.draw(|f| ui::draw(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Tab => app.toggle_panel(),
                    KeyCode::Char('t') => app.toggle_time(),

                    KeyCode::Char('j') | KeyCode::Down => match app.active_panel {
                        app::Panel::History => app.history_down(visible_history),
                        app::Panel::Projects => app.project_down(visible_projects),
                    },
                    KeyCode::Char('k') | KeyCode::Up => match app.active_panel {
                        app::Panel::History => app.history_up(visible_history),
                        app::Panel::Projects => app.project_up(visible_projects),
                    },

                    // Ctrl+D / Ctrl+U — page navigation
                    KeyCode::Char('d') if ctrl => match app.active_panel {
                        app::Panel::History => app.history_page_down(visible_history),
                        app::Panel::Projects => app.project_page_down(visible_projects),
                    },
                    KeyCode::Char('u') if ctrl => match app.active_panel {
                        app::Panel::History => app.history_page_up(visible_history),
                        app::Panel::Projects => app.project_page_up(visible_projects),
                    },

                    // J/K — scroll diff
                    KeyCode::Char('J') => app.diff_down(),
                    KeyCode::Char('K') => app.diff_up(),

                    // Ctrl+D/U on diff when history panel active (using PageDown/Up as alternative)
                    KeyCode::PageDown => app.diff_page_down(visible_diff),
                    KeyCode::PageUp => app.diff_page_up(visible_diff),

                    _ => {}
                }
            }
        }
    }
    Ok(())
}
