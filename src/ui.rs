use crate::app::{App, DisplayRow, Panel, SearchKind, SearchPhase};
use crate::diff::{DiffLine, DiffSummary};
use crate::history::{align_relative_times, EditSource, HistoryEntry};
use chrono::{DateTime, Local};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, List, ListItem, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
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

pub fn compute_layout(area: Rect, projects_active: bool, files_active: bool) -> LayoutRects {
    let v_outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let content_area = v_outer[0];

    let split_50_50 = |rect: Rect| {
        let s = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rect);
        (s[0], s[1])
    };

    // Side panels (Projects / Files) share the same width so swapping focus
    // between them doesn't shift History and Diff horizontally.
    let side_width: u16 = 26 * 4;

    if projects_active {
        let h_content = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(side_width), Constraint::Min(0)])
            .split(content_area);
        let (history_full, diff) = split_50_50(h_content[1]);
        let (history, history_footer) = split_history_footer(history_full);
        LayoutRects {
            projects: Some(h_content[0]),
            files: None,
            history,
            history_footer,
            diff: Some(diff),
        }
    } else if files_active {
        let h_content = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(side_width), Constraint::Min(0)])
            .split(content_area);
        let (history_full, diff) = split_50_50(h_content[1]);
        let (history, history_footer) = split_history_footer(history_full);
        LayoutRects {
            projects: None,
            files: Some(h_content[0]),
            history,
            history_footer,
            diff: Some(diff),
        }
    } else {
        // Both side panels hidden — wasted space when not focused. History
        // and Diff each get half the row.
        let (history_full, diff) = split_50_50(content_area);
        let (history, history_footer) = split_history_footer(history_full);
        LayoutRects {
            projects: None,
            files: None,
            history,
            history_footer,
            diff: Some(diff),
        }
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
const PROMPT_COLOR: Color = Color::Rgb(135, 206, 250);
const SOURCE_WIDTH: usize = 6;
/// Green used for the "session still running" play glyph.
const RUNNING_COLOR: Color = Color::Rgb(90, 220, 120);

/// Width (cells) of the per-session color-bar column: a box-drawing bar plus
/// a trailing space.
const BAR_COL_WIDTH: usize = 2;
/// Cells reserved at the right edge of a row for the trailing "running" glyph.
pub const RUNNING_RESERVE: usize = 2;
/// Height (rows) of the History footer box (2 border + 4 content lines).
pub const HISTORY_FOOTER_HEIGHT: u16 = 6;

/// Eight bright, well-separated hues rotated across agent sessions so
/// consecutive-but-distinct sessions read apart at a glance. A session's
/// color is derived from a stable hash of its id (see [`session_color`]).
const SESSION_COLORS: [Color; 8] = [
    Color::Rgb(235, 90, 90),   // red
    Color::Rgb(240, 150, 40),  // orange
    Color::Rgb(230, 210, 70),  // yellow
    Color::Rgb(90, 210, 110),  // green
    Color::Rgb(70, 200, 210),  // cyan
    Color::Rgb(110, 160, 255), // blue
    Color::Rgb(180, 130, 255), // purple
    Color::Rgb(240, 120, 200), // pink
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
fn session_bar_span(session_id: Option<&str>) -> Span<'static> {
    match session_id {
        Some(id) => Span::styled(
            "┃ ",
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
fn continuation_bar_span(session_id: Option<&str>) -> Span<'static> {
    match session_id {
        Some(id) => Span::styled("│ ", Style::default().fg(session_color(id))),
        None => Span::styled("┆ ", Style::default().fg(Color::Rgb(120, 120, 120))),
    }
}

/// Columns consumed by a History row's fixed prefix (before the project
/// chain / entry text): leading marker, timestamp, change column, source
/// label, and the session bar. Kept in one place so the prompt-wrapping
/// width used for row-height math ([`crate::app::App::recompute_row_heights`])
/// matches what the renderer lays out. Column order doesn't affect the total.
pub fn history_prefix_cols(ts_width: usize) -> usize {
    // leading(2) + ts + ' ' + change + ' ' + source + ' ' + bar
    2 + ts_width + 1 + CHANGE_COL_WIDTH + 1 + SOURCE_WIDTH + 1 + BAR_COL_WIDTH
}

/// Total char width of a run of spans (for right-aligning a trailing column).
fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Append the trailing "running" glyph flush against the right edge of a row
/// of the given inner width. No-op when the session isn't running.
fn append_running(spans: &mut Vec<Span<'static>>, inner_width: usize, running: bool) {
    if !running {
        return;
    }
    let used = spans_width(spans);
    // Park the glyph in the last column; keep at least one space of gap.
    let target = inner_width.saturating_sub(1);
    let pad = if target > used { target - used } else { 1 };
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(
        "▶",
        Style::default()
            .fg(RUNNING_COLOR)
            .add_modifier(Modifier::BOLD),
    ));
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

/// Down-and-right arrow used in place of the last masked separator " → ",
/// so a row whose project prefix matches its predecessor reads as a branch
/// continuation rather than a blank gap. Picked from the Arrows block (same
/// as `→`) so both glyphs share stroke weight; box-drawing alternatives
/// like `└` render noticeably bolder than `→` in most monospaced fonts.
const TREE_CORNER: char = '↳';

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

/// The source-label column ("claude" / "cursor" / "nvim"). Colored to match
/// the session bar so the label and its band read as one unit — session hue
/// when the entry has a session, else the source's own color (Cursor / Neovim
/// have no session bar to match). Blank when `show` is false, so a run of
/// consecutive rows sharing one session prints the label only on the first
/// row and the rest read as a continuation of the same block.
fn source_span_for(entry: &HistoryEntry, show: bool) -> Span<'static> {
    if !show {
        return Span::raw(" ".repeat(SOURCE_WIDTH));
    }
    let (label, own_color) = source_label(entry.source);
    let color = match entry.session_id.as_deref() {
        Some(id) => session_color(id),
        None => own_color,
    };
    Span::styled(
        format!("{:<width$}", label, width = SOURCE_WIDTH),
        Style::default().fg(color),
    )
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

    let rects = compute_layout(content_area, projects_active, files_active);

    if let Some(projects_area) = rects.projects {
        draw_projects(f, app, projects_area);
    }
    if let Some(files_area) = rects.files {
        draw_files(f, app, files_area);
    }
    draw_history(f, app, rects.history);
    draw_history_footer(f, app, rects.history_footer);
    if let Some(diff_area) = rects.diff {
        draw_diff(f, app, diff_area);
    }
    draw_help(f, app, help_area);
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
    let (src_label, src_color) = source_label(entry.source);

    let mut lines: Vec<Line> = Vec::with_capacity(4);

    // Session line: source + short session id (+ live status when running).
    let mut session_spans = vec![
        footer_label("Session"),
        Span::styled(format!("{} ", src_label), Style::default().fg(src_color)),
    ];
    match entry.session_id.as_deref() {
        Some(id) => {
            session_spans.push(session_bar_span(Some(id)));
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
        "prompt".to_string()
    } else if let Some(idx) = entry_idx {
        app.diff_summaries
            .get(&idx)
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
#[allow(clippy::too_many_arguments)]
/// Display name of the Cursor composer (agent session) an entry belongs to, if
/// any — the attributed composer for an edit, or the owning composer for a
/// Cursor prompt. `None` for unattributed edits and non-Cursor sources (a
/// Claude prompt's `session_id` isn't a composerId, so the lookup misses).
fn cursor_session_name<'a>(app: &'a App, entry: &HistoryEntry) -> Option<&'a str> {
    entry
        .session_id
        .as_deref()
        .and_then(|sid| app.store.cursor_session_names.get(sid))
        .map(|s| s.as_str())
}

fn group_row_line(
    entry: &HistoryEntry,
    project_names: &HashMap<PathBuf, String>,
    leading: Vec<Span<'static>>,
    ts: &str,
    ts_width: usize,
    time_color: Color,
    change_spans: Vec<Span<'static>>,
    match_indices: Option<&[u32]>,
    chain_mask: &[bool],
    show_source: bool,
    running: bool,
    inner_width: usize,
    reserved_lines: usize,
    prompt_max: usize,
    session_name: Option<&str>,
) -> Vec<Line<'static>> {
    // Fixed prefix: leading, timestamp, change column, source, session bar.
    // Source sits right before the bar so its session-colored label leads
    // directly into the matching band.
    let mut prefix: Vec<Span<'static>> = Vec::with_capacity(16);
    prefix.extend(leading);
    prefix.push(Span::styled(
        format!("{:<w$}", ts, w = ts_width),
        Style::default().fg(time_color),
    ));
    prefix.push(Span::raw(" "));
    prefix.extend(change_spans);
    prefix.push(Span::raw(" "));
    prefix.push(source_span_for(entry, show_source));
    prefix.push(Span::raw(" "));
    prefix.push(session_bar_span(entry.session_id.as_deref()));

    let (chain_names, rel) = entry.display_path_segments(project_names);
    let dim_base = Style::default().fg(Color::Gray);
    let file_base = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let match_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    // Build the chain string. Char count is preserved across masking so
    // the search match offsets (computed against the unmasked chain) keep
    // pointing at the right columns.
    let chain_str_opt: Option<String> = if chain_names.is_empty() {
        None
    } else {
        let last_masked = chain_mask.iter().rposition(|&visible| !visible);
        let mut s = String::new();
        for (i, name) in chain_names.iter().enumerate() {
            let visible = chain_mask.get(i).copied().unwrap_or(true);
            let name_chars = name.chars().count();
            if visible {
                s.push_str(name);
                s.push_str(" → ");
            } else if Some(i) == last_masked {
                for _ in 0..name_chars {
                    s.push(' ');
                }
                s.push(' ');
                s.push(TREE_CORNER);
                s.push(' ');
            } else {
                for _ in 0..(name_chars + 3) {
                    s.push(' ');
                }
            }
        }
        Some(s)
    };
    let chain_chars = chain_str_opt
        .as_ref()
        .map(|s| s.chars().count())
        .unwrap_or(0);

    // ── Prompt entries: wrap the (single-lined) text over up to N rows. ──
    if entry.is_prompt() {
        let prompt_style = Style::default().fg(PROMPT_COLOR);
        // Column at which the prompt text begins on the first line; also the
        // indent for continuation lines.
        let text_start = 2 + ts_width + 1 + SOURCE_WIDTH + 1 + CHANGE_COL_WIDTH + 1
            + BAR_COL_WIDTH
            + chain_chars;
        // Reserve room for the trailing ⟨composer⟩ label (first line only, but
        // reserved uniformly so the full-width first line can't overflow into
        // it): "  ⟨" + name + "⟩" = name chars + 4.
        let label_reserve = session_name.map(|n| n.chars().count() + 4).unwrap_or(0);
        let avail = inner_width
            .saturating_sub(text_start + RUNNING_RESERVE + label_reserve)
            .max(1);
        // Wrap the raw (multi-line) prompt text: real linebreaks start fresh
        // un-indented lines, soft-wrapped lines are flagged for indenting.
        let raw = entry.prompt_text().unwrap_or("");
        let mut wrapped = crate::history::wrap_text(raw, avail, prompt_max);
        // Force the rendered line count to match the reserved height so the
        // scroll model and the drawn rows stay in lock-step.
        wrapped.truncate(reserved_lines);
        while wrapped.len() < reserved_lines {
            wrapped.push(crate::history::WrappedLine {
                text: String::new(),
                continuation: false,
            });
        }

        // Column where the fixed session-bar sits (start of the bar column),
        // i.e. `text_start` minus the bar column and the chain that follow it.
        let bar_col = text_start - BAR_COL_WIDTH - chain_chars;

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(wrapped.len());
        for (i, seg) in wrapped.into_iter().enumerate() {
            if i == 0 {
                let mut spans = prefix.clone();
                if let Some(chain_str) = chain_str_opt.clone() {
                    spans.push(Span::styled(chain_str, dim_base));
                }
                spans.push(Span::styled(seg.text, prompt_style));
                // A Cursor prompt names its composer (agent session) in that
                // session's color, matching the label on its attributed edits.
                if let Some(name) = session_name {
                    let color =
                        entry.session_id.as_deref().map(session_color).unwrap_or(Color::Gray);
                    spans.push(Span::styled(
                        format!("  ⟨{}⟩", name),
                        Style::default().fg(color),
                    ));
                }
                append_running(&mut spans, inner_width, running);
                lines.push(Line::from(spans));
            } else {
                // Continuation line: redraw the grouping bar (thin) in the
                // session column so the band is unbroken, then re-indent past
                // the (blank) chain to line up under the first line's text.
                // Soft-wrapped lines get an extra indent so they read as
                // distinct from lines produced by a real linebreak.
                let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
                if bar_col > 0 {
                    spans.push(Span::raw(" ".repeat(bar_col)));
                }
                spans.push(continuation_bar_span(entry.session_id.as_deref()));
                let indent = chain_chars
                    + if seg.continuation {
                        crate::history::WRAP_INDENT
                    } else {
                        0
                    };
                if indent > 0 {
                    spans.push(Span::raw(" ".repeat(indent)));
                }
                spans.push(Span::styled(seg.text, prompt_style));
                lines.push(Line::from(spans));
            }
        }
        return lines;
    }

    // ── File-change entries: single line, chain + dir + filename. ──
    let mut spans = prefix;
    let split = rel.rfind('/').map(|i| i + 1).unwrap_or(0);
    let (dir_part, file_part) = rel.split_at(split);
    let dir_chars = dir_part.chars().count();

    if let Some(idx) = match_indices {
        if let Some(chain_str) = chain_str_opt.as_ref() {
            spans.extend(highlighted_spans(chain_str, dim_base, match_style, idx, 0));
        }
        if !dir_part.is_empty() {
            spans.extend(highlighted_spans(
                dir_part,
                dim_base,
                match_style,
                idx,
                chain_chars,
            ));
        }
        spans.extend(highlighted_spans(
            file_part,
            file_base,
            match_style,
            idx,
            chain_chars + dir_chars,
        ));
    } else {
        if let Some(chain_str) = chain_str_opt {
            spans.push(Span::styled(chain_str, dim_base));
        }
        if !dir_part.is_empty() {
            spans.push(Span::styled(dir_part.to_string(), dim_base));
        }
        spans.push(Span::styled(file_part.to_string(), file_base));
    }

    // Cursor edits attributed to an agent (composer) get a trailing label in
    // that session's color, so agent edits stand out from manual saves.
    if let Some(name) = session_name {
        let color = entry.session_id.as_deref().map(session_color).unwrap_or(Color::Gray);
        spans.push(Span::styled(format!("  ⟨{}⟩", name), Style::default().fg(color)));
    }

    append_running(&mut spans, inner_width, running);
    vec![Line::from(spans)]
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
fn compute_chain_visuals(
    chains: &[Vec<String>],
    init_prev: Option<Vec<String>>,
    init_bg_idx: u8,
    disable_mask: bool,
) -> (Vec<Vec<bool>>, Vec<Color>) {
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
    (masks, bgs)
}

/// Render a per-child diff separator: timestamp, source, +/-, project chain,
/// path. No column padding — fields are space-separated. The whole row gets
/// a dark gray background that fills the panel width to read as a header.
fn separator_line(
    entry: &HistoryEntry,
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

fn draw_projects(f: &mut Frame, app: &App, area: Rect) {
    let active = app.active_panel == Panel::Projects;
    let block = Block::default()
        .title(format!(" Projects ({}) ", app.store.projects.len()))
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
    let total_rows = app.store.projects.len() + 1;
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

    let visible_timestamps: Vec<DateTime<Local>> = app.store.projects[first_proj..last_proj]
        .iter()
        .filter_map(|p| app.store.project_latest.get(p).copied())
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
            let proj = &app.store.projects[i];
            let selected = sel_display == row_idx;
            let chain = app
                .store
                .project_chains
                .get(proj)
                .map(|c| {
                    c.iter()
                        .map(|p| match app.store.project_names.get(p) {
                            Some(s) => s.clone(),
                            None => p
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or("?")
                                .to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(" → ")
                })
                .unwrap_or_else(|| App::project_label(proj));
            let count = app.store.project_counts.get(proj).copied().unwrap_or(0);
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
    let total = app.store.projects.len() + 1;
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
            .position(|p| p == f)
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
        .filter_map(|p| app.file_latest.get(p).copied())
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
            let file = &app.files[i];
            let selected = sel_display == row_idx;
            let chain = app.file_chains.get(file).cloned().unwrap_or_default();
            let label =
                file_display_for_panel(file, &chain, &app.store.project_names, project_selected);
            let count = app.file_counts.get(file).copied().unwrap_or(0);
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

fn draw_history(f: &mut Frame, app: &App, area: Rect) {
    let active = app.active_panel == Panel::History;
    let entry_count: usize = app.groups.iter().map(|g| g.entry_indices.len()).sum();
    let block = Block::default()
        .title(format!(" History ({}) ", entry_count))
        .borders(Borders::ALL)
        .border_type(if active {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(border_style(active));

    let inner = block.inner(area);
    f.render_widget(block, area);

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
            app.store.entries[entry_idx].timestamp
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
        let entry = &app.store.entries[entry_idx];
        let (chain, _) = entry.display_path_segments(&app.store.project_names);
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
    let (chain_masks, chain_bgs) = compute_chain_visuals(
        &visible_chains,
        anchor_prev,
        anchor_bg_idx,
        search_highlight,
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
        app.store.entries[entry_idx].session_id.clone()
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

            let lines: Vec<Line> = match row {
                DisplayRow::Group(gi) => {
                    let group = &app.groups[*gi];
                    let is_group = group.entry_indices.len() > 1;
                    let entry = &app.store.entries[entry_idx];
                    let count = group.entry_indices.len();
                    let time_color = if epochs[*gi] == 0 {
                        TIME_COLOR_RECENT
                    } else {
                        TIME_COLOR_OLD
                    };
                    let summary = app.diff_summaries.get(&entry_idx);

                    let leading = leading_marker(is_group, group.expanded, app.effective_auto_expand());
                    // Multi-entry groups display the child count instead of
                    // +/-, since +/- on a group reflects only one child.
                    // Prompts leave the column blank (no marker).
                    let change_spans = if is_group {
                        count_spans(count)
                    } else if entry.is_prompt() {
                        vec![Span::raw(" ".repeat(CHANGE_COL_WIDTH))]
                    } else {
                        summary_spans(summary)
                    };

                    let local_idx = row_idx - app.history_scroll;
                    let mask = chain_masks
                        .get(local_idx)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    let show_source = show_source_flags.get(local_idx).copied().unwrap_or(true);
                    // Only label the collapsed single-file row; a multi-entry
                    // group's children carry their own per-edit labels.
                    let session_name = if is_group {
                        None
                    } else {
                        cursor_session_name(app, entry)
                    };
                    group_row_line(
                        entry,
                        &app.store.project_names,
                        leading,
                        &ts,
                        ts_width,
                        time_color,
                        change_spans,
                        match_indices,
                        mask,
                        show_source,
                        app.is_entry_running(entry_idx),
                        inner_width,
                        app.row_height(row_idx),
                        app.prompt_lines,
                        session_name,
                    )
                }
                DisplayRow::Child { .. } => {
                    let entry = &app.store.entries[entry_idx];
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
                    let summary = app.diff_summaries.get(&entry_idx);
                    let local_idx = row_idx - app.history_scroll;
                    let show_source = show_source_flags.get(local_idx).copied().unwrap_or(true);

                    let mut spans = Vec::with_capacity(8);
                    spans.push(Span::raw("    "));
                    spans.push(Span::styled(
                        format!("{:<w$}", ts, w = ts_width),
                        Style::default().fg(time_color),
                    ));
                    spans.push(Span::raw(" "));
                    spans.extend(summary_spans(summary));
                    spans.push(Span::raw(" "));
                    spans.push(source_span_for(entry, show_source));
                    spans.push(Span::raw(" "));
                    spans.push(session_bar_span(entry.session_id.as_deref()));
                    let fname_style = if match_indices.is_some() {
                        match_style
                    } else {
                        Style::default()
                            .fg(if is_selected { Color::White } else { Color::Gray })
                    };
                    spans.push(Span::styled(fname.to_string(), fname_style));
                    if let Some(name) = cursor_session_name(app, entry) {
                        let color = entry
                            .session_id
                            .as_deref()
                            .map(session_color)
                            .unwrap_or(Color::Gray);
                        spans.push(Span::styled(
                            format!("  ⟨{}⟩", name),
                            Style::default().fg(color),
                        ));
                    }
                    append_running(&mut spans, inner_width, app.is_entry_running(entry_idx));
                    vec![Line::from(spans)]
                }
            };

            let local_idx = row_idx - app.history_scroll;
            let row_bg = chain_bgs
                .get(local_idx)
                .copied()
                .unwrap_or(CHAIN_BG_A);
            let mut style = Style::default().bg(row_bg);
            if is_selected {
                // Selection bg wins over the alternating band.
                style = style.bg(SELECTED_BG);
            }
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
        .map(|e| format!(" {} ", e.display_path(&app.store.project_names)))
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
    let is_prompt = app
        .current_entry()
        .map(|e| e.is_prompt())
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
                let base = Style::default().fg(PROMPT_COLOR);
                let mut spans = vec![Span::styled("> ", base)];
                spans.extend(text_spans(s, base));
                Line::from(spans)
            }
            DiffLine::ChildSeparator { entry_idx } => {
                let entry = &app.store.entries[*entry_idx];
                let ts_vec = format_panel_timestamps(&[entry.timestamp], app.absolute_time);
                let ts = ts_vec.into_iter().next().unwrap_or_default();
                let summary = app.diff_summaries.get(entry_idx);
                let session_name = entry
                    .session_id
                    .as_deref()
                    .and_then(|sid| app.store.cursor_session_names.get(sid))
                    .map(|s| s.as_str());
                separator_line(
                    entry,
                    &app.store.project_names,
                    &ts,
                    summary,
                    session_name,
                    inner_width,
                )
            }
        }
    };

    if is_prompt {
        // Wrap so long prompt lines fold instead of clipping. Paragraph::scroll
        // operates on visual rows, so diff_scroll naturally maps to wrapped rows.
        let lines: Vec<Line> = app.diff_lines.iter().map(render_dl).collect();
        let scroll = app.diff_scroll.min(u16::MAX as usize) as u16;
        let para = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        f.render_widget(para, inner);
    } else {
        let lines: Vec<Line> = app
            .diff_lines
            .iter()
            .skip(app.diff_scroll)
            .take(visible)
            .map(render_dl)
            .collect();
        let para = Paragraph::new(lines);
        f.render_widget(para, inner);
    }

    if app.diff_lines.len() > visible {
        let mut sb = ScrollbarState::new(app.diff_lines.len()).position(app.diff_scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut sb,
        );
    }
}

/// Render a generic typing prompt (Projects/Files panels). The History panel
/// has its own renderer because it carries the path-vs-regex kind tag.
fn panel_typing_prompt(prefix: &str, query: &str, total: usize) -> Line<'static> {
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
            };
            // For an empty regex query the matcher returns no results; show
            // a hint instead of a misleading "(0 matches)".
            let regex_invalid = app.search.kind == SearchKind::Content
                && !app.search.query.is_empty()
                && app.search.content_regex.is_none();
            let count_str = if app.search.query.is_empty() {
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
            ])
        }
        HelpMode::ProjectTyping => panel_typing_prompt(
            "/",
            &app.project_search.query,
            app.project_search.matches.len(),
        ),
        HelpMode::FileTyping => panel_typing_prompt(
            "/",
            &app.file_search.query,
            app.file_search.matches.len(),
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
            ])
        }
        HelpMode::HistoryActive => {
            let total = app.search.matches.len();
            let curr = if total == 0 { 0 } else { app.search.current + 1 };
            let mode_tag = match app.search.kind {
                SearchKind::Path => "",
                SearchKind::Content => " [regex]",
            };
            Line::from(vec![
                Span::styled(
                    format!(" [{}/{}]", curr, total),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    mode_tag.to_string(),
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
                Panel::Diff => {
                    spans.push(Span::styled("/", Style::default().fg(Color::White)));
                    spans.push(Span::styled(":find  ", Style::default().fg(Color::DarkGray)));
                }
            }
            spans.extend([
                Span::styled("o", Style::default().fg(Color::White)),
                Span::styled(":go to session  ", Style::default().fg(Color::DarkGray)),
                Span::styled("O", Style::default().fg(Color::White)),
                Span::styled(":open file  ", Style::default().fg(Color::DarkGray)),
                Span::styled("y", Style::default().fg(Color::White)),
                Span::styled(":copy path  ", Style::default().fg(Color::DarkGray)),
                Span::styled("t", Style::default().fg(Color::White)),
                Span::styled(":time format  ", Style::default().fg(Color::DarkGray)),
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
            Line::from(spans)
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::app::{App, Sources};
    use crate::history::{HistoryEntry, HistoryEntryKind, HistoryStore};
    use ratatui::{backend::TestBackend, Terminal};

    fn prompt(text: &str, session: &str) -> HistoryEntry {
        HistoryEntry {
            timestamp: Local::now() - chrono::Duration::minutes(7),
            file_path: PathBuf::from(format!("claude:{session}:{text}")),
            snapshot_path: PathBuf::new(),
            project_roots: vec![PathBuf::from("/dev/proj")],
            source: EditSource::ClaudeCode,
            kind: HistoryEntryKind::Prompt { text: text.to_string() },
            session_id: Some(session.to_string()),
            session_cwd: Some(PathBuf::from("/dev/proj")),
        }
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

    #[test]
    fn history_renders_bar_wrapping_and_footer() {
        let long = "this is a very long single line prompt that should wrap across \
                    several rows in the history list when the panel is narrow enough";
        let store = HistoryStore::from_entries(vec![
            prompt(long, "sessionAAAA-1111"),
            prompt("short one", "sessionBBBB-2222"),
        ]);
        let mut app = App::new(store, Sources::default());
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
                let e = &app.store.entries[app.groups[*gi].entry_indices[0]];
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
        // The `>` caret is gone from prompt rows (footer/help may still have '>').
    }
}
