use crate::app::{App, DisplayRow, Panel};
use crate::diff::{DiffLine, DiffSummary};
use crate::history::EditSource;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};

const ORANGE: Color = Color::Rgb(220, 120, 20);
const CURSOR_COLOR: Color = Color::Rgb(120, 170, 255);
const CLAUDE_COLOR: Color = Color::Rgb(217, 119, 87);
const SOURCE_WIDTH: usize = 6;

fn source_span(src: EditSource) -> Span<'static> {
    let (label, color) = match src {
        EditSource::Cursor => ("cursor", CURSOR_COLOR),
        EditSource::ClaudeCode => ("claude", CLAUDE_COLOR),
    };
    Span::styled(format!("{:<width$}", label, width = SOURCE_WIDTH), Style::default().fg(color))
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    // When Projects panel is active, hide the diff to give more space.
    let projects_active = app.active_panel == Panel::Projects;
    let projects_width: u16 = if projects_active { 26 * 4 } else { 26 * 2 };

    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(projects_width), Constraint::Min(0)])
        .split(area);

    let right = h_chunks[1];

    // Reserve 1 row at the bottom for help bar
    let v_outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let content_area = v_outer[0];
    let help_area = v_outer[1];

    // Re-split horizontally within the content area
    let h_content = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(projects_width), Constraint::Min(0)])
        .split(content_area);

    let right_content = h_content[1];

    // Right side: history + maybe diff
    let right_chunks = if projects_active {
        // No diff — full width for history
        vec![right_content]
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(right_content)
            .to_vec()
    };

    draw_projects(f, app, h_content[0]);
    draw_history(f, app, right_chunks[0]);
    if !projects_active {
        draw_diff(f, app, right_chunks[1]);
    }
    draw_help(f, help_area);

    // suppress unused warning
    let _ = right;
}

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
        None => vec![Span::raw(" ".repeat(9))],
    }
}

fn border_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn draw_projects(f: &mut Frame, app: &App, area: Rect) {
    let active = app.active_panel == Panel::Projects;
    let block = Block::default()
        .title(" Projects ")
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

    // Build rows: "All" first, then each project
    let sel_display = app.selected_project_idx.map(|i| i + 1).unwrap_or(0);

    // All items (display_idx → label + timestamp)
    let mut all_rows: Vec<Line> = Vec::new();

    // "All projects" row
    let all_selected = sel_display == 0;
    all_rows.push(Line::from(vec![
        Span::styled(
            " All projects",
            Style::default()
                .fg(if all_selected {
                    Color::White
                } else {
                    Color::Gray
                })
                .add_modifier(if all_selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
    ]));

    for (i, proj) in app.store.projects.iter().enumerate() {
        let selected = sel_display == i + 1;
        let label = App::project_label(proj);
        let ts = app
            .store
            .project_latest
            .get(proj)
            .map(|dt| app.timestamp_str(dt))
            .unwrap_or_default();

        // Truncate label to leave room for timestamp. Use char-counts (not byte
        // length) so the multi-byte `…` doesn't throw off right-alignment.
        let ts_chars = ts.chars().count();
        let max_label = (inner.width as usize).saturating_sub(ts_chars + 2);
        let label_chars: Vec<char> = label.chars().collect();
        let label_truncated = if label_chars.len() > max_label {
            let take = max_label.saturating_sub(1);
            let skip = label_chars.len() - take;
            let suffix: String = label_chars[skip..].iter().collect();
            format!("…{}", suffix)
        } else {
            label
        };

        let padding = (inner.width as usize)
            .saturating_sub(label_truncated.chars().count() + ts_chars + 1);

        let fg = if selected { Color::White } else { Color::Gray };
        let bold = if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        };

        all_rows.push(Line::from(vec![
            Span::styled(format!(" {label_truncated}"), Style::default().fg(fg).add_modifier(bold)),
            Span::raw(" ".repeat(padding)),
            Span::styled(ts, Style::default().fg(Color::DarkGray)),
        ]));
    }

    // Apply scroll + highlight
    let items: Vec<ListItem> = all_rows
        .into_iter()
        .enumerate()
        .skip(app.project_scroll)
        .take(visible)
        .map(|(i, line)| {
            let selected = i == sel_display;
            let style = if selected {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();

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

fn draw_history(f: &mut Frame, app: &App, area: Rect) {
    let active = app.active_panel == Panel::History;
    let block = Block::default()
        .title(" History ")
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
    let epochs = app.group_epochs();
    let sel_row = app.sel_display_row();

    let items: Vec<ListItem> = app
        .display_rows
        .iter()
        .enumerate()
        .skip(app.history_scroll)
        .take(visible)
        .map(|(row_idx, row)| {
            let is_selected = row_idx == sel_row;
            let line = match row {
                DisplayRow::Group(gi) => {
                    let group = &app.groups[*gi];
                    let single = group.entry_indices.len() == 1;
                    let entry_idx = group.entry_indices[0];
                    let entry = &app.store.entries[entry_idx];
                    let ts = app.timestamp_str(&entry.timestamp);
                    let path = entry.display_path();
                    let count = group.entry_indices.len();
                    let time_color = if epochs[*gi] == 0 { Color::Yellow } else { ORANGE };
                    let summary = app.diff_summaries.get(&entry_idx);

                    let mut spans = Vec::with_capacity(10);
                    if single {
                        spans.push(Span::raw("  "));
                    } else {
                        let chevron = if group.expanded { "▾" } else { "▸" };
                        spans.push(Span::styled(
                            format!("{chevron} "),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    spans.push(Span::styled(
                        format!("{:<8}", ts),
                        Style::default().fg(time_color),
                    ));
                    spans.push(Span::raw(" "));
                    spans.push(source_span(entry.source));
                    spans.push(Span::raw(" "));
                    spans.extend(summary_spans(summary));
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        path,
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                    ));
                    if !single {
                        spans.push(Span::styled(
                            format!(" ({})", count),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    Line::from(spans)
                }
                DisplayRow::Child { group_idx, child_local } => {
                    let entry_idx = app.groups[*group_idx].entry_indices[*child_local];
                    let entry = &app.store.entries[entry_idx];
                    let ts = app.timestamp_str(&entry.timestamp);
                    let fname = entry.file_name();
                    let time_color = if epochs[*group_idx] == 0 { Color::Yellow } else { ORANGE };
                    let summary = app.diff_summaries.get(&entry_idx);

                    let mut spans = Vec::with_capacity(8);
                    spans.push(Span::raw("    "));
                    spans.push(Span::styled(
                        format!("{:<8}", ts),
                        Style::default().fg(time_color),
                    ));
                    spans.push(Span::raw(" "));
                    spans.push(source_span(entry.source));
                    spans.push(Span::raw(" "));
                    spans.extend(summary_spans(summary));
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        fname.to_string(),
                        Style::default().fg(if is_selected { Color::White } else { Color::Gray }),
                    ));
                    Line::from(spans)
                }
            };

            let style = if is_selected {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();

    let list = List::new(items);
    f.render_widget(list, inner);

    // Scrollbar
    if app.display_rows.len() > visible {
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
    // Title shows the current file path
    let title = app
        .current_entry()
        .map(|e| format!(" {} ", e.display_path()))
        .unwrap_or_else(|| " Diff ".to_string());

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(border_style(false));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible = inner.height as usize;
    let lines: Vec<Line> = app
        .diff_lines
        .iter()
        .skip(app.diff_scroll)
        .take(visible)
        .map(|dl| match dl {
            DiffLine::Header(s) => Line::from(Span::styled(
                s.clone(),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )),
            DiffLine::Added(s) => Line::from(vec![
                Span::styled("+ ", Style::default().fg(Color::Green)),
                Span::styled(s.clone(), Style::default().fg(Color::Green)),
            ]),
            DiffLine::Removed(s) => Line::from(vec![
                Span::styled("- ", Style::default().fg(Color::Red)),
                Span::styled(s.clone(), Style::default().fg(Color::Red)),
            ]),
            DiffLine::Context(s) => Line::from(vec![
                Span::raw("  "),
                Span::styled(s.clone(), Style::default().fg(Color::DarkGray)),
            ]),
        })
        .collect();

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);

    if app.diff_lines.len() > visible {
        let mut sb = ScrollbarState::new(app.diff_lines.len()).position(app.diff_scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut sb,
        );
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let para = Paragraph::new(Line::from(vec![
        Span::styled(" Tab", Style::default().fg(Color::White)),
        Span::styled(":panel  ", Style::default().fg(Color::DarkGray)),
        Span::styled("j/k", Style::default().fg(Color::White)),
        Span::styled(":nav  ", Style::default().fg(Color::DarkGray)),
        Span::styled("^D/^U", Style::default().fg(Color::White)),
        Span::styled(":page  ", Style::default().fg(Color::DarkGray)),
        Span::styled("J/K", Style::default().fg(Color::White)),
        Span::styled(":diff scroll  ", Style::default().fg(Color::DarkGray)),
        Span::styled("t", Style::default().fg(Color::White)),
        Span::styled(":time format  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::White)),
        Span::styled(":quit", Style::default().fg(Color::DarkGray)),
    ]));
    f.render_widget(para, area);
}
