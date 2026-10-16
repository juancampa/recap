use crate::app::{App, DisplayRow, KindFilter, Mode, Panel, RunState, SearchKind, SearchPhase};
use crate::reltime::align_relative_times;
use recap_api::{DiffLine, DiffSummary, EditSource, EmbedStatus, Entry, EntryFields};
use crate::search_history::SearchMode;
use crate::settings::{Settings, SETTINGS_ITEMS};
use crate::prompt::{
    self, HANG_INDENT, PROMPT_COLOR, RESPONSE_COLOR, WRAP_INDENT, WRAP_MARKER, WRAP_MARKER_COLOR,
};
use chrono::{DateTime, Local, NaiveDate};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState,
    },
    Frame,
};
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Rectangles for each rendered panel. Computed once per frame from the
/// terminal size; reused by the event loop to translate mouse coordinates
/// into panel-local positions. `projects` and `files` are `None` when their
/// respective panel is hidden (each shows only while focused).
#[derive(Debug, Clone, Copy)]
pub struct LayoutRects {
    pub projects: Option<Rect>,
    pub files: Option<Rect>,
    /// The Agents list. `None` outside Agent mode.
    pub agents: Option<Rect>,
    /// The History entry list (footer carved out below it).
    pub history: Rect,
    /// The footer box under the History list. Height 0 when the panel is too
    /// short to spare the room.
    pub history_footer: Rect,
    pub diff: Option<Rect>,
}

/// Split the History panel rect into (entry-list, footer). The footer takes a
/// fixed height off the bottom; when the panel is too short it collapses to
/// zero height and the list keeps the whole rect.
fn split_history_footer(history: Rect) -> (Rect, Rect) {
    if history.height <= HISTORY_FOOTER_HEIGHT + 2 {
        let empty = Rect {
            x: history.x,
            y: history.y + history.height,
            width: history.width,
            height: 0,
        };
        return (history, empty);
    }
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(HISTORY_FOOTER_HEIGHT)])
        .split(history);
    (parts[0], parts[1])
}

/// Rectangles for the current frame. The Projects and Files panels share one
/// slot on the left (each shows only while focused), so History and Diff never
/// shift when the user swaps between them. Agent mode adds the Agents panel
/// between that slot and History.
pub fn compute_layout(
    area: Rect,
    projects_active: bool,
    files_active: bool,
    agent_mode: bool,
) -> LayoutRects {
    let v_outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let content_area = v_outer[0];

    // Side panels (Projects / Files) share the same width.
    let side_width: u16 = 26 * 4;
    let (side, rest) = if projects_active || files_active {
        let h = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(side_width), Constraint::Min(0)])
            .split(content_area);
        (Some(h[0]), h[1])
    } else {
        // Both side panels are hidden — they waste space when not focused.
        (None, content_area)
    };

    let (agents, history_full, diff) = if agent_mode {
        let s = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(33),
                Constraint::Percentage(34),
                Constraint::Percentage(33),
            ])
            .split(rest);
        (Some(s[0]), s[1], s[2])
    } else {
        let s = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rest);
        (None, s[0], s[1])
    };
    let (history, history_footer) = split_history_footer(history_full);

    LayoutRects {
        projects: if projects_active { side } else { None },
        files: if files_active { side } else { None },
        agents,
        history,
        history_footer,
        diff: Some(diff),
    }
}

/// Vertically-aligned timestamp formatting shared by Projects and History.
/// Returns one string per input — same width across the batch in relative
/// mode (so units stack), or fixed `YYYY-MM-DD HH:MM` in absolute mode.
fn format_panel_timestamps(timestamps: &[DateTime<Local>], absolute: bool) -> Vec<String> {
    if absolute {
        timestamps
            .iter()
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
            .collect()
    } else {
        align_relative_times(timestamps)
    }
}

const ORANGE: Color = Color::Rgb(220, 120, 20);
/// Timestamp grays — de-emphasized vs the entry text, but the recency split
/// (this-epoch vs older) is kept as a light/dark gray so the cue survives.
const TIME_COLOR_RECENT: Color = Color::Rgb(165, 165, 165);
const TIME_COLOR_OLD: Color = Color::Rgb(110, 110, 110);
const CURSOR_COLOR: Color = Color::Rgb(120, 170, 255);
const CLAUDE_COLOR: Color = Color::Rgb(217, 119, 87);
const NEOVIM_COLOR: Color = Color::Rgb(120, 200, 100);
const SOURCE_WIDTH: usize = 6;
/// Green used for the "session still running" play glyph.
const RUNNING_COLOR: Color = Color::Rgb(90, 220, 120);

/// Color of the label of a compaction row. The orange differs from the
/// session colors and from the prompt and response colors, so the row is
/// easy to find in a long session.
const COMPACTION_COLOR: Color = Color::Rgb(255, 165, 80);
/// The glyph of a session that Claude Code deleted and recap archived.
const ARCHIVED_GLYPH: &str = "◌";

/// Width (cells) of the per-session color-bar column: a box-drawing bar plus
/// a trailing space.
const BAR_COL_WIDTH: usize = 2;
/// Cells reserved at the right edge of a row for the trailing "running" glyph.
pub const RUNNING_RESERVE: usize = 2;
/// The most lines that the session summary uses in the History footer.
const SUMMARY_LINES: usize = 3;
/// Height (rows) of the History footer box: 2 border lines, 4 field lines,
/// and [`SUMMARY_LINES`]. The height stays the same for an entry without a
/// summary, so the list above does not move when the selection changes.
pub const HISTORY_FOOTER_HEIGHT: u16 = 6 + SUMMARY_LINES as u16;

/// Sixteen bright, well-separated hues rotated across agent sessions so
/// consecutive-but-distinct sessions read apart at a glance. A session's
/// color is derived from a stable hash of its id (see [`session_color`]).
/// Sixteen rather than eight, because two neighboring sessions that draw the
/// same hue read as one session; the half-height end glyph of
/// [`session_bar_span`] is the other half of that fix.
const SESSION_COLORS: [Color; 16] = [
    Color::Rgb(235, 90, 90),   // red
    Color::Rgb(240, 150, 40),  // orange
    Color::Rgb(230, 210, 70),  // yellow
    Color::Rgb(90, 210, 110),  // green
    Color::Rgb(70, 200, 210),  // cyan
    Color::Rgb(110, 160, 255), // blue
    Color::Rgb(180, 130, 255), // purple
    Color::Rgb(240, 120, 200), // pink
    Color::Rgb(200, 80, 60),   // brick
    Color::Rgb(215, 180, 110), // sand
    Color::Rgb(160, 210, 60),  // lime
    Color::Rgb(60, 190, 150),  // teal
    Color::Rgb(90, 150, 200),  // steel
    Color::Rgb(140, 120, 230), // indigo
    Color::Rgb(220, 140, 160), // rose
    Color::Rgb(170, 170, 190), // slate
];

/// Stable FNV-1a hash of a session id, mapped into [`SESSION_COLORS`]. Stable
/// across runs (unlike `DefaultHasher`, which is fine here but we keep it
/// explicit) so a session keeps its color.
fn session_color(session_id: &str) -> Color {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in session_id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    SESSION_COLORS[(h as usize) % SESSION_COLORS.len()]
}

/// The per-session color-bar column shown just before the entry text. A
/// heavy vertical box-drawing bar in the session's color, or blank for
/// entries without a session (Cursor / Neovim).
///
/// On the last row of a session the bar becomes `╹`, which draws only the
/// top half of the cell. The gap that this leaves keeps the band apart from
/// the band of the next session, even when both sessions drew the same hue.
/// A half-height glyph, and not an underline in the row background: crossterm
/// writes the underline color as `CSI 58;2;r;g;b m`, and a terminal that only
/// reads the colon form of `58` ignores it (Ghostty, measured 2026-09-20).
fn session_bar_span(session_id: Option<&str>, last: bool) -> Span<'static> {
    match session_id {
        Some(id) => Span::styled(
            if last { "╹ " } else { "┃ " },
            Style::default()
                .fg(session_color(id))
                .add_modifier(Modifier::BOLD),
        ),
        None => Span::raw(" ".repeat(BAR_COL_WIDTH)),
    }
}

/// The bar drawn in the session column on the *continuation* lines of a
/// wrapped prompt, so the grouping band stays unbroken across the wrapped
/// rows instead of the bar showing only on the first line.
///
/// Uses a light vertical (`│`) rather than the first line's heavy `┃` — box
/// drawing verticals share the same cell axis, so the thin stroke reads as a
/// quieter continuation of the same band while sitting flush under the heavy
/// bar above. Entries with a known session keep that session's hue; entries
/// without one (Cursor / Neovim, or a session we never resolved) fall back to
/// a dotted gray line so the band is still visible without implying a color
/// identity we don't have.
fn continuation_bar_span(session_id: Option<&str>, last: bool) -> Span<'static> {
    match session_id {
        Some(id) => Span::styled(
            if last { "╵ " } else { "│ " },
            Style::default().fg(session_color(id)),
        ),
        None => Span::styled("┆ ", Style::default().fg(Color::Rgb(120, 120, 120))),
    }
}

/// Columns consumed by a History row's fixed prefix (before the project
/// chain / entry text): leading marker, timestamp, change column, source
/// label, and the session bar. Kept in one place so the prompt-wrapping
/// width used for row-height math ([`crate::app::App::recompute_row_heights`])
/// matches what the renderer lays out. Column order doesn't affect the total.
/// A prompt's `[N] ` count sits after this prefix; see
/// [`ordinal_prefix_width`].
pub fn history_prefix_cols(ts_width: usize, list_width: usize, settings: &Settings) -> usize {
    // leading(2) + ts + ' ' + change + ' ' + source + ' ' + title + ' ' + bar
    let mut cols = 2 + BAR_COL_WIDTH;
    if settings.show_time {
        cols += ts_width + 1;
    }
    if settings.show_change {
        cols += CHANGE_COL_WIDTH + 1;
    }
    if settings.show_source {
        cols += SOURCE_WIDTH + 1;
    }
    let title_width = title_col_width(list_width, settings);
    if title_width > 0 {
        cols += title_width + 1;
    }
    cols
}

/// Widest session-title column the panel draws, and the width below which
/// it drops the column. A title column must leave most of the row to the
/// entry text, so the width follows the width of the panel.
const TITLE_WIDTH_MAX: usize = 24;
const TITLE_WIDTH_MIN: usize = 10;

/// Width (cells) of the session-title column at a given panel width. A
/// longer title is cut and ends with an ellipsis, so the column never moves
/// the text after it. Returns 0 when the user hid the column, and when the
/// panel is too narrow to give the column its minimum width.
pub fn title_col_width(list_width: usize, settings: &Settings) -> usize {
    if !settings.show_title {
        return 0;
    }
    let width = list_width / 5;
    if width < TITLE_WIDTH_MIN {
        0
    } else {
        width.min(TITLE_WIDTH_MAX)
    }
}

/// Pad `s` to `width` cells, or cut it and end it with an ellipsis.
fn fit_width(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        return format!("{:<w$}", s, w = width);
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The session-title column of a History row. The panel prints the title
/// only on the first row of a run of rows that share one session, in that
/// session's color, so the column reads as the head of the block below it.
fn title_span(
    title: Option<&str>,
    session_id: Option<&str>,
    show: bool,
    width: usize,
) -> Span<'static> {
    let Some(title) = title.filter(|_| show) else {
        return Span::raw(" ".repeat(width));
    };
    let color = session_id.map(session_color).unwrap_or(Color::Gray);
    Span::styled(fit_width(title, width), Style::default().fg(color))
}

/// Total char width of a run of spans (for right-aligning a trailing column).
fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Append the trailing session glyph flush against the right edge of a row
/// of the given inner width: a green play glyph when the session runs, a gray
/// dotted circle when the session is archived. No-op for the other states.
fn append_run_state(spans: &mut Vec<Span<'static>>, inner_width: usize, state: RunState) {
    let glyph = match state {
        RunState::Running => Span::styled(
            "▶",
            Style::default()
                .fg(RUNNING_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
        RunState::Archived => Span::styled(ARCHIVED_GLYPH, Style::default().fg(Color::Gray)),
        RunState::Stopped | RunState::Unknown => return,
    };
    let used = spans_width(spans);
    // Park the glyph in the last column; keep at least one space of gap.
    let target = inner_width.saturating_sub(1);
    let pad = if target > used { target - used } else { 1 };
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(glyph);
}

/// Fixed-width (2 cell) indicator column shown between the source label and the
/// change column: a green play glyph when the entry's originating session is
/// still running, blanks otherwise (so the columns stay aligned across rows).
fn running_span(running: bool) -> Span<'static> {
    if running {
        Span::styled(
            "▶ ",
            Style::default().fg(RUNNING_COLOR).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    }
}

/// Two near-identical gray hues alternated across rows so consecutive rows
/// sharing a project chain visually band together.
const CHAIN_BG_A: Color = Color::Rgb(28, 28, 28);
const CHAIN_BG_B: Color = Color::Rgb(38, 38, 38);
/// Solid bg for the Projects/Diff panels and the History panel's empty
/// area; matches the lighter stripe so panels read as one piece.
const PANEL_BG: Color = CHAIN_BG_B;
/// Selection background — darker than ratatui's `DarkGray`, sitting roughly
/// halfway between it and the lighter band so selected rows stand out
/// without flashing against the band.
const SELECTED_BG: Color = Color::Rgb(60, 60, 60);

/// The glyphs of the project-column tree. A row prints a corner at the
/// deepest chain segment that repeats the row above, and a vertical line at
/// the root position above that corner. The corner is `TREE_END` on the last
/// row of the branch and `TREE_BRANCH` while more rows follow.
const TREE_BRANCH: char = '\u{251c}';
const TREE_END: char = '\u{2514}';
const TREE_LINE: char = '\u{2502}';
/// Indent of the tree glyph that stands in for the root project name. Two
/// columns, so a child row sits close under its root.
const TREE_INDENT: usize = 2;
/// The pad that carries the eye from the project chain to the text after it.
const PROJECT_FILL: char = '\u{2504}';
/// Separator between two project names in the project column.
const PROJECT_ARROW: char = '\u{2192}';
/// Blank columns between the project column and the text after it.
pub const PROJECT_GAP: usize = 2;
/// Narrowest project column: the indent plus the tree glyph.
pub const PROJECT_MIN_CHARS: usize = TREE_INDENT + 1;
/// Color of the tree glyphs and of the pad, one step below the names so the
/// rule does not compete with the project names.
const PROJECT_RULE_COLOR: Color = Color::Rgb(82, 82, 82);

/// The date row that separates the days: the teal of the active panel
/// border, on a background darker than either band, so the row reads as a
/// break rather than as another entry.
const DATE_ROW_COLOR: Color = Color::Cyan;
const DATE_ROW_BG: Color = Color::Rgb(8, 8, 10);

/// Replace a leading `$HOME` prefix with `~`.
fn home_relative(path: &Path) -> String {
    let s = path.to_string_lossy().into_owned();
    if let Some(home) = std::env::var_os("HOME") {
        let home_str = home.to_string_lossy();
        if !home_str.is_empty() {
            if let Some(rest) = s.strip_prefix(&*home_str) {
                return format!("~{}", rest);
            }
        }
    }
    s
}

fn source_label(src: EditSource) -> (&'static str, Color) {
    match src {
        EditSource::Cursor => ("cursor", CURSOR_COLOR),
        EditSource::ClaudeCode => ("claude", CLAUDE_COLOR),
        EditSource::Neovim => ("nvim", NEOVIM_COLOR),
    }
}

/// The source-label column ("claude" / "cursor" / "nvim"), in the color of
/// that source: blue for Cursor, orange for Claude Code, green for Neovim.
/// The source keeps its own palette, apart from the session hues of the
/// title column and the session bar, so the column says which tool wrote the
/// entry and the two other columns say which session. Blank when `show` is
/// false, so a run of consecutive rows sharing one session prints the label
/// only on the first row and the rest read as a continuation of the block.
fn source_span_for(entry: &Entry, show: bool, state: RunState) -> Span<'static> {
    if !show {
        return Span::raw(" ".repeat(SOURCE_WIDTH));
    }
    let (label, color) = source_label(entry.source);
    Span::styled(
        format!("{:<width$}", label, width = SOURCE_WIDTH),
        Style::default().fg(shade_for(color, state)),
    )
}

/// Darken `color` when the agent session has stopped. A source that reports
/// nothing ([`RunState::Unknown`]) keeps the bright color, because a dim label
/// there would claim knowledge the app does not have.
fn shade_for(color: Color, state: RunState) -> Color {
    match state {
        RunState::Stopped | RunState::Archived => dim_color(color),
        RunState::Running | RunState::Unknown => color,
    }
}

/// Multiply an RGB color toward black. Non-RGB colors pass through, since the
/// terminal owns their exact value.
fn dim_color(color: Color) -> Color {
    const FACTOR: f32 = 0.45;
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(
            (r as f32 * FACTOR) as u8,
            (g as f32 * FACTOR) as u8,
            (b as f32 * FACTOR) as u8,
        ),
        other => other,
    }
}

/// Columns the `[N] ` prompt-count prefix takes in front of the prompt text.
/// Zero when the prompt has no count (no agent session).
pub fn ordinal_prefix_width(ordinal: Option<usize>) -> usize {
    match ordinal {
        Some(n) => format!("[{}] ", n).chars().count(),
        None => 0,
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let projects_active = app.active_panel == Panel::Projects;
    let files_active = app.active_panel == Panel::Files;

    // Reserve 1 row at the bottom for help bar.
    let v_outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let content_area = v_outer[0];
    let help_area = v_outer[1];

    let rects = compute_layout(
        content_area,
        projects_active,
        files_active,
        app.mode == Mode::Agent,
    );

    if let Some(projects_area) = rects.projects {
        draw_projects(f, app, projects_area);
    }
    if let Some(files_area) = rects.files {
        draw_files(f, app, files_area);
    }
    if let Some(agents_area) = rects.agents {
        draw_agents(f, app, agents_area);
    }
    draw_history(f, app, rects.history);
    draw_history_footer(f, app, rects.history_footer);
    if let Some(diff_area) = rects.diff {
        draw_diff(f, app, diff_area);
    }
    draw_help(f, app, help_area);
    if app.settings_menu.open {
        draw_settings_menu(f, app, area);
    }
}

/// Draw the settings menu over the middle of the screen. The menu lists one
/// row per setting, with a check box that shows the current value.
fn draw_settings_menu(f: &mut Frame, app: &App, area: Rect) {
    let rows = SETTINGS_ITEMS.len() as u16;
    let width = 76.min(area.width.saturating_sub(4)).max(20);
    let height = (rows + 4).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);

    let block = Block::default()
        .title(" Settings ")
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(SETTINGS_ITEMS.len() + 1);
    for (i, item) in SETTINGS_ITEMS.iter().enumerate() {
        let on = app.settings.value(*item);
        let selected = i == app.settings_menu.selected;
        let mark = if on { "[x] " } else { "[ ] " };
        let label_style = if selected {
            Style::default()
                .fg(Color::White)
                .bg(SELECTED_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let mark_style = if on {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mark_style = if selected { mark_style.bg(SELECTED_BG) } else { mark_style };
        let hint_style = if selected {
            Style::default().fg(Color::DarkGray).bg(SELECTED_BG)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { " > " } else { "   " }, mark_style),
            Span::styled(mark.to_string(), mark_style),
            Span::styled(format!("{:<24}", item.label()), label_style),
            Span::styled(item.hint().to_string(), hint_style),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("   j/k", Style::default().fg(Color::White)),
        Span::styled(":move  ", Style::default().fg(Color::DarkGray)),
        Span::styled("space", Style::default().fg(Color::White)),
        Span::styled(":toggle  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::White)),
        Span::styled(":close", Style::default().fg(Color::DarkGray)),
    ]));

    f.render_widget(Paragraph::new(lines), inner);
}

/// Fixed-width label span for a footer field ("Session", "Dir", …).
fn footer_label(s: &str) -> Span<'static> {
    Span::styled(
        format!("{:<8}", s),
        Style::default().fg(Color::DarkGray),
    )
}

/// Shorten a session id (a long UUID) to something that fits the footer:
/// the first segment before `-`, or the first 8 chars.
fn short_session(id: &str) -> String {
    let head = id.split('-').next().unwrap_or(id);
    head.chars().take(8).collect()
}

/// Render the History footer: a box summarizing the selected entry — its
/// session, working directory, running state (with the `o` hint), and a
/// timestamp + change summary line.
fn draw_history_footer(f: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let block = Block::default()
        .title(" Selected ")
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::default().bg(PANEL_BG));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(entry) = app.current_entry() else {
        f.render_widget(
            Paragraph::new(Span::styled(
                "(no selection)",
                Style::default().fg(Color::DarkGray),
            )),
            inner,
        );
        return;
    };

    let entry_idx = app.current_entry_idx();
    let running = entry_idx.map(|i| app.is_entry_running(i)).unwrap_or(false);
    let archived = entry_idx.map(|i| app.entry_run_state(i)) == Some(RunState::Archived);
    let (src_label, src_color) = source_label(entry.source);

    let mut lines: Vec<Line> = Vec::with_capacity(4);

    // Session line: source + short session id (+ live status when running).
    let mut session_spans = vec![
        footer_label("Session"),
        Span::styled(format!("{} ", src_label), Style::default().fg(src_color)),
    ];
    match entry.session_id.as_deref() {
        Some(id) => {
            session_spans.push(session_bar_span(Some(id), false));
            session_spans.push(Span::styled(
                short_session(id),
                Style::default().fg(Color::White),
            ));
            if let Some(rs) = app.running.get(id) {
                if let Some(status) = &rs.status {
                    session_spans.push(Span::styled(
                        format!("  [{}]", status),
                        Style::default().fg(RUNNING_COLOR),
                    ));
                }
            }
        }
        None => session_spans.push(Span::styled(
            "—",
            Style::default().fg(Color::DarkGray),
        )),
    }
    lines.push(Line::from(session_spans));

    // The summary that Claude Code shows while the session is idle.
    if let Some(summary) = entry.session_summary.as_deref() {
        const LABEL_WIDTH: usize = 8;
        let width = (inner.width as usize).saturating_sub(LABEL_WIDTH).max(1);
        let wrapped = prompt::wrap(&prompt::highlight(summary, RESPONSE_COLOR), width, SUMMARY_LINES);
        for (i, line) in wrapped.iter().enumerate() {
            let mut spans = vec![if i == 0 {
                footer_label("Summary")
            } else {
                Span::raw(" ".repeat(LABEL_WIDTH))
            }];
            spans.extend(prompt_line_spans(line, None));
            lines.push(Line::from(spans));
        }
    }

    // Working directory.
    let dir = entry
        .session_cwd
        .clone()
        .or_else(|| entry.project_root().map(Path::to_path_buf));
    let dir_str = dir
        .map(|d| home_relative(&d))
        .unwrap_or_else(|| "—".to_string());
    lines.push(Line::from(vec![
        footer_label("Dir"),
        Span::styled(dir_str, Style::default().fg(Color::Gray)),
    ]));

    // Running state + `o` hint (only meaningful for entries with a session).
    if entry.session_id.is_some() {
        let hint_gray = Style::default().fg(Color::DarkGray);
        let key = Style::default().fg(Color::White);
        let status_line = if running {
            Line::from(vec![
                Span::styled(
                    "● Still Running",
                    Style::default().fg(RUNNING_COLOR).add_modifier(Modifier::BOLD),
                ),
                Span::styled("   press ", hint_gray),
                Span::styled("o", key),
                Span::styled(" to focus", hint_gray),
            ])
        } else if archived {
            Line::from(vec![
                Span::styled(
                    format!("{ARCHIVED_GLYPH} Archived"),
                    Style::default().fg(Color::Gray),
                ),
                Span::styled("   press ", hint_gray),
                Span::styled("o", key),
                Span::styled(" to restore and resume", hint_gray),
            ])
        } else {
            Line::from(vec![
                Span::styled("○ Not running", Style::default().fg(Color::Gray)),
                Span::styled("   press ", hint_gray),
                Span::styled("o", key),
                Span::styled(" to resume", hint_gray),
            ])
        };
        lines.push(status_line);
    } else {
        lines.push(Line::from(Span::styled(
            "no agent session to focus",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Timestamp + change summary (or "prompt").
    let time_str = entry.timestamp.format("%Y-%m-%d %H:%M:%S").to_string();
    let summary_str = if entry.is_prompt() {
        if entry.response_text().is_some() {
            "prompt + response".to_string()
        } else {
            "prompt".to_string()
        }
    } else if let Some(idx) = entry_idx {
        app.diff_summaries
            .get(&app.view.entries[idx].id)
            .map(|s| format!("+{} -{}", s.added, s.removed))
            .unwrap_or_default()
    } else {
        String::new()
    };
    lines.push(Line::from(vec![
        footer_label("When"),
        Span::styled(time_str, Style::default().fg(Color::Gray)),
        Span::raw("   "),
        Span::styled(summary_str, Style::default().fg(Color::DarkGray)),
    ]));

    f.render_widget(Paragraph::new(lines), inner);
}

/// Width of the change-indicator column (matches `+NNN -NNN`).
const CHANGE_COL_WIDTH: usize = 9;

/// Render the "+N -M" diff summary column. Returns a fixed-width 9-char block
/// of spans (or a blank placeholder while the summary is still loading).
fn summary_spans(summary: Option<&DiffSummary>) -> Vec<Span<'static>> {
    match summary {
        Some(s) => vec![
            Span::styled(
                format!("{:>4}", format!("+{}", s.added)),
                Style::default().fg(Color::Green),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{:>4}", format!("-{}", s.removed)),
                Style::default().fg(Color::Red),
            ),
        ],
        None => vec![Span::raw(" ".repeat(CHANGE_COL_WIDTH))],
    }
}

/// Render a multi-entry group's child count, right-aligned in yellow and
/// padded to the same width as `summary_spans`. The +/- numbers are
/// misleading on multi-groups (they reflect a single child), so the count
/// of edits is what we surface in their place.
fn count_spans(count: usize) -> Vec<Span<'static>> {
    vec![Span::styled(
        format!("{:>w$}", count, w = CHANGE_COL_WIDTH),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )]
}

fn border_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Render `text` (a slice of a longer string) with `match_style` applied to
/// any character whose char-index in the parent string is listed in
/// `match_indices`. `text_offset` is `text`'s start position (in chars) in
/// that parent string.
fn highlighted_spans(
    text: &str,
    base: Style,
    match_style: Style,
    match_indices: &[u32],
    text_offset: usize,
) -> Vec<Span<'static>> {
    if match_indices.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let len = text.chars().count();
    let local: Vec<usize> = match_indices
        .iter()
        .filter_map(|&i| {
            let i = i as usize;
            if i >= text_offset && i < text_offset + len {
                Some(i - text_offset)
            } else {
                None
            }
        })
        .collect();
    if local.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let local: std::collections::BTreeSet<usize> = local.into_iter().collect();

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut current_match = false;
    for (i, ch) in text.chars().enumerate() {
        let is_match = local.contains(&i);
        if !buf.is_empty() && is_match != current_match {
            let style = if current_match { match_style } else { base };
            spans.push(Span::styled(std::mem::take(&mut buf), style));
        }
        buf.push(ch);
        current_match = is_match;
    }
    if !buf.is_empty() {
        let style = if current_match { match_style } else { base };
        spans.push(Span::styled(buf, style));
    }
    spans
}

/// Split `text` into spans, restyling regions matched by `regex` with
/// `match_style` while keeping the rest in `base`. Zero-width matches are
/// skipped — `find_iter` already advances past them, so this is just a guard
/// against emitting empty spans.
fn regex_highlight_spans(
    text: &str,
    base: Style,
    match_style: Style,
    regex: &Regex,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut last_end = 0;
    for m in regex.find_iter(text) {
        if m.start() > last_end {
            spans.push(Span::styled(text[last_end..m.start()].to_string(), base));
        }
        if m.end() > m.start() {
            spans.push(Span::styled(text[m.start()..m.end()].to_string(), match_style));
            last_end = m.end();
        }
    }
    if last_end < text.len() {
        spans.push(Span::styled(text[last_end..].to_string(), base));
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), base));
    }
    spans
}

/// Spans for one wrapped prompt line: its indent, then the highlighted text.
///
/// The History, Agents and Diff panels all render prompt text through this,
/// so the indent rules, the soft-wrap marker and the Markdown colors stay
/// identical in every panel. Each panel supplies its own gutter in front of
/// the returned spans.
///
/// A soft-wrapped line draws [`WRAP_MARKER`] in its last indent column, which
/// tells it apart from a line that a real linebreak started.
fn prompt_line_spans(
    line: &prompt::WrappedLine,
    highlight: Option<(&Regex, Style)>,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.pieces.len() + 2);
    if line.continuation && line.indent > 0 {
        if line.indent > 1 {
            spans.push(Span::raw(" ".repeat(line.indent - 1)));
        }
        spans.push(Span::styled(
            WRAP_MARKER,
            Style::default().fg(WRAP_MARKER_COLOR),
        ));
    } else if line.indent > 0 {
        spans.push(Span::raw(" ".repeat(line.indent)));
    }
    match highlight {
        Some((re, style)) => spans.extend(overlay_regex(&line.pieces, re, style)),
        None => spans.extend(
            line.pieces
                .iter()
                .map(|p| Span::styled(p.text.clone(), p.style)),
        ),
    }
    spans
}

/// Restyle the runs of `pieces` that `re` matches, keeping the Markdown style
/// everywhere else. Used so a content (`f`) search still marks its hits
/// inside a syntax-highlighted prompt.
fn overlay_regex(
    pieces: &[prompt::Piece],
    re: &Regex,
    match_style: Style,
) -> Vec<Span<'static>> {
    let text: String = pieces.iter().map(|p| p.text.as_str()).collect();
    let ranges: Vec<(usize, usize)> = re
        .find_iter(&text)
        .filter(|m| m.end() > m.start())
        .map(|m| {
            (
                text[..m.start()].chars().count(),
                text[..m.end()].chars().count(),
            )
        })
        .collect();
    if ranges.is_empty() {
        return pieces
            .iter()
            .map(|p| Span::styled(p.text.clone(), p.style))
            .collect();
    }
    let matched = |abs: usize| ranges.iter().any(|(s, e)| abs >= *s && abs < *e);

    let mut out: Vec<Span<'static>> = Vec::new();
    let mut base = 0usize;
    for piece in pieces {
        let chars: Vec<char> = piece.text.chars().collect();
        let mut i = 0usize;
        while i < chars.len() {
            let hit = matched(base + i);
            let mut j = i + 1;
            while j < chars.len() && matched(base + j) == hit {
                j += 1;
            }
            out.push(Span::styled(
                chars[i..j].iter().collect::<String>(),
                if hit { match_style } else { piece.style },
            ));
            i = j;
        }
        base += chars.len();
    }
    out
}

/// Leading column for a history row. Multi-entry groups in auto-expand mode
/// show `+`/`-` to mark their collapsed/expanded state. Everything else
/// keeps the column blank so the timestamp stays aligned across rows.
fn leading_marker(is_group: bool, expanded: bool, auto_expand: bool) -> Vec<Span<'static>> {
    if !is_group || !auto_expand {
        return vec![Span::raw("  ")];
    }
    let glyph = if expanded { "- " } else { "+ " };
    vec![Span::styled(
        glyph.to_string(),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )]
}

/// Build the spans of a group-style row (timestamp, source, change column,
/// project chain, path, optional markers).
///
/// `chain_mask` is one bool per chain segment: false replaces the segment
/// with whitespace so a prefix shared with the previous visible row reads
/// as continuation. The very last masked segment additionally swaps its
/// trailing arrow for a `└` tree corner, so the next visible segment looks
/// like a branch dropping out of the row above. The row's banding bg is
/// painted by the caller on the whole ListItem.
/// Title of the agent session an entry belongs to, if the source records
/// one: the Cursor composer name, or the Claude Code `aiTitle`. `None` for
/// an unattributed edit and for a session that has no title yet.
fn session_title(entry: &Entry) -> Option<&str> {
    entry.session_title.as_deref()
}

/// Build the project column of one row.
///
/// `mask` marks each chain segment: `false` means the segment repeats the row
/// above. The column draws the repeated part as a tree instead of as blanks:
/// a vertical line at the root position, then one corner at the deepest
/// repeated segment. The corner extends with `fill` up to the first name that
/// the row still shows. `branch_last` picks the corner glyph.
///
/// `width` is the width the column pads to. It never depends on `mask`, so a
/// repeated chain keeps the text after it in place, and the row-height model
/// stays in step with the renderer.
///
/// Returns the column text and the range of chars that holds the project
/// names. The caller draws the rest of the column in the rule color.
fn project_column(
    chain: &[String],
    mask: &[bool],
    branch_last: bool,
    width: usize,
    fill: char,
) -> (String, usize, usize) {
    if chain.is_empty() {
        return (" ".repeat(width), 0, 0);
    }
    let last_repeated = mask.iter().rposition(|&visible| !visible);
    let root_is_glyph = !mask.first().copied().unwrap_or(true);

    // Lay the segments out first: the start column and the width of each
    // one. A repeated root takes one column plus its indent, which is what
    // makes a child row sit close under its project.
    let mut slots: Vec<(usize, usize)> = Vec::with_capacity(chain.len());
    let mut col = 0usize;
    for (i, name) in chain.iter().enumerate() {
        let (start, cell) = if i == 0 {
            if root_is_glyph {
                (TREE_INDENT, 1)
            } else {
                (0, name.chars().count())
            }
        } else {
            let sep = if i == 1 && root_is_glyph { 1 } else { 3 };
            (col + sep, name.chars().count())
        };
        slots.push((start, cell));
        col = start + cell;
    }
    let content = col;

    let mut cells: Vec<char> = vec![' '; content.max(width)];
    let first_visible = mask.iter().position(|&visible| visible);
    let name_start = first_visible.map(|i| slots[i].0).unwrap_or(content);

    for (i, name) in chain.iter().enumerate() {
        let (start, _) = slots[i];
        if mask.get(i).copied().unwrap_or(true) {
            for (j, ch) in name.chars().enumerate() {
                cells[start + j] = ch;
            }
            // A name after another name carries the arrow separator.
            if i > 0 && mask.get(i - 1).copied().unwrap_or(true) {
                cells[start - 2] = PROJECT_ARROW;
            }
        } else if Some(i) == last_repeated {
            cells[start] = if branch_last { TREE_END } else { TREE_BRANCH };
            // The corner reaches to one column before the first name the
            // row still shows, or to the end of the column.
            let stop = first_visible
                .map(|v| slots[v].0.saturating_sub(1))
                .unwrap_or(cells.len());
            for cell in cells.iter_mut().take(stop).skip(start + 1) {
                *cell = fill;
            }
        } else if i == 0 {
            cells[start] = TREE_LINE;
        }
    }

    // The pad that carries the eye from the last name to the text after the
    // column. One blank keeps the rule off the name.
    if first_visible.is_some() {
        for cell in cells.iter_mut().skip(content + 1) {
            *cell = fill;
        }
    }

    if cells.len() > width {
        let text: String = cells.into_iter().collect();
        return (fit_width(&text, width), name_start.min(width), width);
    }
    (cells.into_iter().collect(), name_start, content)
}

/// The project column of a continuation line of a wrapped prompt. The line
/// carries the vertical of the root branch, so the tree stays unbroken over
/// the wrapped lines.
fn project_column_continuation(chain_empty: bool, width: usize) -> String {
    if chain_empty || width < TREE_INDENT + 1 {
        return " ".repeat(width);
    }
    let mut text = " ".repeat(TREE_INDENT);
    text.push(TREE_LINE);
    text.push_str(&" ".repeat(width - TREE_INDENT - 1));
    text
}

/// Width (chars) of an entry's project chain as `project_column` renders it
/// with every segment visible: the names joined by ` → `.
fn chain_chars_of(entry: &Entry, names: &HashMap<PathBuf, String>) -> usize {
    let (chain, _) = entry.display_path_segments(names);
    if chain.is_empty() {
        0
    } else {
        // A fully repeated chain draws `  ├` in place of a short root
        // name, so the column never falls below the width of that glyph.
        (chain.iter().map(|n| n.chars().count()).sum::<usize>() + 3 * (chain.len() - 1))
            .max(PROJECT_MIN_CHARS)
    }
}

/// Name a day both ways: how long ago it was, and its date. The date row
/// carries both so the user can hide the time column and still read the day
/// of every entry.
fn day_label(day: NaiveDate, today: NaiveDate) -> String {
    let days = (today - day).num_days();
    let relative = match days {
        d if d <= 0 => "today".to_string(),
        1 => "yesterday".to_string(),
        d if d < 7 => format!("{d} days ago"),
        d if d < 14 => "last week".to_string(),
        d if d < 60 => format!("{} weeks ago", d / 7),
        d if d < 365 => format!("{} months ago", d / 30),
        d => format!("{} years ago", d / 365),
    };
    format!("{relative} - {}", day.format("%Y-%m-%d"))
}

/// The date row that the panel draws above the first entry of a day. The
/// label sits in the middle of the row, between two rules, on a background
/// darker than any entry row so the day break reads as a break.
fn date_row_line(day: NaiveDate, today: NaiveDate, inner_width: usize) -> Line<'static> {
    let label = format!(" {} ", day_label(day, today));
    let label_chars = label.chars().count();
    let rules = inner_width.saturating_sub(label_chars);
    let left = rules / 2;
    let right = rules - left;
    let base = Style::default().fg(DATE_ROW_COLOR).bg(DATE_ROW_BG);
    Line::from(vec![
        Span::styled("─".repeat(left), base),
        Span::styled(label, base.add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(right), base),
    ])
}

#[allow(clippy::too_many_arguments)]
fn group_row_line(
    entry: &Entry,
    project_names: &HashMap<PathBuf, String>,
    leading: Vec<Span<'static>>,
    ts: &str,
    ts_width: usize,
    time_color: Color,
    change_spans: Vec<Span<'static>>,
    match_indices: Option<&[u32]>,
    chain_mask: &[bool],
    show_head: bool,
    inner_width: usize,
    reserved_lines: usize,
    prompt_max: usize,
    session_name: Option<&str>,
    run_state: RunState,
    ordinal: Option<usize>,
    settings: &Settings,
    date_row: Option<Line<'static>>,
    // Width of the project column (`App::project_col_width`), 0 when the
    // panel draws no project column. `branch_last` picks the corner glyph of
    // the project tree. `bar_end` marks the last row of a session, where the
    // session bar draws its half-height end glyph.
    project_width: usize,
    branch_last: bool,
    bar_end: bool,
) -> Vec<Line<'static>> {
    // Fixed prefix: leading, timestamp, change column, source, title,
    // session bar. Source and title sit right before the bar so their
    // session-colored text leads directly into the matching band.
    let mut prefix: Vec<Span<'static>> = Vec::with_capacity(16);
    prefix.extend(leading);
    if settings.show_time {
        prefix.push(Span::styled(
            format!("{:<w$}", ts, w = ts_width),
            Style::default().fg(time_color),
        ));
        prefix.push(Span::raw(" "));
    }
    if settings.show_change {
        prefix.extend(change_spans);
        prefix.push(Span::raw(" "));
    }
    if settings.show_source {
        prefix.push(source_span_for(entry, show_head, run_state));
        prefix.push(Span::raw(" "));
    }
    let title_width = title_col_width(inner_width, settings);
    if title_width > 0 {
        prefix.push(title_span(
            session_name,
            entry.chain_id.as_deref(),
            show_head,
            title_width,
        ));
        prefix.push(Span::raw(" "));
    }
    prefix.push(session_bar_span(entry.chain_id.as_deref(), false));

    let (chain_names, rel) = entry.display_path_segments(project_names);
    let dim_base = Style::default().fg(Color::Gray);
    let file_base = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let match_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    // The project column: the chain drawn as a tree, then the pad that
    // carries the eye to the text. A row without a chain still takes the
    // column when the user aligns it, so the text after it stays in line.
    let project_col: Option<(String, usize, usize)> = if !settings.show_project
        || project_width == 0
    {
        None
    } else {
        Some(project_column(
            &chain_names,
            chain_mask,
            branch_last,
            project_width,
            if settings.project_fill {
                PROJECT_FILL
            } else {
                ' '
            },
        ))
    };
    let chain_chars = project_col
        .as_ref()
        .map(|(text, _, _)| text.chars().count() + PROJECT_GAP)
        .unwrap_or(0);
    let rule_style = Style::default().fg(PROJECT_RULE_COLOR);
    // The search records its match offsets over the full display path, which
    // always starts with the project chain. The path highlight therefore
    // counts from the full chain width, even when the panel hides the
    // project column.
    let chain_match_base = if chain_names.is_empty() {
        0
    } else {
        chain_names.iter().map(|n| n.chars().count()).sum::<usize>() + 3 * chain_names.len()
    };

    // Draw the project column: the names in gray, the pad in the rule color.
    // `match_idx` highlights the part of the search match that falls in the
    // names.
    let project_spans = |match_idx: Option<&[u32]>| -> Vec<Span<'static>> {
        let Some((text, name_start, name_end)) = project_col.as_ref() else {
            return Vec::new();
        };
        let byte_of = |chars: usize| -> usize {
            text.char_indices()
                .nth(chars)
                .map(|(b, _)| b)
                .unwrap_or(text.len())
        };
        let (head, rest) = text.split_at(byte_of(*name_start));
        let (names, pad) = rest.split_at(byte_of(*name_end) - head.len());
        let mut out: Vec<Span<'static>> = Vec::with_capacity(4);
        if !head.is_empty() {
            out.push(Span::styled(head.to_string(), rule_style));
        }
        // The search records its match offsets over the chain with every
        // name shown, which is also when the panel drops the tree.
        match match_idx {
            Some(idx) => out.extend(highlighted_spans(names, dim_base, match_style, idx, 0)),
            None => out.push(Span::styled(names.to_string(), dim_base)),
        }
        if !pad.is_empty() {
            out.push(Span::styled(pad.to_string(), rule_style));
        }
        out.push(Span::raw(" ".repeat(PROJECT_GAP)));
        out
    };

    // The date row of a day takes the first line of the row below it, so
    // the body of the row gets one line less.
    let body_lines = reserved_lines.saturating_sub(date_row.is_some() as usize).max(1);

    // ── Prompt entries: wrap the text over up to N rows. ──
    if entry.is_prompt() {
        // The prompt count sits in front of the text, in the timestamp gray.
        let ord_width = ordinal_prefix_width(ordinal);
        let ord_span = ordinal.map(|n| {
            Span::styled(format!("[{}] ", n), Style::default().fg(time_color))
        });
        // Column at which the prompt text begins on the first line.
        let text_start =
            history_prefix_cols(ts_width, inner_width, settings) + chain_chars + ord_width;
        let avail = inner_width
            .saturating_sub(text_start + RUNNING_RESERVE)
            .max(1);
        // Highlight the raw (multi-line) prompt text and the response as
        // Markdown, then wrap them. A real linebreak starts a fresh line; a
        // soft wrap is flagged so the indent below can mark it.
        let raw = entry.prompt_text().unwrap_or("");
        let mut wrapped =
            prompt::wrap_turn_highlighted(raw, entry.response_text(), avail, prompt_max);
        // Force the rendered line count to match the reserved height so the
        // scroll model and the drawn rows stay in lock-step.
        wrapped.truncate(body_lines);
        while wrapped.len() < body_lines {
            wrapped.push(prompt::WrappedLine {
                pieces: Vec::new(),
                indent: 0,
                continuation: false,
            });
        }

        // Column where the fixed session-bar sits (start of the bar column),
        // i.e. `text_start` minus everything that follows the bar.
        let bar_col = text_start - BAR_COL_WIDTH - chain_chars - ord_width;

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(wrapped.len() + 1);
        lines.extend(date_row);
        for (i, seg) in wrapped.iter().enumerate() {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(seg.pieces.len() + 6);
            let last_line = i + 1 == wrapped.len();
            if i == 0 {
                spans.extend(prefix.clone());
                // The bar is the last span of the prefix; end it when this
                // is also the last line of the row.
                if last_line && bar_end && spans.pop().is_some() {
                    spans.push(session_bar_span(entry.chain_id.as_deref(), true));
                }
                spans.extend(project_spans(None));
                if let Some(ord) = ord_span.clone() {
                    spans.push(ord);
                }
            } else {
                // Continuation line: redraw the grouping bar (thin) in the
                // session column so the band is unbroken, then re-indent past
                // the (blank) chain and count to line up with the first line.
                if bar_col > 0 {
                    spans.push(Span::raw(" ".repeat(bar_col)));
                }
                spans.push(continuation_bar_span(
                    entry.chain_id.as_deref(),
                    last_line && bar_end,
                ));
                if chain_chars > 0 {
                    spans.push(Span::styled(
                        project_column_continuation(
                            chain_names.is_empty(),
                            chain_chars - PROJECT_GAP,
                        ),
                        rule_style,
                    ));
                    spans.push(Span::raw(" ".repeat(PROJECT_GAP)));
                }
                if ord_width > 0 {
                    spans.push(Span::raw(" ".repeat(ord_width)));
                }
            }
            spans.extend(prompt_line_spans(seg, None));
            if i == 0 {
                append_run_state(&mut spans, inner_width, run_state);
            }
            lines.push(Line::from(spans));
        }
        return lines;
    }

    // ── Compaction entries: single line, chain + label. ──
    if entry.is_compaction() {
        let mut spans = prefix;
        if bar_end && spans.pop().is_some() {
            spans.push(session_bar_span(entry.chain_id.as_deref(), true));
        }
        spans.extend(project_spans(None));
        spans.push(Span::styled(rel, Style::default().fg(COMPACTION_COLOR).add_modifier(Modifier::ITALIC)));
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(2);
        lines.extend(date_row);
        lines.push(Line::from(spans));
        return lines;
    }

    // ── File-change entries: single line, chain + dir + filename. ──
    let mut spans = prefix;
    if bar_end && spans.pop().is_some() {
        spans.push(session_bar_span(entry.chain_id.as_deref(), true));
    }
    let split = rel.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (dir_part, file_part) = rel.split_at(split);
    let dir_chars = dir_part.chars().count();

    if let Some(idx) = match_indices {
        spans.extend(project_spans(Some(idx)));
        if !dir_part.is_empty() {
            spans.extend(highlighted_spans(
                dir_part,
                dim_base,
                match_style,
                idx,
                chain_match_base,
            ));
        }
        spans.extend(highlighted_spans(
            file_part,
            file_base,
            match_style,
            idx,
            chain_match_base + dir_chars,
        ));
    } else {
        spans.extend(project_spans(None));
        if !dir_part.is_empty() {
            spans.push(Span::styled(dir_part.to_string(), dim_base));
        }
        spans.push(Span::styled(file_part.to_string(), file_base));
    }

    append_run_state(&mut spans, inner_width, run_state);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(2);
    lines.extend(date_row);
    lines.push(Line::from(spans));
    lines
}

/// Compute per-row chain-mask (which segments to render) and chain-bg
/// (which of the two grays to use). Mask hides segments that match the
/// previous row's chain at the same position, stopping at the first
/// divergence. Bg flips between two grays whenever the chain changes.
/// `init_prev` and `init_bg_idx` carry over the alternation state from
/// rows that are above the visible window — without them the bands would
/// flip whenever scrolling moved a section break across the top edge.
/// `disable_mask` forces every segment visible (used during search so
/// matches in the chain stay legible).
///
/// The third result is one flag per row: the row holds the last corner of
/// its branch, so `project_column` draws `└` instead of `├`. A row holds the
/// last corner when the next row does not repeat the chain down to the same
/// depth. `next_after` is the chain of the row below the window, so the flag
/// of the bottom row stays correct as the user scrolls.
fn compute_chain_visuals(
    chains: &[Vec<String>],
    init_prev: Option<Vec<String>>,
    init_bg_idx: u8,
    disable_mask: bool,
    next_after: Option<Vec<String>>,
) -> (Vec<Vec<bool>>, Vec<Color>, Vec<bool>) {
    let mut masks: Vec<Vec<bool>> = Vec::with_capacity(chains.len());
    let mut bgs: Vec<Color> = Vec::with_capacity(chains.len());
    let mut prev_chain: Option<Vec<String>> = init_prev;
    let mut bg_idx: u8 = init_bg_idx & 1;

    for chain in chains {
        let same = prev_chain.as_deref() == Some(chain.as_slice());
        if !same && prev_chain.is_some() {
            bg_idx ^= 1;
        }
        bgs.push(if bg_idx == 0 { CHAIN_BG_A } else { CHAIN_BG_B });

        let mask: Vec<bool> = if disable_mask {
            vec![true; chain.len()]
        } else {
            match prev_chain.as_deref() {
                Some(prev) => {
                    let mut m = Vec::with_capacity(chain.len());
                    let mut diverged = false;
                    for (i, name) in chain.iter().enumerate() {
                        if !diverged && prev.get(i) == Some(name) {
                            m.push(false);
                        } else {
                            diverged = true;
                            m.push(true);
                        }
                    }
                    m
                }
                None => vec![true; chain.len()],
            }
        };
        masks.push(mask);
        prev_chain = Some(chain.clone());
    }

    let branch_last: Vec<bool> = masks
        .iter()
        .enumerate()
        .map(|(i, mask)| {
            let Some(depth) = mask.iter().rposition(|&visible| !visible) else {
                return true;
            };
            let next = match chains.get(i + 1) {
                Some(next) => Some(next),
                None => next_after.as_ref(),
            };
            match next {
                Some(next) => next.get(..=depth) != chains[i].get(..=depth),
                None => true,
            }
        })
        .collect();
    (masks, bgs, branch_last)
}

/// Render a per-child diff separator: timestamp, source, +/-, project chain,
/// path. No column padding — fields are space-separated. The whole row gets
/// a dark gray background that fills the panel width to read as a header.
fn separator_line(
    entry: &Entry,
    project_names: &HashMap<PathBuf, String>,
    ts: &str,
    summary: Option<&DiffSummary>,
    session_name: Option<&str>,
    inner_width: usize,
) -> Line<'static> {
    let bg = CHAIN_BG_A;
    let bg_only = Style::default().bg(bg);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(16);
    let mut chars: usize = 0;

    let push = |spans: &mut Vec<Span<'static>>, chars: &mut usize, text: String, fg: Option<Color>, bold: bool| {
        *chars += text.chars().count();
        let mut style = Style::default().bg(bg);
        if let Some(c) = fg {
            style = style.fg(c);
        }
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(text, style));
    };

    push(&mut spans, &mut chars, ts.to_string(), Some(Color::Yellow), true);
    push(&mut spans, &mut chars, " ".to_string(), None, false);

    let (label, color) = source_label(entry.source);
    push(&mut spans, &mut chars, label.to_string(), Some(color), false);
    push(&mut spans, &mut chars, " ".to_string(), None, false);

    if let Some(s) = summary {
        push(&mut spans, &mut chars, format!("+{}", s.added), Some(Color::Green), false);
        push(&mut spans, &mut chars, " ".to_string(), None, false);
        push(&mut spans, &mut chars, format!("-{}", s.removed), Some(Color::Red), false);
        push(&mut spans, &mut chars, " ".to_string(), None, false);
    }

    let (chain_names, rel) = entry.display_path_segments(project_names);
    if !chain_names.is_empty() {
        push(
            &mut spans,
            &mut chars,
            format!("{} → ", chain_names.join(" → ")),
            Some(Color::Gray),
            false,
        );
    }
    let split = rel.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (dir_part, file_part) = rel.split_at(split);
    if !dir_part.is_empty() {
        push(&mut spans, &mut chars, dir_part.to_string(), Some(Color::Gray), false);
    }
    push(&mut spans, &mut chars, file_part.to_string(), Some(Color::White), true);

    // Cursor edits carry the composer (agent session) that produced them;
    // label it in that session's color so concurrent agents stay legible.
    if let Some(name) = session_name {
        push(&mut spans, &mut chars, "  ".to_string(), None, false);
        let color = entry
            .session_id
            .as_deref()
            .map(session_color)
            .unwrap_or(Color::Gray);
        push(&mut spans, &mut chars, format!("⟨{}⟩", name), Some(color), false);
    }

    if chars < inner_width {
        spans.push(Span::styled(" ".repeat(inner_width - chars), bg_only));
    }
    Line::from(spans)
}

/// The help bar text for the semantic search state, or `None` while the
/// state needs no report.
fn embed_label(status: &EmbedStatus) -> Option<String> {
    match status {
        EmbedStatus::Off | EmbedStatus::Ready => None,
        EmbedStatus::Starting => Some("  [embed: starting…]".to_string()),
        EmbedStatus::Indexing { done, total } => Some(format!("  [embedding {done}/{total}]")),
        EmbedStatus::Failed { error } => Some(format!("  [embed: {error}]")),
    }
}

fn draw_projects(f: &mut Frame, app: &App, area: Rect) {
    let active = app.active_panel == Panel::Projects;
    let block = Block::default()
        .title(format!(" Projects ({}) ", app.view.projects.len()))
        .borders(Borders::ALL)
        .border_type(if active {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(border_style(active))
        .style(Style::default().bg(PANEL_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible = inner.height as usize;
    let total_rows = app.view.projects.len() + 1;
    let sel_display = app.selected_project_idx.map(|i| i + 1).unwrap_or(0);

    let first_visible_row = app.project_scroll.min(total_rows);
    let last_visible_row = (app.project_scroll + visible).min(total_rows);

    // Project indices touched by the visible window. Row 0 is "All projects"
    // and has no project, so it doesn't participate in alignment.
    let first_proj = if first_visible_row == 0 {
        0
    } else {
        first_visible_row - 1
    };
    let last_proj = last_visible_row.saturating_sub(1);

    let visible_timestamps: Vec<DateTime<Local>> = app.view.projects[first_proj..last_proj]
        .iter()
        .map(|p| p.latest)
        .collect();
    let formatted_ts = format_panel_timestamps(&visible_timestamps, app.absolute_time);
    let ts_width = formatted_ts.first().map(|s| s.chars().count()).unwrap_or(0);

    let search_active = app.project_search.phase != SearchPhase::Inactive
        && app.project_search.highlights
        && !app.project_search.matches.is_empty();
    let match_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let mut items: Vec<ListItem> = Vec::with_capacity(last_visible_row - first_visible_row);
    for row_idx in first_visible_row..last_visible_row {
        let is_match_row = row_idx != 0
            && search_active
            && app.project_search.match_pos_for(row_idx - 1).is_some();
        let line = if row_idx == 0 {
            let all_selected = sel_display == 0;
            Line::from(vec![Span::styled(
                " All projects",
                Style::default()
                    .fg(if all_selected { Color::White } else { Color::Gray })
                    .add_modifier(if all_selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )])
        } else {
            let i = row_idx - 1;
            let proj = &app.view.projects[i];
            let selected = sel_display == row_idx;
            let chain = if proj.chain.is_empty() {
                App::project_label(&proj.root)
            } else {
                proj.chain
                    .iter()
                    .map(|p| recap_api::project_name(p, &app.view.project_names))
                    .collect::<Vec<_>>()
                    .join(" → ")
            };
            let count = proj.count;
            let count_str = format!("({})", count);
            let ts = formatted_ts
                .get(i - first_proj)
                .cloned()
                .unwrap_or_default();

            let ts_chars = ts_width;
            let count_chars = count_str.chars().count();
            let total = inner.width as usize;
            let label_budget = total.saturating_sub(ts_chars + count_chars + 3);

            let label_chars = chain.chars().count();
            let truncated = label_chars > label_budget;
            let chain_render = if !truncated {
                chain.clone()
            } else {
                let chars: Vec<char> = chain.chars().collect();
                let take = label_budget.saturating_sub(1);
                let skip = chars.len() - take;
                let truncated: String = chars[skip..].iter().collect();
                format!("…{}", truncated)
            };

            let chain_chars_render = chain_render.chars().count();
            let padding = total
                .saturating_sub(chain_chars_render + count_chars + ts_chars + 3);

            let fg = if selected { Color::White } else { Color::Gray };
            let bold = if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            };

            // Per-char match highlighting only when the rendered label hasn't
            // been left-truncated — otherwise the char indices the matcher
            // returned no longer line up with what's on screen.
            let match_indices = if search_active && !truncated {
                app.project_search
                    .match_pos_for(i)
                    .map(|pos| app.project_search.matches[pos].indices.as_slice())
            } else {
                None
            };

            let label_style = Style::default().fg(fg).add_modifier(bold);
            let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
            if let Some(idx) = match_indices {
                spans.extend(highlighted_spans(&chain_render, label_style, match_style, idx, 0));
            } else {
                spans.push(Span::styled(chain_render, label_style));
            }
            spans.push(Span::raw(" "));
            spans.push(Span::styled(count_str, Style::default().fg(Color::DarkGray)));
            spans.push(Span::raw(" ".repeat(padding)));
            spans.push(Span::styled(ts, Style::default().fg(Color::DarkGray)));
            Line::from(spans)
        };

        let is_selected_row = row_idx == sel_display;
        let mut style = if is_selected_row {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default()
        };
        // While a search is active, dim rows that aren't matches so the hits
        // stand out (selected row stays full-brightness).
        if search_active && !is_match_row && !is_selected_row && row_idx != 0 {
            style = style.add_modifier(Modifier::DIM);
        }
        items.push(ListItem::new(line).style(style));
    }

    let list = List::new(items);
    f.render_widget(list, inner);

    // Scrollbar
    let total = app.view.projects.len() + 1;
    if total > visible {
        let mut sb = ScrollbarState::new(total).position(app.project_scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut sb,
        );
    }
}

/// Render the file path for a Files-panel row. When a project is selected,
/// strip the deepest project's prefix (the chain is already implied by the
/// active project filter); otherwise show the full `project chain → rel` so
/// rows from different projects stay distinguishable.
///
/// Exposed so the Files-panel search can fuzzy-match against the same label
/// the user sees on screen, rather than the raw path.
pub fn file_display_for_panel(
    file: &Path,
    chain: &[PathBuf],
    project_names: &HashMap<PathBuf, String>,
    project_selected: bool,
) -> String {
    if project_selected {
        if let Some(deepest) = chain.last() {
            if let Ok(rel) = file.strip_prefix(deepest) {
                return rel.to_string_lossy().into_owned();
            }
        }
        return file.to_string_lossy().into_owned();
    }

    let names: Vec<String> = chain
        .iter()
        .map(|p| match project_names.get(p) {
            Some(s) => s.clone(),
            None => p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string(),
        })
        .collect();
    let rel = chain
        .last()
        .and_then(|deepest| file.strip_prefix(deepest).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.to_string_lossy().into_owned());

    if names.is_empty() {
        rel
    } else {
        format!("{} → {}", names.join(" → "), rel)
    }
}

fn draw_files(f: &mut Frame, app: &App, area: Rect) {
    let active = app.active_panel == Panel::Files;
    let block = Block::default()
        .title(format!(" Files ({}) ", app.files.len()))
        .borders(Borders::ALL)
        .border_type(if active {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(border_style(active))
        .style(Style::default().bg(PANEL_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible = inner.height as usize;
    let total_rows = app.files.len() + 1;

    // Display index of the current selection: 0 = "All files", i+1 = files[i].
    let sel_display = match &app.selected_file_path {
        None => 0,
        Some(f) => app
            .files
            .iter()
            .position(|p| &p.path == f)
            .map(|i| i + 1)
            .unwrap_or(0),
    };

    let first_visible_row = app.file_scroll.min(total_rows);
    let last_visible_row = (app.file_scroll + visible).min(total_rows);

    // File indices touched by the visible window. Row 0 is "All files" and
    // has no file, so it doesn't participate in timestamp alignment.
    let first_file = if first_visible_row == 0 {
        0
    } else {
        first_visible_row - 1
    };
    let last_file = last_visible_row.saturating_sub(1);

    let visible_timestamps: Vec<DateTime<Local>> = app.files[first_file..last_file]
        .iter()
        .map(|p| p.latest)
        .collect();
    let formatted_ts = format_panel_timestamps(&visible_timestamps, app.absolute_time);
    let ts_width = formatted_ts.first().map(|s| s.chars().count()).unwrap_or(0);

    let project_selected = app.selected_project_idx.is_some();

    let search_active = app.file_search.phase != SearchPhase::Inactive
        && app.file_search.highlights
        && !app.file_search.matches.is_empty();
    let match_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let mut items: Vec<ListItem> = Vec::with_capacity(last_visible_row - first_visible_row);
    for row_idx in first_visible_row..last_visible_row {
        let is_match_row = row_idx != 0
            && search_active
            && app.file_search.match_pos_for(row_idx - 1).is_some();
        let line = if row_idx == 0 {
            let all_selected = sel_display == 0;
            Line::from(vec![Span::styled(
                " All files",
                Style::default()
                    .fg(if all_selected { Color::White } else { Color::Gray })
                    .add_modifier(if all_selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )])
        } else {
            let i = row_idx - 1;
            let file = &app.files[i].path;
            let selected = sel_display == row_idx;
            let chain = &app.files[i].chain;
            let label =
                file_display_for_panel(file, chain, &app.view.project_names, project_selected);
            let count = app.files[i].count;
            let count_str = format!("({})", count);
            let ts = formatted_ts
                .get(i - first_file)
                .cloned()
                .unwrap_or_default();

            let ts_chars = ts_width;
            let count_chars = count_str.chars().count();
            let total = inner.width as usize;
            let label_budget = total.saturating_sub(ts_chars + count_chars + 3);

            // File paths are commonly longer than the panel — truncate on the
            // left so the filename (the most informative part) stays visible.
            let label_chars = label.chars().count();
            let truncated = label_chars > label_budget;
            let label_render = if !truncated {
                label.clone()
            } else {
                let chars: Vec<char> = label.chars().collect();
                let take = label_budget.saturating_sub(1);
                let skip = chars.len() - take;
                let truncated: String = chars[skip..].iter().collect();
                format!("…{}", truncated)
            };

            let label_chars_render = label_render.chars().count();
            let padding = total
                .saturating_sub(label_chars_render + count_chars + ts_chars + 3);

            let fg = if selected { Color::White } else { Color::Gray };
            let bold = if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            };

            // Skip per-char highlighting when the label was left-truncated —
            // the matcher's char indices were computed against the full
            // string and would point at the wrong columns.
            let match_indices = if search_active && !truncated {
                app.file_search
                    .match_pos_for(i)
                    .map(|pos| app.file_search.matches[pos].indices.as_slice())
            } else {
                None
            };

            let label_style = Style::default().fg(fg).add_modifier(bold);
            let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
            if let Some(idx) = match_indices {
                spans.extend(highlighted_spans(&label_render, label_style, match_style, idx, 0));
            } else {
                spans.push(Span::styled(label_render, label_style));
            }
            spans.push(Span::raw(" "));
            spans.push(Span::styled(count_str, Style::default().fg(Color::DarkGray)));
            spans.push(Span::raw(" ".repeat(padding)));
            spans.push(Span::styled(ts, Style::default().fg(Color::DarkGray)));
            Line::from(spans)
        };

        let is_selected_row = row_idx == sel_display;
        let mut style = if is_selected_row {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default()
        };
        if search_active && !is_match_row && !is_selected_row && row_idx != 0 {
            style = style.add_modifier(Modifier::DIM);
        }
        items.push(ListItem::new(line).style(style));
    }

    let list = List::new(items);
    f.render_widget(list, inner);

    if total_rows > visible {
        let mut sb = ScrollbarState::new(total_rows).position(app.file_scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut sb,
        );
    }
}

/// Render the Agents panel: one row per agent session, newest first. Each row
/// carries the time of the agent's last prompt, its prompt count, its source,
/// its project chain, and its last prompt. Agent mode only.
fn draw_agents(f: &mut Frame, app: &App, area: Rect) {
    let active = app.active_panel == Panel::Agents;
    let block = Block::default()
        .title(format!(" Agents ({}) ", app.agents.len()))
        .borders(Borders::ALL)
        .border_type(if active {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(border_style(active))
        .style(Style::default().bg(PANEL_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible = inner.height as usize;
    let inner_width = inner.width as usize;
    let first = app.agent_scroll.min(app.agents.len());
    let last = (first + visible).min(app.agents.len());
    let sel = app.sel_agent_idx();

    let timestamps: Vec<DateTime<Local>> =
        app.agents[first..last].iter().map(|a| a.last_ts).collect();
    let formatted_ts = format_panel_timestamps(&timestamps, app.absolute_time);
    let ts_width = formatted_ts.first().map(|s| s.chars().count()).unwrap_or(0);

    let items: Vec<ListItem> = (first..last)
        .map(|i| {
            let agent = &app.agents[i];
            let state = app.run_state_of(agent.source, Some(&agent.session_id), agent.archived);

            let mut spans: Vec<Span<'static>> = Vec::with_capacity(10);
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("{:<w$}", formatted_ts.get(i - first).cloned().unwrap_or_default(), w = ts_width),
                Style::default().fg(TIME_COLOR_RECENT),
            ));
            spans.push(Span::raw(" "));
            // Prompt count, in the same column the History panel uses for its
            // change counts.
            spans.push(Span::styled(
                format!("{:>w$}", agent.prompt_count, w = CHANGE_COL_WIDTH),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" "));
            let (label, _) = source_label(agent.source);
            let src_color = shade_for(session_color(&agent.chain_id), state);
            spans.push(Span::styled(
                format!("{:<w$}", label, w = SOURCE_WIDTH),
                Style::default().fg(src_color),
            ));
            spans.push(Span::raw(" "));
            spans.push(session_bar_span(Some(&agent.chain_id), false));

            let chain: Vec<String> = agent
                .project_roots
                .iter()
                .map(|p| match app.view.project_names.get(p) {
                    Some(s) => s.clone(),
                    None => p
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("?")
                        .to_string(),
                })
                .collect();
            if !chain.is_empty() {
                spans.push(Span::styled(
                    format!("{} → ", chain.join(" → ")),
                    Style::default().fg(Color::Gray),
                ));
            }

            // The last prompt fills whatever room is left on the row, with
            // the same Markdown highlighting the other panels apply.
            let used = spans_width(&spans);
            let budget = inner_width.saturating_sub(used + RUNNING_RESERVE).max(1);
            let hl = prompt::highlight(&agent.last_prompt, PROMPT_COLOR);
            let empty: Vec<prompt::Piece> = Vec::new();
            let one = prompt::wrap_line(hl.first().unwrap_or(&empty), budget, false, 1);
            spans.extend(prompt_line_spans(&one[0], None));
            append_run_state(&mut spans, inner_width, state);

            let mut style = Style::default();
            if sel == Some(i) {
                style = style.bg(SELECTED_BG);
            }
            ListItem::new(Line::from(spans)).style(style)
        })
        .collect();

    f.render_widget(List::new(items), inner);

    if app.agents.len() > visible {
        let mut sb = ScrollbarState::new(app.agents.len()).position(app.agent_scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut sb,
        );
    }
}

fn draw_history(f: &mut Frame, app: &App, area: Rect) {
    let active = app.active_panel == Panel::History;
    let entry_count: usize = app.groups.iter().map(|g| g.entry_indices.len()).sum();
    // The title names the kind filter, because the count means something else
    // under each one. `all` is the default, so it needs no name.
    let title = match app.kind_filter {
        KindFilter::All => format!(" History ({entry_count}) "),
        filter => format!(" History ({entry_count} {}) ", filter.label()),
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(if active {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(border_style(active));

    let mut inner = block.inner(area);
    f.render_widget(block, area);

    // The date row names each day relative to the current day.
    let today = Local::now().date_naive();

    // The sticky date row: when the top row has no date row of its own, the
    // top line shows the day of the top row, and the rows start below it.
    if let Some(day) = app.sticky_date()
        && inner.height > 0
    {
        let top = Rect { height: 1, ..inner };
        f.render_widget(Paragraph::new(date_row_line(day, today, inner.width as usize)), top);
        inner.y += 1;
        inner.height -= 1;
    }

    let visible = inner.height as usize;
    let inner_width = inner.width as usize;
    let epochs = app.group_epochs();
    let sel_row = app.sel_display_row();
    let search_highlight = app.search.phase != SearchPhase::Inactive
        && app.search.highlights
        && !app.search.matches.is_empty();
    let match_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    // Rows are variable-height (a prompt can span several lines), so take as
    // many display rows as fit in `visible` terminal lines rather than a
    // fixed count.
    let render_count = {
        let mut count = 0usize;
        let mut used = 0usize;
        for idx in app.history_scroll..app.display_rows.len() {
            if used >= visible {
                break;
            }
            count += 1;
            used += app.row_height(idx);
        }
        count
    };

    let visible_timestamps: Vec<DateTime<Local>> = app
        .display_rows
        .iter()
        .skip(app.history_scroll)
        .take(render_count)
        .map(|row| {
            let entry_idx = match row {
                DisplayRow::Group(gi) => app.groups[*gi].entry_indices[0],
                DisplayRow::Child {
                    group_idx,
                    child_local,
                } => app.groups[*group_idx].entry_indices[*child_local],
            };
            app.view.entries[entry_idx].timestamp
        })
        .collect();
    let formatted_ts = format_panel_timestamps(&visible_timestamps, app.absolute_time);
    let ts_width = formatted_ts.first().map(|s| s.chars().count()).unwrap_or(0);

    // Per visible row: project chain (used for prefix-masking against the
    // previous row, and for picking the alternating chain-bg hue). Child
    // rows borrow their parent group's chain so the alternation stays in
    // sync even though the chain isn't drawn on a child line.
    //
    // To keep banding/masking stable while the user scrolls, we walk
    // `display_rows[..history_scroll]` and replay the alternation off-screen,
    // arriving at an anchor `(prev_chain, bg_idx)` to seed the visible run.
    // Without this, scrolling a section break across the top edge would
    // flip every group's color.
    let row_chain = |row: &DisplayRow| -> Vec<String> {
        let group_idx = match row {
            DisplayRow::Group(gi) => *gi,
            DisplayRow::Child { group_idx, .. } => *group_idx,
        };
        let entry_idx = app.groups[group_idx].entry_indices[0];
        let entry = &app.view.entries[entry_idx];
        let (chain, _) = entry.display_path_segments(&app.view.project_names);
        chain
    };

    let mut anchor_prev: Option<Vec<String>> = None;
    let mut anchor_bg_idx: u8 = 0;
    for row in &app.display_rows[..app.history_scroll.min(app.display_rows.len())] {
        let chain = row_chain(row);
        let same = anchor_prev.as_deref() == Some(chain.as_slice());
        if !same && anchor_prev.is_some() {
            anchor_bg_idx ^= 1;
        }
        anchor_prev = Some(chain);
    }

    let visible_chains: Vec<Vec<String>> = app
        .display_rows
        .iter()
        .skip(app.history_scroll)
        .take(render_count)
        .map(&row_chain)
        .collect();
    let (chain_masks, chain_bgs, branch_last_flags) = compute_chain_visuals(
        &visible_chains,
        anchor_prev,
        anchor_bg_idx,
        search_highlight,
        app.display_rows
            .get(app.history_scroll + render_count)
            .map(&row_chain),
    );

    // Per visible row: whether to print the source label. A run of consecutive
    // rows sharing one session shows the label only on its first row (the rest
    // read as a continuation of the same block, tied together by the bar).
    // Rows without a session never dedup — they have no bar, so hiding the
    // label would drop the only cue. The previous row's session for the top of
    // the window comes from the row just above the scroll, so the dedup is
    // stable as rows scroll across the top edge.
    let row_session = |row: &DisplayRow| -> Option<String> {
        let entry_idx = match row {
            DisplayRow::Group(gi) => app.groups[*gi].entry_indices[0],
            DisplayRow::Child {
                group_idx,
                child_local,
            } => app.groups[*group_idx].entry_indices[*child_local],
        };
        app.view.entries[entry_idx].chain_id.clone()
    };
    let mut prev_session: Option<String> = app
        .history_scroll
        .checked_sub(1)
        .and_then(|i| app.display_rows.get(i))
        .and_then(&row_session);
    let show_source_flags: Vec<bool> = app
        .display_rows
        .iter()
        .skip(app.history_scroll)
        .take(render_count)
        .map(|row| {
            let sess = row_session(row);
            let show = match &sess {
                None => true,
                Some(id) => prev_session.as_deref() != Some(id.as_str()),
            };
            prev_session = sess;
            show
        })
        .collect();

    // Per visible row: whether it is the last row of its session. The bar of
    // that row draws its half-height end glyph, so the band stops there
    // instead of joining the band of the next session.
    let session_end_flags: Vec<bool> = (app.history_scroll
        ..(app.history_scroll + render_count).min(app.display_rows.len()))
        .map(|idx| {
            let sess = app.display_rows.get(idx).and_then(&row_session);
            let next = app.display_rows.get(idx + 1).and_then(&row_session);
            sess.is_some() && sess != next
        })
        .collect();

    let items: Vec<ListItem> = app
        .display_rows
        .iter()
        .enumerate()
        .skip(app.history_scroll)
        .take(render_count)
        .map(|(row_idx, row)| {
            let is_selected = row_idx == sel_row;
            let entry_idx = match row {
                DisplayRow::Group(gi) => app.groups[*gi].entry_indices[0],
                DisplayRow::Child {
                    group_idx,
                    child_local,
                } => app.groups[*group_idx].entry_indices[*child_local],
            };
            let match_indices = if search_highlight {
                app.search
                    .match_pos_for(entry_idx)
                    .map(|pos| app.search.matches[pos].indices.as_slice())
            } else {
                None
            };
            let row_dim = search_highlight && match_indices.is_none() && !is_selected;
            let ts = formatted_ts
                .get(row_idx - app.history_scroll)
                .cloned()
                .unwrap_or_default();
            let local_idx = row_idx - app.history_scroll;
            // The row background: the alternating band, or the selection.
            let row_bg = if is_selected {
                SELECTED_BG
            } else {
                chain_bgs.get(local_idx).copied().unwrap_or(CHAIN_BG_A)
            };
            // The first row of a day carries the date row above it.
            let date_row = app.row_starts_day(row_idx).then(|| {
                date_row_line(
                    app.view.entries[entry_idx].timestamp.date_naive(),
                    today,
                    inner_width,
                )
            });

            let lines: Vec<Line> = match row {
                DisplayRow::Group(gi) => {
                    let group = &app.groups[*gi];
                    let is_group = group.entry_indices.len() > 1;
                    let entry = &app.view.entries[entry_idx];
                    let count = group.entry_indices.len();
                    let time_color = if epochs[*gi] == 0 {
                        TIME_COLOR_RECENT
                    } else {
                        TIME_COLOR_OLD
                    };
                    let summary = app.diff_summaries.get(&app.view.entries[entry_idx].id);

                    let leading = leading_marker(is_group, group.expanded, app.effective_auto_expand());
                    // Multi-entry groups display the child count instead of
                    // +/-, since +/- on a group reflects only one child.
                    // Prompts and compactions leave the column blank (no
                    // marker).
                    let change_spans = if is_group {
                        count_spans(count)
                    } else if !entry.is_file_change() {
                        vec![Span::raw(" ".repeat(CHANGE_COL_WIDTH))]
                    } else {
                        summary_spans(summary)
                    };

                    let mask = chain_masks
                        .get(local_idx)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    let show_source = show_source_flags.get(local_idx).copied().unwrap_or(true);
                    // Only title the collapsed single-file row; a multi-entry
                    // group's children carry their own per-edit titles.
                    let session_name = if is_group {
                        None
                    } else {
                        session_title(entry)
                    };
                    group_row_line(
                        entry,
                        &app.view.project_names,
                        leading,
                        &ts,
                        ts_width,
                        time_color,
                        change_spans,
                        match_indices,
                        mask,
                        show_source,
                        inner_width,
                        app.row_height(row_idx),
                        app.prompt_lines,
                        session_name,
                        app.entry_run_state(entry_idx),
                        app.prompt_ordinal(entry),
                        &app.settings,
                        date_row,
                        app.project_col_width(chain_chars_of(entry, &app.view.project_names)),
                        branch_last_flags.get(local_idx).copied().unwrap_or(true),
                        session_end_flags.get(local_idx).copied().unwrap_or(false),
                    )
                }
                DisplayRow::Child { .. } => {
                    let entry = &app.view.entries[entry_idx];
                    let fname = entry.file_name();
                    let time_color = match row {
                        DisplayRow::Child { group_idx, .. } => {
                            if epochs[*group_idx] == 0 {
                                TIME_COLOR_RECENT
                            } else {
                                TIME_COLOR_OLD
                            }
                        }
                        _ => TIME_COLOR_RECENT,
                    };
                    let summary = app.diff_summaries.get(&app.view.entries[entry_idx].id);
                    let show_source = show_source_flags.get(local_idx).copied().unwrap_or(true);

                    // A child row repeats the column layout of its group row,
                    // so the two stay aligned when the user hides a column.
                    let mut spans = Vec::with_capacity(10);
                    spans.push(Span::raw("    "));
                    if app.settings.show_time {
                        spans.push(Span::styled(
                            format!("{:<w$}", ts, w = ts_width),
                            Style::default().fg(time_color),
                        ));
                        spans.push(Span::raw(" "));
                    }
                    if app.settings.show_change {
                        spans.extend(summary_spans(summary));
                        spans.push(Span::raw(" "));
                    }
                    if app.settings.show_source {
                        spans.push(source_span_for(
                            entry,
                            show_source,
                            app.entry_run_state(entry_idx),
                        ));
                        spans.push(Span::raw(" "));
                    }
                    let title_width = title_col_width(inner_width, &app.settings);
                    if title_width > 0 {
                        spans.push(title_span(
                            session_title(entry),
                            entry.chain_id.as_deref(),
                            show_source,
                            title_width,
                        ));
                        spans.push(Span::raw(" "));
                    }
                    spans.push(session_bar_span(
                        entry.chain_id.as_deref(),
                        session_end_flags.get(local_idx).copied().unwrap_or(false),
                    ));
                    let fname_style = if match_indices.is_some() {
                        match_style
                    } else {
                        Style::default()
                            .fg(if is_selected { Color::White } else { Color::Gray })
                    };
                    spans.push(Span::styled(fname.to_string(), fname_style));
                    append_run_state(&mut spans, inner_width, app.entry_run_state(entry_idx));
                    let mut lines: Vec<Line<'static>> = Vec::with_capacity(2);
                    lines.extend(date_row);
                    lines.push(Line::from(spans));
                    lines
                }
            };

            // `row_bg` already folds the selection into the band.
            let mut style = Style::default().bg(row_bg);
            if row_dim {
                style = style.add_modifier(Modifier::DIM);
            }
            ListItem::new(lines).style(style)
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, inner);

    // Scrollbar
    if app.display_rows.len() > render_count {
        let mut sb =
            ScrollbarState::new(app.display_rows.len()).position(app.history_scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut sb,
        );
    }
}

fn draw_diff(f: &mut Frame, app: &App, area: Rect) {
    let active = app.active_panel == Panel::Diff;
    // Title shows the current file path
    let title = app
        .current_entry()
        .map(|e| format!(" {} ", e.display_path(&app.view.project_names)))
        .unwrap_or_else(|| " Diff ".to_string());

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(if active {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(border_style(active))
        .style(Style::default().bg(PANEL_BG));

    let inner = block.inner(area);
    f.render_widget(block, area);
    // Wipe leftover content from a previous diff. Paragraph only writes cells
    // covered by its lines, so when the new diff is shorter than the old one
    // the trailing rows would otherwise show stale text. Match the panel bg
    // so the cleared area doesn't flash a different color.
    f.render_widget(Block::default().style(Style::default().bg(PANEL_BG)), inner);

    let visible = inner.height as usize;
    let inner_width = inner.width as usize;

    let mut lines = diff_visible_lines(app, inner_width, visible);
    apply_diff_selection(app, &mut lines);
    f.render_widget(Paragraph::new(lines), inner);

    if app.diff_lines.len() > visible {
        let mut sb = ScrollbarState::new(app.diff_lines.len()).position(app.diff_scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut sb,
        );
    }
}

/// Background of a selected character in the Diff panel.
const SELECTION_BG: Color = Color::Rgb(60, 74, 96);

/// Paint the selection background over the selected characters of `lines`.
/// The selected run keeps its own foreground color, so the diff stays
/// readable. No-op without a live selection.
fn apply_diff_selection(app: &App, lines: &mut [Line<'static>]) {
    let Some((start, end)) = app.diff_selection_range() else {
        return;
    };
    for (row, line) in lines.iter_mut().enumerate() {
        if row < start.0 || row > end.0 {
            continue;
        }
        let from = if row == start.0 { start.1 } else { 0 };
        let to = if row == end.0 { end.1 } else { usize::MAX };
        if from >= to {
            continue;
        }
        *line = highlight_columns(line, from, to, Style::default().bg(SELECTION_BG));
    }
}

/// Rebuild `line` with `style` merged into the characters in the column range
/// `from..to`. Columns count characters, the same way [`spans_width`] does.
fn highlight_columns(line: &Line<'static>, from: usize, to: usize, style: Style) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    let mut col = 0usize;
    for span in &line.spans {
        // Split the span into the run before the range, the run inside it,
        // and the run after it. An empty run adds no span.
        let mut before = String::new();
        let mut inside = String::new();
        let mut after = String::new();
        for ch in span.content.chars() {
            if col < from {
                before.push(ch);
            } else if col < to {
                inside.push(ch);
            } else {
                after.push(ch);
            }
            col += 1;
        }
        if !before.is_empty() {
            out.push(Span::styled(before, span.style));
        }
        if !inside.is_empty() {
            out.push(Span::styled(inside, span.style.patch(style)));
        }
        if !after.is_empty() {
            out.push(Span::styled(after, span.style));
        }
    }
    Line::from(out)
}

/// The text the Diff panel shows inside the selection, ready for the
/// clipboard. Each row keeps only its selected columns, and the rows join
/// with a newline. Returns None without a live selection, or when the
/// selection covers no character.
pub fn diff_selection_text(app: &App, inner_width: usize, visible: usize) -> Option<String> {
    let (start, end) = app.diff_selection_range()?;
    let lines = diff_visible_lines(app, inner_width, visible);
    let mut rows: Vec<String> = Vec::new();
    for (row, line) in lines.iter().enumerate() {
        if row < start.0 || row > end.0 {
            continue;
        }
        let from = if row == start.0 { start.1 } else { 0 };
        let to = if row == end.0 { end.1 } else { usize::MAX };
        let text: String = line
            .spans
            .iter()
            .flat_map(|s| s.content.chars())
            .skip(from)
            .take(to.saturating_sub(from))
            .collect();
        rows.push(text.trim_end().to_string());
    }
    let out = rows.join("\n");
    (!out.trim().is_empty()).then_some(out)
}

/// The lines the Diff panel draws for the current scroll position: one entry
/// per screen line, at most `visible` of them. `draw_diff` renders these, and
/// [`diff_selection_text`] reads the same lines back, so the text the user
/// copies is the text the panel shows.
fn diff_visible_lines(app: &App, inner_width: usize, visible: usize) -> Vec<Line<'static>> {
    // A compaction shows its summary as a response, so it takes the prompt
    // path too.
    let is_prompt = app
        .current_entry()
        .map(|e| !e.is_file_change())
        .unwrap_or(false);

    // Active regex for content search — used to highlight in-diff hits on
    // Added/Removed/Prompt lines. None for path search, an inactive search,
    // or while the user has toggled highlights off (`:noh`-style). A Diff-
    // panel `/` search takes priority over History's `f` so the same render
    // path handles either (the two queries are independent — whichever the
    // user is actively driving wins).
    let content_regex: Option<&Regex> = if app.diff_search.phase != SearchPhase::Inactive
        && app.diff_search.highlights
        && app.diff_search.regex.is_some()
    {
        app.diff_search.regex.as_ref()
    } else if app.search.phase != SearchPhase::Inactive
        && app.search.highlights
        && app.search.kind == SearchKind::Content
    {
        app.search.content_regex.as_ref()
    } else {
        None
    };
    let match_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let render_dl = |dl: &DiffLine| -> Line<'static> {
        let text_spans = |text: &str, base: Style| -> Vec<Span<'static>> {
            match content_regex {
                Some(re) => regex_highlight_spans(text, base, match_style, re),
                None => vec![Span::styled(text.to_string(), base)],
            }
        };
        match dl {
            DiffLine::Header(s) => Line::from(Span::styled(
                s.clone(),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )),
            DiffLine::Added(s) => {
                let base = Style::default().fg(Color::Green);
                let mut spans = vec![Span::styled("+ ", base)];
                spans.extend(text_spans(s, base));
                Line::from(spans)
            }
            DiffLine::Removed(s) => {
                let base = Style::default().fg(Color::Red);
                let mut spans = vec![Span::styled("- ", base)];
                spans.extend(text_spans(s, base));
                Line::from(spans)
            }
            DiffLine::Context(s) => Line::from(vec![
                Span::raw("  "),
                Span::styled(s.clone(), Style::default().fg(Color::DarkGray)),
            ]),
            DiffLine::Prompt(s) => {
                // The prompt path below formats these through
                // `prompt_line_spans`; this arm only catches a stray line.
                Line::from(text_spans(s, Style::default().fg(PROMPT_COLOR)))
            }
            DiffLine::Response(s) => {
                Line::from(text_spans(s, Style::default().fg(RESPONSE_COLOR)))
            }
            DiffLine::ChildSeparator(id) => {
                // The server sent the diff for the entries of the selected
                // group, so the view holds each of them.
                let Some(entry) = app.view.get(id) else {
                    return Line::from(Span::styled(format!("── {id}"), Style::default().fg(Color::DarkGray)));
                };
                let ts_vec = format_panel_timestamps(&[entry.timestamp], app.absolute_time);
                let ts = ts_vec.into_iter().next().unwrap_or_default();
                let summary = app.diff_summaries.get(id);
                let session_name = session_title(entry);
                separator_line(
                    entry,
                    &app.view.project_names,
                    &ts,
                    summary,
                    session_name,
                    inner_width,
                )
            }
        }
    };

    if is_prompt {
        // Render the prompt with the same wrapping, indent rules and Markdown
        // highlighting the History panel uses, instead of Paragraph's own
        // wrap — that one knows nothing about the hang indent or the
        // soft-wrap marker. `diff_lines` holds one line per source line, so
        // `diff_scroll` steps by source line and the scroll bounds in
        // `App::diff_down` and friends stay correct.
        // `diff_lines` holds the prompt lines, then a blank line and the
        // response lines when the turn has a response. The highlighter runs
        // once per text; the walk below maps each `diff_lines` index onto
        // its line in the matching highlight.
        let entry = app.current_entry();
        let text = entry.and_then(|e| e.prompt_text()).unwrap_or("");
        let response = entry
            .and_then(|e| e.response_text().or(e.compaction_summary()))
            .unwrap_or("");
        let hl_prompt = prompt::highlight(text, PROMPT_COLOR);
        let hl_response = prompt::highlight(response, RESPONSE_COLOR);
        let empty: Vec<prompt::Piece> = Vec::new();
        let overlay = content_regex.map(|re| (re, match_style));
        let mut lines: Vec<Line> = Vec::with_capacity(visible);
        let mut prompt_i = 0usize;
        let mut response_i = 0usize;
        for (i, dl) in app.diff_lines.iter().enumerate() {
            let (src, hang) = match dl {
                DiffLine::Prompt(_) => {
                    prompt_i += 1;
                    (hl_prompt.get(prompt_i - 1), prompt_i > 1)
                }
                DiffLine::Response(_) => {
                    response_i += 1;
                    (hl_response.get(response_i - 1), response_i > 1)
                }
                _ => (None, false),
            };
            if i < app.diff_scroll {
                continue;
            }
            if lines.len() >= visible {
                break;
            }
            let src = src.unwrap_or(&empty);
            for seg in prompt::wrap_line(src, inner_width, hang, visible.max(1)) {
                lines.push(Line::from(prompt_line_spans(&seg, overlay)));
            }
        }
        lines.truncate(visible);
        lines
    } else {
        app.diff_lines
            .iter()
            .skip(app.diff_scroll)
            .take(visible)
            .map(render_dl)
            .collect()
    }
}

/// Render a generic typing prompt (Projects/Files panels). The History panel
/// has its own renderer because it carries the path-vs-regex kind tag.
/// A dim `↑↓ history` hint for a search prompt whose mode remembers a query.
/// It shows only on an empty prompt, where the whole list is within reach.
fn history_hint(app: &App, mode: SearchMode, query: &str) -> Span<'static> {
    let remembers = app
        .search_history
        .get(mode)
        .map(|h| !h.entries().is_empty())
        .unwrap_or(false);
    if query.is_empty() && remembers {
        Span::styled("  ↑↓ history", Style::default().fg(Color::DarkGray))
    } else {
        Span::raw("")
    }
}

fn panel_typing_prompt(
    prefix: &str,
    query: &str,
    total: usize,
    hint: Span<'static>,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            prefix.to_string(),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(query.to_string(), Style::default().fg(Color::White)),
        Span::styled(
            "█",
            Style::default().fg(Color::White).add_modifier(Modifier::DIM),
        ),
        Span::raw("  "),
        Span::styled(
            if query.is_empty() {
                String::new()
            } else {
                format!("({} match{})", total, if total == 1 { "" } else { "es" })
            },
            Style::default().fg(Color::DarkGray),
        ),
        hint,
    ])
}

/// Render the `[N/total]  n:next  N:prev  Esc:clear  /:new search` bar shown
/// for a committed side-panel search.
fn panel_active_nav(total: usize, current: usize) -> Line<'static> {
    let curr = if total == 0 { 0 } else { current + 1 };
    Line::from(vec![
        Span::styled(
            format!(" [{}/{}]  ", curr, total),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("n", Style::default().fg(Color::White)),
        Span::styled(":next  ", Style::default().fg(Color::DarkGray)),
        Span::styled("N", Style::default().fg(Color::White)),
        Span::styled(":prev  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::White)),
        Span::styled(":clear  ", Style::default().fg(Color::DarkGray)),
        Span::styled("/", Style::default().fg(Color::White)),
        Span::styled(":new search  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::White)),
        Span::styled(":quit", Style::default().fg(Color::DarkGray)),
    ])
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    // Only one search prompt can be in Typing phase at a time; that prompt
    // wins over the default help regardless of which panel is focused. Once
    // committed (Active), the focused panel's search drives the bar.
    // Vim's `:noh` (`highlights == false`) makes Active read as Inactive so
    // the default help comes back without losing the matches behind it.
    enum HelpMode {
        HistoryTyping,
        ProjectTyping,
        FileTyping,
        DiffTyping,
        HistoryActive,
        ProjectActive,
        FileActive,
        DiffActive,
        Default,
    }
    let mode = if app.search.phase == SearchPhase::Typing {
        HelpMode::HistoryTyping
    } else if app.project_search.phase == SearchPhase::Typing {
        HelpMode::ProjectTyping
    } else if app.file_search.phase == SearchPhase::Typing {
        HelpMode::FileTyping
    } else if app.diff_search.phase == SearchPhase::Typing {
        HelpMode::DiffTyping
    } else if app.active_panel == Panel::History
        && app.search.phase == SearchPhase::Active
        && app.search.highlights
    {
        HelpMode::HistoryActive
    } else if app.active_panel == Panel::Projects
        && app.project_search.phase == SearchPhase::Active
        && app.project_search.highlights
    {
        HelpMode::ProjectActive
    } else if app.active_panel == Panel::Files
        && app.file_search.phase == SearchPhase::Active
        && app.file_search.highlights
    {
        HelpMode::FileActive
    } else if app.active_panel == Panel::Diff
        && app.diff_search.phase == SearchPhase::Active
        && app.diff_search.highlights
    {
        HelpMode::DiffActive
    } else {
        HelpMode::Default
    };

    let line = match mode {
        HelpMode::HistoryTyping => {
            let total = app.search.matches.len();
            // Prefix mirrors which key opened the prompt (/ for fuzzy path,
            // f for regex content). Suffix tag reinforces which corpus is
            // being searched so the count below makes sense.
            let (prefix, tag) = match app.search.kind {
                SearchKind::Path => ("/", ""),
                SearchKind::Content => ("f ", " [regex]"),
                SearchKind::Semantic => ("F ", " [semantic]"),
            };
            // For an empty regex query the matcher returns no results; show
            // a hint instead of a misleading "(0 matches)".
            let regex_invalid = app.search.kind == SearchKind::Content
                && !app.search.query.is_empty()
                && app.search.content_regex.is_none();
            let count_str = if app.search.kind == SearchKind::Semantic {
                // The search runs on Enter, so a count while typing would
                // describe nothing. Say what the key does instead.
                "(press Enter)".to_string()
            } else if app.search.query.is_empty() {
                String::new()
            } else if regex_invalid {
                "(invalid regex)".to_string()
            } else if app.search.needs_more_chars() {
                "(keep typing…)".to_string()
            } else if app.search.scan_pending() {
                // A content scan is debounced; `total` still describes the
                // previous query, so don't present it as this query's count.
                "(searching…)".to_string()
            } else {
                format!("({} match{})", total, if total == 1 { "" } else { "es" })
            };
            let count_style = if regex_invalid {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(vec![
                Span::styled(
                    prefix,
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled(app.search.query.clone(), Style::default().fg(Color::White)),
                Span::styled(
                    "█",
                    Style::default().fg(Color::White).add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    tag.to_string(),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::DIM),
                ),
                Span::raw("  "),
                Span::styled(count_str, count_style),
                history_hint(app, app.history_search_mode(), &app.search.query),
            ])
        }
        HelpMode::ProjectTyping => panel_typing_prompt(
            "/",
            &app.project_search.query,
            app.project_search.matches.len(),
            history_hint(app, SearchMode::Project, &app.project_search.query),
        ),
        HelpMode::FileTyping => panel_typing_prompt(
            "/",
            &app.file_search.query,
            app.file_search.matches.len(),
            history_hint(app, SearchMode::File, &app.file_search.query),
        ),
        HelpMode::DiffTyping => {
            // Regex prompt for `/` in the Diff panel. Mirrors History's `f`
            // typing prompt — including the `[regex]` tag and the invalid-
            // regex error styling — so the two surfaces feel like one.
            let total = app.diff_search.matches.len();
            let regex_invalid = !app.diff_search.query.is_empty()
                && app.diff_search.regex.is_none();
            let count_str = if app.diff_search.query.is_empty() {
                String::new()
            } else if regex_invalid {
                "(invalid regex)".to_string()
            } else {
                format!("({} match{})", total, if total == 1 { "" } else { "es" })
            };
            let count_style = if regex_invalid {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(vec![
                Span::styled(
                    "/",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    app.diff_search.query.clone(),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    "█",
                    Style::default().fg(Color::White).add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    " [regex]".to_string(),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::DIM),
                ),
                Span::raw("  "),
                Span::styled(count_str, count_style),
                history_hint(app, SearchMode::Diff, &app.diff_search.query),
            ])
        }
        HelpMode::HistoryActive => {
            let total = app.search.matches.len();
            let curr = if total == 0 { 0 } else { app.search.current + 1 };
            let mode_tag = match app.search.kind {
                SearchKind::Path => "",
                SearchKind::Content => " [regex]",
                SearchKind::Semantic => " [semantic]",
            };
            // A semantic result list is ranked, not in time order, so the
            // score of the current row tells the user how far down they are.
            let score_tag = match app.semantic_current_score() {
                Some(score) => {
                    let (prompts, edits) = app.semantic_split();
                    format!(" {score:.2}  {prompts} prompts, {edits} edits")
                }
                None => String::new(),
            };
            Line::from(vec![
                Span::styled(
                    format!(" [{}/{}]", curr, total),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{mode_tag}{score_tag}"),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::DIM),
                ),
                Span::raw("  "),
                Span::styled("n", Style::default().fg(Color::White)),
                Span::styled(":next  ", Style::default().fg(Color::DarkGray)),
                Span::styled("N", Style::default().fg(Color::White)),
                Span::styled(":prev  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Esc", Style::default().fg(Color::White)),
                Span::styled(":clear  ", Style::default().fg(Color::DarkGray)),
                Span::styled("/", Style::default().fg(Color::White)),
                Span::styled(":path  ", Style::default().fg(Color::DarkGray)),
                Span::styled("f", Style::default().fg(Color::White)),
                Span::styled(":content  ", Style::default().fg(Color::DarkGray)),
                Span::styled("p", Style::default().fg(Color::White)),
                Span::styled(
                    format!(":{}  ", app.kind_filter.label()),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled("q", Style::default().fg(Color::White)),
                Span::styled(":quit", Style::default().fg(Color::DarkGray)),
            ])
        }
        HelpMode::ProjectActive => panel_active_nav(
            app.project_search.matches.len(),
            app.project_search.current,
        ),
        HelpMode::FileActive => panel_active_nav(
            app.file_search.matches.len(),
            app.file_search.current,
        ),
        HelpMode::DiffActive => panel_active_nav(
            app.diff_search.matches.len(),
            app.diff_search.current,
        ),
        HelpMode::Default => {
            let mut spans = vec![
                Span::styled(" h/l", Style::default().fg(Color::White)),
                Span::styled(":panel  ", Style::default().fg(Color::DarkGray)),
                Span::styled("j/k", Style::default().fg(Color::White)),
                Span::styled(":nav  ", Style::default().fg(Color::DarkGray)),
                Span::styled("ctrl+d/ctrl+u", Style::default().fg(Color::White)),
                Span::styled(":page  ", Style::default().fg(Color::DarkGray)),
                Span::styled("g/G", Style::default().fg(Color::White)),
                Span::styled(":top/bottom  ", Style::default().fg(Color::DarkGray)),
            ];
            match app.active_panel {
                Panel::History => {
                    spans.push(Span::styled("/", Style::default().fg(Color::White)));
                    spans.push(Span::styled(":path  ", Style::default().fg(Color::DarkGray)));
                    spans.push(Span::styled("f", Style::default().fg(Color::White)));
                    spans.push(Span::styled(":find  ", Style::default().fg(Color::DarkGray)));
                    spans.push(Span::styled("F", Style::default().fg(Color::White)));
                    spans.push(Span::styled(":semantic  ", Style::default().fg(Color::DarkGray)));
                    // Hide the `x` hint while auto-expand is force-on (file
                    // filter active) — the toggle is a no-op in that mode.
                    if app.can_toggle_auto_expand() {
                        spans.push(Span::styled("x", Style::default().fg(Color::White)));
                        spans.push(Span::styled(
                            if app.auto_expand {
                                ":group  "
                            } else {
                                ":expand  "
                            },
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    spans.push(Span::styled("p", Style::default().fg(Color::White)));
                    spans.push(Span::styled(
                        format!(":{}  ", app.kind_filter.label()),
                        Style::default().fg(Color::DarkGray),
                    ));
                    spans.push(Span::styled("<>", Style::default().fg(Color::White)));
                    spans.push(Span::styled(
                        format!(":prompt lines ({})  ", app.prompt_lines),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                Panel::Projects | Panel::Files => {
                    spans.push(Span::styled("/", Style::default().fg(Color::White)));
                    spans.push(Span::styled(":search  ", Style::default().fg(Color::DarkGray)));
                }
                Panel::Agents => {}
                Panel::Diff => {
                    spans.push(Span::styled("/", Style::default().fg(Color::White)));
                    spans.push(Span::styled(":find  ", Style::default().fg(Color::DarkGray)));
                }
            }
            spans.extend([
                Span::styled("m", Style::default().fg(Color::White)),
                Span::styled(
                    format!(":{} mode  ", app.mode.other().label()),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled("o", Style::default().fg(Color::White)),
                Span::styled(":go to session  ", Style::default().fg(Color::DarkGray)),
            ]);
            // A prompt row has no file, so `O` does nothing on it.
            if app.can_open_file() {
                spans.push(Span::styled("O", Style::default().fg(Color::White)));
                spans.push(Span::styled(
                    ":open file  ",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            // A mouse selection in the Diff panel is what `y` copies, so the
            // hint must name it instead of the whole entry.
            let copy_noun = if app.diff_selection_range().is_some() {
                "selection"
            } else {
                app.copy_target().map(|(_, noun)| noun).unwrap_or("path")
            };
            spans.extend([
                Span::styled("y", Style::default().fg(Color::White)),
                Span::styled(
                    format!(":copy {copy_noun}  "),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            // Only the Diff panel shows a response, so only it offers `Y`.
            if app.copy_response_target().is_some() {
                spans.push(Span::styled("Y", Style::default().fg(Color::White)));
                spans.push(Span::styled(
                    ":copy response  ",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            spans.extend([
                Span::styled("t", Style::default().fg(Color::White)),
                Span::styled(":time format  ", Style::default().fg(Color::DarkGray)),
                Span::styled("S", Style::default().fg(Color::White)),
                Span::styled(":settings  ", Style::default().fg(Color::DarkGray)),
                Span::styled("q", Style::default().fg(Color::White)),
                Span::styled(":quit", Style::default().fg(Color::DarkGray)),
            ]);
            if app.loading {
                spans.push(Span::styled(
                    "  [loading…]",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::DIM),
                ));
            }
            if let Some(error) = &app.error {
                spans.push(Span::styled(
                    format!("  [server: {error}]"),
                    Style::default().fg(Color::Red),
                ));
            }
            if let Some(label) = embed_label(&app.embed) {
                spans.push(Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::DIM),
                ));
            }
            Line::from(spans)
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::app::App;
    use recap_api::EntryKind;
    use recap_server::history::HistoryEntry;
    use recap_server::store::Store;
    use std::sync::Arc;

    /// An App whose server holds `entries` and runs no loader.
    fn test_app(entries: Vec<HistoryEntry>) -> App {
        App::new(Arc::new(recap_server::Server::from_entries(entries)))
    }
    use ratatui::{backend::TestBackend, Terminal};

    fn prompt(text: &str, session: &str) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local::now() - chrono::Duration::minutes(7),
            file_path: PathBuf::from(format!("claude:{session}:{text}")),
            snapshot_path: PathBuf::new(),
            base_path: None,
            project_roots: vec![PathBuf::from("/dev/proj")],
            source: EditSource::ClaudeCode,
            kind: EntryKind::Prompt { text: text.to_string(), response: None },
            session_id: Some(session.to_string()),
            session_cwd: Some(PathBuf::from("/dev/proj")),
        }
    }

    fn file_edit(path: &str, session: Option<&str>, source: EditSource) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local::now() - chrono::Duration::minutes(3),
            file_path: PathBuf::from(path),
            snapshot_path: PathBuf::from(format!("/snapshots{path}/{source:?}/{}", session.unwrap_or(""))),
            base_path: None,
            project_roots: vec![PathBuf::from("/dev/proj")],
            source,
            kind: EntryKind::FileChange,
            session_id: session.map(str::to_string),
            session_cwd: Some(PathBuf::from("/dev/proj")),
        }
    }

    /// Foreground color of the cell at (x, y).
    fn fg_at(app: &App, w: u16, h: u16, x: u16, y: u16) -> Color {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        term.backend().buffer().cell((x, y)).unwrap().fg
    }

    fn bg_at(app: &App, w: u16, h: u16, x: u16, y: u16) -> Color {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        term.backend().buffer().cell((x, y)).unwrap().bg
    }

    /// Crop each row to the left half of the frame, which holds the History
    /// panel. Column and row numbers stay absolute, so [`fg_at`] can use them.
    fn history_half(out: &str, w: u16) -> String {
        out.lines()
            .map(|l| l.chars().take(w as usize / 2).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Cell column and row of the first cell of `needle`. Rows hold box
    /// characters, so the column must count characters, not bytes.
    fn find(out: &str, needle: &str) -> (usize, usize) {
        for (y, line) in out.lines().enumerate() {
            if let Some(byte) = line.find(needle) {
                return (line[..byte].chars().count(), y);
            }
        }
        panic!("{needle:?} not found in\n{out}");
    }


    /// Render the whole frame to a plain-text grid for assertions.
    fn render(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A compaction row shows the label in the compaction color. The rows of
    /// two sessions of one session chain have the bar color of the chain.
    #[test]
    fn a_compaction_row_shows_the_label_and_the_chain_color() {
        let mut compaction = prompt("summary", "child");
        compaction.file_path = PathBuf::from("claude:child:c1");
        compaction.kind = EntryKind::Compaction { summary: "The summary.".to_string() };
        compaction.timestamp = Local::now() - chrono::Duration::minutes(5);
        let mut store = Store::from_entries(vec![prompt("start", "root"), compaction, file_edit("/dev/proj/a.rs", Some("child"), EditSource::ClaudeCode)]);
        store.extend_parents([("child".to_string(), "root".to_string())]);
        store.commit();
        let mut app = App::new(Arc::new(recap_server::Server::from_store(store)));
        app.recompute_row_heights(90, 18);
        let out = history_half(&render(&app, 190, 20), 190);

        let (label_x, label_y) = find(&out, recap_api::COMPACTION_LABEL);
        assert_eq!(fg_at(&app, 190, 20, (label_x + 3) as u16, label_y as u16), COMPACTION_COLOR);
        let (_, start_y) = find(&out, "start");
        // The oldest row of the chain ends the band with `╹`.
        let (bar_x, _) = find(&out, "╹");
        assert_eq!(
            fg_at(&app, 190, 20, bar_x as u16, label_y as u16),
            fg_at(&app, 190, 20, bar_x as u16, start_y as u16),
            "the two sessions of the chain have one bar color\n{out}"
        );
        assert_eq!(fg_at(&app, 190, 20, bar_x as u16, label_y as u16), session_color("root"), "{out}");
    }

    /// The title column prints the session title of the entry. The map holds
    /// the Cursor composer names and the Claude Code `aiTitle` values alike,
    /// so one test covers both sources.
    #[test]
    fn the_title_column_shows_the_session_title() {
        let mut store = Store::from_entries(vec![prompt("fix it", "sessionAAAA-1111")]);
        store.extend_titles([("sessionAAAA-1111".to_string(), "Page embed".to_string())]);
        store.commit();
        let mut app = App::new(Arc::new(recap_server::Server::from_store(store)));
        app.recompute_row_heights(90, 18);
        let out = history_half(&render(&app, 190, 20), 190);

        let (title_x, _) = find(&out, "Page embed");
        let (source_x, _) = find(&out, "claude");
        assert!(
            title_x > source_x,
            "the title column follows the source column\n{out}"
        );
        // The prompt text still follows both columns.
        let (text_x, _) = find(&out, "fix it");
        assert!(text_x > title_x, "the prompt text follows the title\n{out}");
    }

    /// The user can hide the title column, and then the row drops it.
    #[test]
    fn hiding_the_title_column_removes_it() {
        let mut store = Store::from_entries(vec![prompt("fix it", "sessionAAAA-1111")]);
        store.extend_titles([("sessionAAAA-1111".to_string(), "Page embed".to_string())]);
        store.commit();
        let mut app = App::new(Arc::new(recap_server::Server::from_store(store)));
        app.settings.show_title = false;
        app.recompute_row_heights(90, 18);
        let out = history_half(&render(&app, 190, 20), 190);
        assert!(!out.contains("Page embed"), "the column is hidden\n{out}");
    }

    /// With the date row on, the panel prints one row per day, and the row
    /// names the day both ways.
    #[test]
    fn the_date_row_names_the_day_twice() {
        let mut old = prompt("older ask", "sessionAAAA-1111");
        old.timestamp = Local::now() - chrono::Duration::days(3);
        let store = (vec![prompt("new ask", "sessionAAAA-1111"), old]);
        let mut app = test_app(store);
        app.settings.date_separator = true;
        app.recompute_row_heights(90, 18);
        let out = history_half(&render(&app, 190, 20), 190);

        let today = Local::now().date_naive();
        assert!(out.contains("today - "), "the first day is today\n{out}");
        let older = (Local::now() - chrono::Duration::days(3)).date_naive();
        assert!(
            out.contains(&format!("3 days ago - {}", older.format("%Y-%m-%d"))),
            "the older day names both forms\n{out}"
        );
        assert!(
            out.contains(&today.format("%Y-%m-%d").to_string()),
            "the date row prints the date\n{out}"
        );
        // The date row costs the first row of each day one extra line.
        assert_eq!(app.row_height(0), 2);
    }

    /// The aligned project column starts the text of every row at the same
    /// column, however deep each row's own chain is.
    #[test]
    fn the_aligned_project_column_lines_up_every_row() {
        let mut deep = prompt("deep ask", "sessionAAAA-1111");
        deep.project_roots = vec![
            PathBuf::from("/dev/proj"),
            PathBuf::from("/dev/proj/inner"),
        ];
        let store = (vec![prompt("flat ask", "sessionBBBB-2222"), deep]);
        let mut app = test_app(store);
        app.settings.align_project = true;
        app.recompute_row_heights(90, 18);
        let out = history_half(&render(&app, 190, 20), 190);

        let (flat_x, _) = find(&out, "flat ask");
        let (deep_x, _) = find(&out, "deep ask");
        assert_eq!(flat_x, deep_x, "both rows start their text alike\n{out}");

        // Without the setting the shallower chain starts its text earlier.
        app.settings.align_project = false;
        app.recompute_row_heights(90, 18);
        let out = history_half(&render(&app, 190, 20), 190);
        let (flat_x, _) = find(&out, "flat ask");
        let (deep_x, _) = find(&out, "deep ask");
        assert!(flat_x < deep_x, "each row uses its own width\n{out}");
    }

    /// A chain that repeats the row above reads as a tree: a corner at the
    /// deepest repeated segment, and no project name twice. `└` marks the
    /// last row of the branch, `├` marks a branch that goes on.
    #[test]
    fn a_repeated_project_chain_reads_as_a_tree() {
        let deep = |file: &str| {
            let mut e = file_edit(file, Some("sessionAAAA-1111"), EditSource::Cursor);
            e.project_roots = vec![
                PathBuf::from("/dev/proj"),
                PathBuf::from("/dev/proj/inner"),
            ];
            e
        };
        let store = (vec![
            deep("/dev/proj/inner/a.rs"),
            deep("/dev/proj/inner/b.rs"),
            deep("/dev/proj/inner/c.rs"),
        ]);
        let mut app = test_app(store);
        app.recompute_row_heights(90, 18);
        let out = history_half(&render(&app, 190, 20), 190);

        // The newest row names the chain; the two below repeat it.
        let (_, top) = find(&out, "c.rs");
        let rows: Vec<&str> = out.lines().skip(top).take(3).collect();
        assert!(rows[0].contains("proj → inner"), "the first row names the chain\n{out}");
        assert!(!rows[1].contains("proj"), "the next row repeats no name\n{out}");
        assert!(rows[1].contains('├'), "a branch that goes on\n{out}");
        assert!(rows[2].contains('└'), "the last row of the branch\n{out}");
        assert!(!out.contains('↳'), "no trailing arrow is left\n{out}");

        // The rule is a setting.
        assert!(out.contains('┄'), "the pad draws the rule\n{out}");
        app.settings.project_fill = false;
        let out = history_half(&render(&app, 190, 20), 190);
        assert!(!out.contains('┄'), "the pad goes blank\n{out}");
    }

    /// The source column keeps the color of its source, not the hue of the
    /// session, so Cursor and Claude Code read apart at a glance.
    #[test]
    fn each_source_keeps_its_own_color() {
        let store = (vec![
            file_edit("/dev/proj/a.rs", Some("sessionAAAA-1111"), EditSource::Cursor),
            file_edit("/dev/proj/b.rs", Some("sessionBBBB-2222"), EditSource::ClaudeCode),
        ]);
        let mut app = test_app(store);
        app.recompute_row_heights(90, 18);
        let out = history_half(&render(&app, 190, 20), 190);

        let (cx, cy) = find(&out, "cursor");
        assert_eq!(fg_at(&app, 190, 20, cx as u16, cy as u16), CURSOR_COLOR);
        let (kx, ky) = find(&out, "claude");
        // Claude Code reports no live session here, so its label dims.
        assert_eq!(
            fg_at(&app, 190, 20, kx as u16, ky as u16),
            dim_color(CLAUDE_COLOR)
        );
    }

    /// The last row of a session draws the half-height end glyph, so the
    /// band does not join the band of the next session.
    #[test]
    fn the_last_row_of_a_session_breaks_its_bar() {
        let store = (vec![
            file_edit("/dev/proj/a.rs", Some("sessionAAAA-1111"), EditSource::Cursor),
            file_edit("/dev/proj/b.rs", Some("sessionBBBB-2222"), EditSource::Cursor),
        ]);
        let mut app = test_app(store);
        app.recompute_row_heights(90, 18);
        let out = history_half(&render(&app, 190, 20), 190);
        // Each row holds one entry of its own session, so both rows end
        // their band. Neither row draws the full-height `┃`.
        for name in ["a.rs", "b.rs"] {
            let (_, y) = find(&out, name);
            let line = out.lines().nth(y).unwrap();
            assert!(
                line.contains('╹'),
                "the {name} row ends its session bar\n{out}"
            );
            let inner: String = {
                let chars: Vec<char> = line.chars().collect();
                chars[1..chars.len() - 1].iter().collect()
            };
            assert!(
                !inner.contains('┃'),
                "the {name} row draws no full-height bar\n{out}"
            );
        }
    }

    /// The settings menu lists every setting with its check box.
    #[test]
    fn the_settings_menu_lists_the_columns() {
        let store = (vec![prompt("fix it", "sessionAAAA-1111")]);
        let mut app = test_app(store);
        app.settings_menu.open = true;
        app.settings.show_change = false;
        let out = render(&app, 190, 24);

        assert!(out.contains("Settings"), "the menu has a title\n{out}");
        for item in SETTINGS_ITEMS {
            assert!(out.contains(item.label()), "{} missing\n{out}", item.label());
        }
        // The first row is selected, and the row the user turned off is clear.
        let (_, time_y) = find(&out, "Time column");
        let (_, change_y) = find(&out, "Change column");
        let time_row = out.lines().nth(time_y).unwrap();
        let change_row = out.lines().nth(change_y).unwrap();
        assert!(time_row.contains("> [x]"), "time is on and selected\n{out}");
        assert!(change_row.contains("[ ]"), "change is off\n{out}");
    }

    /// The empty search prompt says that Up and Down reach the query list,
    /// but only once the mode remembers a query.
    #[test]
    fn the_search_prompt_offers_the_query_list_it_holds() {
        let store = (vec![prompt("fix it", "sessionAAAA-1111")]);
        let mut app = test_app(store);

        app.search_open();
        assert!(
            !render(&app, 100, 12).contains("↑↓ history"),
            "an empty query list offers nothing"
        );

        for c in "fix".chars() {
            app.search_push_char(c, 8);
        }
        assert!(
            !render(&app, 100, 12).contains("↑↓ history"),
            "the hint stays off a prompt that already holds text"
        );
        app.search_commit(8);

        app.search_open();
        assert!(render(&app, 100, 12).contains("↑↓ history"));
    }

    #[test]
    fn history_renders_bar_wrapping_and_footer() {
        let long = "this is a very long single line prompt that should wrap across \
                    several rows in the history list when the panel is narrow enough";
        let store = (vec![
            prompt(long, "sessionAAAA-1111"),
            prompt("short one", "sessionBBBB-2222"),
        ]);
        let mut app = test_app(store);
        // Mirror what the event loop does each frame.
        app.recompute_row_heights(40, 18);
        let out = render(&app, 84, 26);

        // Session color-bar column is drawn.
        assert!(out.contains('┃'), "expected session bar glyph\n{out}");
        // The long prompt wraps to more than one line (default N=5).
        let long_row_idx = app
            .display_rows
            .iter()
            .position(|r| matches!(r, DisplayRow::Group(gi) if {
                let e = &app.view.entries[app.groups[*gi].entry_indices[0]];
                e.prompt_text() == Some(long)
            }))
            .unwrap();
        assert!(app.row_height(long_row_idx) > 1, "long prompt should wrap");
        // Footer box + its fields render.
        assert!(out.contains("Selected"), "footer title missing\n{out}");
        assert!(out.contains("Session"), "footer session field missing\n{out}");
        assert!(
            out.contains("Not running") || out.contains("Still Running"),
            "footer running line missing\n{out}"
        );
    }
    #[test]
    fn an_archived_session_shows_its_glyph_and_the_restore_hint() {
        let mut app = test_app(vec![prompt("fix it", "sessionAAAA-1111")]);
        app.view.entries[0].session_archived = true;
        app.recompute_row_heights(40, 18);
        let out = render(&app, 100, 26);
        assert!(out.contains("◌ Archived"), "footer archived line missing\n{out}");
        assert!(out.contains("to restore and resume"), "footer restore hint missing\n{out}");
        assert!(out.contains(ARCHIVED_GLYPH), "row glyph missing\n{out}");
    }

    #[test]
    fn the_footer_shows_the_session_summary_under_the_session() {
        let mut app = test_app(vec![prompt("fix it", "sessionAAAA-1111")]);
        app.view.entries[0].session_summary = Some("I fixed the bug in the loader.".to_string());
        app.recompute_row_heights(40, 18);
        let out = render(&app, 100, 30);
        let rows: Vec<&str> = out.lines().collect();
        let session = rows.iter().position(|r| r.contains("Session claude")).expect("session line");
        assert!(
            rows[session + 1].contains("Summary I fixed the bug in the loader."),
            "the summary is not under the session line\n{out}"
        );
    }

    #[test]
    fn prompt_rows_carry_their_count_in_brackets() {
        // Two prompts in one session. `store.entries` sorts newest first, so
        // the older prompt is number 1 and the newer one is number 2.
        let mut older = prompt("first ask", "sessionAAAA-1111");
        older.timestamp = Local::now() - chrono::Duration::minutes(30);
        let store = (vec![prompt("second ask", "sessionAAAA-1111"), older]);
        let mut app = test_app(store);
        // Wide enough that the title column leaves the short prompts on one
        // line (see `title_col_width`).
        app.recompute_row_heights(90, 18);
        let out = render(&app, 190, 20);

        assert!(out.contains("[2] second ask"), "newest prompt is number 2\n{out}");
        assert!(out.contains("[1] first ask"), "oldest prompt is number 1\n{out}");
    }

    #[test]
    fn prompt_count_uses_the_timestamp_gray() {
        let store = (vec![prompt("only ask", "sessionAAAA-1111")]);
        let mut app = test_app(store);
        app.recompute_row_heights(60, 18);
        let out = history_half(&render(&app, 120, 20), 120);
        let (x, y) = find(&out, "[1]");
        let color = fg_at(&app, 120, 20, x as u16, y as u16);
        assert!(
            color == TIME_COLOR_RECENT || color == TIME_COLOR_OLD,
            "the count must use a timestamp gray, got {color:?}"
        );
    }

    #[test]
    fn stopped_claude_session_dims_its_source_label() {
        let store = (vec![prompt("an ask", "sessionAAAA-1111")]);
        let mut app = test_app(store);
        app.recompute_row_heights(60, 18);
        let out = history_half(&render(&app, 120, 20), 120);
        let (x, y) = find(&out, "claude");
        // No session registry runs in the test, so the session reads as
        // stopped and the label takes the darkened source color.
        let (_, bright) = source_label(EditSource::ClaudeCode);
        assert_eq!(
            fg_at(&app, 120, 20, x as u16, y as u16),
            dim_color(bright),
            "a stopped session must dim its source label\n{out}"
        );
    }

    #[test]
    fn neovim_keeps_a_bright_source_label() {
        // Neovim reports no run state, so the label must stay bright.
        let store = (vec![file_edit(
            "/dev/proj/src/main.rs",
            None,
            EditSource::Neovim,
        )]);
        let mut app = test_app(store);
        app.recompute_row_heights(60, 18);
        let out = history_half(&render(&app, 120, 20), 120);
        let (x, y) = find(&out, "nvim");
        assert_eq!(
            fg_at(&app, 120, 20, x as u16, y as u16),
            NEOVIM_COLOR,
            "an unknown run state must keep the bright label\n{out}"
        );
    }

    #[test]
    fn a_real_linebreak_indents_without_a_marker() {
        let store = (vec![prompt(
            "alpha bravo\nsecond paragraph",
            "sessionAAAA-1111",
        )]);
        let mut app = test_app(store);
        app.recompute_row_heights(100, 18);
        let out = history_half(&render(&app, 200, 20), 200);

        let (first_x, first_y) = find(&out, "alpha");
        let (second_x, second_y) = find(&out, "second paragraph");
        assert_eq!(second_y, first_y + 1);
        assert_eq!(
            second_x,
            first_x + HANG_INDENT,
            "a line after a real linebreak hangs by HANG_INDENT\n{out}"
        );
        // The hang columns stay blank — no marker on a hard break.
        let cells: String = out
            .lines()
            .nth(second_y)
            .unwrap()
            .chars()
            .skip(first_x)
            .take(HANG_INDENT)
            .collect();
        assert_eq!(
            cells,
            " ".repeat(HANG_INDENT),
            "a hard break carries no marker\n{out}"
        );
    }

    #[test]
    fn a_soft_wrap_marks_its_indent() {
        let long = "alpha bravo charlie delta echo foxtrot golf hotel india juliett kilo \
                    lima mike november oscar papa quebec romeo sierra tango uniform";
        let store = (vec![prompt(long, "sessionAAAA-1111")]);
        let mut app = test_app(store);
        app.recompute_row_heights(100, 18);
        let out = history_half(&render(&app, 200, 20), 200);

        let (first_x, first_y) = find(&out, "alpha");
        let wrapped = out.lines().nth(first_y + 1).unwrap();
        let indent: String = wrapped.chars().skip(first_x).take(WRAP_INDENT).collect();
        assert_eq!(
            indent,
            format!("{}{}", " ".repeat(WRAP_INDENT - 1), WRAP_MARKER),
            "a soft wrap puts the marker in its last indent column\n{out}"
        );
        // The marker is faint gray, not the prompt color.
        assert_eq!(
            fg_at(&app, 200, 20, (first_x + WRAP_INDENT - 1) as u16, (first_y + 1) as u16),
            WRAP_MARKER_COLOR
        );
    }

    #[test]
    fn a_soft_wrap_nests_under_its_own_source_line() {
        // The list item hangs by HANG_INDENT and carries two leading spaces of
        // its own, so its soft wrap sits WRAP_INDENT past that.
        let text = "do these:\n  - keep the wrapped words nested under the item text they belong to";
        let store = (vec![prompt(text, "sessionAAAA-1111")]);
        let mut app = test_app(store);
        app.prompt_lines = 6;
        app.recompute_row_heights(90, 18);
        let out = history_half(&render(&app, 190, 20), 190);

        let (first_x, _) = find(&out, "do these:");
        let (item_x, item_y) = find(&out, "- keep");
        assert_eq!(
            item_x,
            first_x + HANG_INDENT + 2,
            "the item keeps its own two leading spaces on top of the hang\n{out}"
        );
        let wrapped = out.lines().nth(item_y + 1).unwrap();
        let indent: String = wrapped
            .chars()
            .skip(item_x)
            .take(WRAP_INDENT)
            .collect();
        assert_eq!(
            indent,
            format!("{}{}", " ".repeat(WRAP_INDENT - 1), WRAP_MARKER),
            "the soft wrap nests under the item, not under the prompt\n{out}"
        );
    }

    /// Crop each row to the right half of the frame, which holds the Diff
    /// panel. Row 0 is blanked because a panel title repeats the prompt text.
    /// Row numbers stay absolute so [`fg_at`] can use them.
    fn diff_half(out: &str, w: u16) -> String {
        out.lines()
            .enumerate()
            .map(|(y, l)| {
                if y == 0 {
                    String::new()
                } else {
                    l.chars().skip(w as usize / 2).collect()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A prompt with a response. The synthetic key stays the prompt text.
    fn turn(text: &str, response: &str, session: &str) -> HistoryEntry {
        let mut e = prompt(text, session);
        e.kind = EntryKind::Prompt {
            text: text.to_string(),
            response: Some(response.to_string()),
        };
        e
    }

    #[test]
    fn the_response_hangs_under_its_prompt_in_the_response_color() {
        let store = (vec![turn(
            "fix the bug",
            "Fixed the null check.",
            "sessionAAAA-1111",
        )]);
        let mut app = test_app(store);
        app.recompute_row_heights(90, 18);
        let out = render(&app, 190, 20);

        // History (left half) and Diff (right half) both show the response
        // below the prompt: the prompt in white, the response in blue.
        let mut seen = 0;
        for (y, line) in out.lines().enumerate().skip(1) {
            if let Some(byte) = line.find("Fixed the null") {
                let x = line[..byte].chars().count();
                assert_eq!(fg_at(&app, 190, 20, x as u16, y as u16), RESPONSE_COLOR, "{out}");
                // The History row puts the response right under the prompt;
                // the Diff panel leaves one blank line between them.
                let above: Vec<&str> = out.lines().skip(y.saturating_sub(2)).take(2).collect();
                assert!(
                    above.iter().any(|l| l.contains("fix the bug")),
                    "the response follows its prompt\n{out}"
                );
                seen += 1;
            }
            if let Some(byte) = line.find("fix the bug") {
                let x = line[..byte].chars().count();
                assert_eq!(fg_at(&app, 190, 20, x as u16, y as u16), PROMPT_COLOR, "{out}");
            }
        }
        assert_eq!(seen, 2, "History and Diff each show the response once\n{out}");
        // The history row reserves the response lines.
        assert_eq!(app.row_height(0), 2);
    }

    /// Cell column and row of the first cell of `needle` inside `rect`.
    /// Numbers stay absolute, so [`bg_at`] can use them.
    fn find_in_rect(out: &str, rect: ratatui::layout::Rect, needle: &str) -> (usize, usize) {
        for (y, line) in out.lines().enumerate() {
            // Skip the border rows: the title can repeat the needle.
            if y <= rect.y as usize || y + 1 >= (rect.y + rect.height) as usize {
                continue;
            }
            let chars: Vec<char> = line.chars().collect();
            let from = rect.x as usize;
            let hay: String = chars.iter().skip(from).collect();
            if let Some(byte) = hay.find(needle) {
                return (from + hay[..byte].chars().count(), y);
            }
        }
        panic!("{needle:?} is missing from the panel\n{out}");
    }

    /// The Diff panel's rect, in the frame that `render` draws.
    fn diff_rect(app: &App, w: u16, h: u16) -> ratatui::layout::Rect {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut out = None;
        term.draw(|f| {
            let area = f.area();
            let content = ratatui::layout::Rect {
                height: area.height.saturating_sub(1),
                ..area
            };
            out = compute_layout(
                content,
                app.active_panel == Panel::Projects,
                app.active_panel == Panel::Files,
                app.mode == crate::app::Mode::Agent,
            )
            .diff;
            draw(f, app);
        })
        .unwrap();
        out.expect("the Diff panel has a rect")
    }

    #[test]
    fn a_mouse_drag_selects_and_copies_the_diff_panel_text() {
        let store = (vec![turn(
            "fix the bug",
            "Fixed the null check.",
            "sessionAAAA-1111",
        )]);
        let mut app = test_app(store);
        app.active_panel = Panel::Diff;
        app.recompute_row_heights(60, 18);

        let (w, h) = (110u16, 20u16);
        let rect = diff_rect(&app, w, h);
        let inner_width = (rect.width as usize).saturating_sub(2);
        let visible = (rect.height as usize).saturating_sub(2);

        // Find "the bug" on the prompt row of the Diff panel.
        let out = render(&app, w, h);
        let (col, row) = find_in_rect(&out, rect, "the bug");
        let inner_row = row - (rect.y as usize + 1);
        let inner_col = col - (rect.x as usize + 1);

        // A plain click marks nothing, so it must not replace the clipboard.
        app.diff_select_start(inner_row, inner_col);
        assert_eq!(app.diff_selection_range(), None, "a click selects no text");
        assert_eq!(diff_selection_text(&app, inner_width, visible), None);

        // Drag over the seven characters of "the bug".
        app.diff_select_to(inner_row, inner_col + 6);
        assert_eq!(
            diff_selection_text(&app, inner_width, visible).as_deref(),
            Some("the bug")
        );

        // The selected cells carry the selection background; the cell before
        // the selection keeps the panel background.
        assert_eq!(bg_at(&app, w, h, col as u16, row as u16), SELECTION_BG);
        assert_eq!(bg_at(&app, w, h, col as u16 + 6, row as u16), SELECTION_BG);
        assert_eq!(bg_at(&app, w, h, col as u16 - 1, row as u16), PANEL_BG);

        // A drag the other way selects the same text.
        app.diff_select_start(inner_row, inner_col + 6);
        app.diff_select_to(inner_row, inner_col);
        assert_eq!(
            diff_selection_text(&app, inner_width, visible).as_deref(),
            Some("the bug")
        );
    }

    #[test]
    fn a_diff_selection_spans_rows_and_drops_when_the_panel_scrolls() {
        let store = (vec![turn(
            "fix the bug",
            "Fixed the null check.",
            "sessionAAAA-1111",
        )]);
        let mut app = test_app(store);
        app.active_panel = Panel::Diff;
        app.recompute_row_heights(60, 18);

        let (w, h) = (110u16, 20u16);
        let rect = diff_rect(&app, w, h);
        let inner_width = (rect.width as usize).saturating_sub(2);
        let visible = (rect.height as usize).saturating_sub(2);

        let out = render(&app, w, h);
        let (pcol, prow) = find_in_rect(&out, rect, "fix the bug");
        let (rcol, rrow) = find_in_rect(&out, rect, "Fixed the null");
        let top = rect.y as usize + 1;
        let left = rect.x as usize + 1;

        // From the "the" of the prompt down to the "null" of the response.
        app.diff_select_start(prow - top, pcol - left + 4);
        app.diff_select_to(rrow - top, rcol - left + 13);
        let text = diff_selection_text(&app, inner_width, visible).expect("a selection");
        let rows: Vec<&str> = text.lines().collect();
        assert_eq!(rows.first().copied(), Some("the bug"));
        assert_eq!(rows.last().copied(), Some("Fixed the null"));

        // A scroll moves the text under the selection, so the app drops it.
        app.diff_scroll += 1;
        assert_eq!(app.diff_selection_range(), None, "a scroll drops the selection");

        // So does a new diff.
        app.diff_scroll -= 1;
        assert!(app.diff_selection_range().is_some());
        app.refresh_diff();
        assert_eq!(app.diff_selection_range(), None, "a new diff drops the selection");
    }

    #[test]
    fn the_diff_panel_drops_the_caret_and_keeps_the_shared_indent() {
        let text = "run the tests\n  - keep every wrapped word nested under the item it belongs to";
        let store = (vec![prompt(text, "sessionAAAA-1111")]);
        let mut app = test_app(store);
        app.recompute_row_heights(60, 18);
        // File mode: History on the left, Diff on the right.
        let out = diff_half(&render(&app, 110, 20), 110);

        // The old per-line "> " prefix is gone.
        assert!(
            !out.lines().any(|l| l.trim_start_matches('│').starts_with("> ")),
            "the Diff panel must not prefix prompt lines\n{out}"
        );
        let (first_x, _) = find(&out, "run the tests");
        let (item_x, item_y) = find(&out, "- keep");
        assert_eq!(
            item_x,
            first_x + HANG_INDENT + 2,
            "the Diff panel applies the same hang and source indent\n{out}"
        );
        // The wrapped remainder of the item carries the marker.
        let wrapped = out.lines().nth(item_y + 1).unwrap();
        let indent: String = wrapped.chars().skip(item_x).take(WRAP_INDENT).collect();
        assert_eq!(
            indent,
            format!("{}{}", " ".repeat(WRAP_INDENT - 1), WRAP_MARKER),
            "the Diff panel marks its soft wraps too\n{out}"
        );
    }

    #[test]
    fn every_panel_highlights_inline_code() {
        // The same prompt reaches History, Agents and Diff. Each must color
        // the backticked run apart from the prose.
        let store = (vec![prompt(
            "please run `cargo test` now",
            "sessionAAAA-1111",
        )]);
        let mut app = test_app(store);
        app.toggle_mode(18, 18);
        app.recompute_row_heights(50, 18);
        let out = render(&app, 200, 20);

        // Every row that shows the prompt must color `cargo` off-prose.
        // Row 0 holds the panel titles, one of which repeats the prompt.
        let mut checked = 0;
        for (y, line) in out.lines().enumerate().skip(1) {
            let mut from = 0usize;
            while let Some(byte) = line[from..].find("cargo") {
                let x = line[..from + byte].chars().count();
                assert_ne!(
                    fg_at(&app, 200, 20, x as u16, y as u16),
                    PROMPT_COLOR,
                    "inline code at ({x}, {y}) kept the prose color\n{out}"
                );
                checked += 1;
                from += byte + 5;
            }
        }
        // Agents, History and Diff each show the prompt once.
        assert_eq!(checked, 3, "expected the prompt in three panels\n{out}");
    }

    #[test]
    fn agent_mode_shows_the_agents_panel_and_filters_history() {
        let store = (vec![
            prompt("ask from A", "sessionAAAA-1111"),
            prompt("ask from B", "sessionBBBB-2222"),
        ]);
        let mut app = test_app(store);
        app.toggle_mode(18, 18);
        app.recompute_row_heights(50, 18);
        let out = render(&app, 160, 20);

        assert!(out.contains("Agents (2)"), "the Agents panel is missing\n{out}");
        // The switch started on the History panel, so History stays active.
        assert_eq!(app.active_panel, Panel::History);
        // History now holds only the selected agent's entries.
        assert_eq!(app.groups.len(), 1, "History must show one agent only");
        let entry_idx = app.groups[0].entry_indices[0];
        assert_eq!(
            app.view.entries[entry_idx].session_id.as_deref(),
            app.selected_agent.as_deref()
        );
    }

    #[test]
    fn agent_rows_count_the_prompts_of_one_session() {
        let mut older = prompt("first ask", "sessionAAAA-1111");
        older.timestamp = Local::now() - chrono::Duration::minutes(30);
        let store = (vec![
            prompt("newest ask", "sessionAAAA-1111"),
            older,
            prompt("other ask", "sessionBBBB-2222"),
        ]);
        let mut app = test_app(store);
        app.toggle_mode(18, 18);

        assert_eq!(app.agents.len(), 2);
        let a = app
            .agents
            .iter()
            .find(|a| a.session_id == "sessionAAAA-1111")
            .unwrap();
        assert_eq!(a.prompt_count, 2);
        assert_eq!(a.last_prompt, "newest ask");
    }

    #[test]
    fn agent_mode_hides_entries_without_a_session() {
        let store = (vec![
            file_edit("/dev/proj/src/main.rs", None, EditSource::Neovim),
            prompt("an ask", "sessionAAAA-1111"),
        ]);
        let mut app = test_app(store);
        app.toggle_mode(18, 18);
        assert_eq!(app.agents.len(), 1);
        assert_eq!(app.agents[0].session_id, "sessionAAAA-1111");
    }
}
