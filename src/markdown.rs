//! A small Markdown renderer for prompts and responses.
//!
//! [`render`] turns each source line into exactly one [`MdLine`]. The Diff
//! panel scrolls by source line, so the renderer must keep that one-to-one
//! relation. The renderer removes the syntax characters whose effect the
//! style already shows (`**`, `*`, `_`, `` ` ``, `~~`, `#`, the link URL),
//! draws a list bullet as a disk, and keeps a table row as cells, so the
//! wrap step can align the columns at the width of the panel.
//!
//! The renderer is line based on purpose. A full CommonMark parser joins the
//! lines of a paragraph, but a prompt is text that a person typed, and its
//! line breaks are part of the text.

use crate::prompt::Piece;
use ratatui::style::{Color, Modifier, Style};
use std::rc::Rc;

/// Color of inline code and of a code block line when the renderer does not
/// highlight the code.
pub const CODE_COLOR: Color = Color::Rgb(215, 153, 108);

/// The glyph that replaces a list bullet (`-`, `*`, `+`).
pub const BULLET: &str = "•";

/// One rendered source line.
#[derive(Debug, Clone, PartialEq)]
pub enum MdLine {
    /// A line of text. It keeps the leading whitespace of the source line.
    Text(Vec<Piece>),
    /// One row of a table.
    Row(TableRow),
}

/// One row of a table. All the rows of one table share `widths`.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRow {
    /// The cells of the row. A short row has fewer cells than `widths`.
    pub cells: Vec<Vec<Piece>>,
    /// The width of the widest cell of each column, in the whole table.
    pub widths: Rc<[usize]>,
    /// True for the row under the header (`|---|---|`).
    pub rule: bool,
}

impl MdLine {
    /// The pieces of the line. A table row joins its cells with ` │ `.
    #[cfg(test)]
    pub fn pieces(&self) -> Vec<Piece> {
        match self {
            MdLine::Text(pieces) => pieces.clone(),
            MdLine::Row(row) => row.pieces_joined(),
        }
    }
}

impl TableRow {
    /// The cells of the row, joined with ` │ `.
    pub fn pieces_joined(&self) -> Vec<Piece> {
        let mut out = Vec::new();
        for (i, cell) in self.cells.iter().enumerate() {
            if i > 0 {
                out.push(Piece {
                    text: " │ ".to_string(),
                    style: Style::default(),
                });
            }
            out.extend(cell.iter().cloned());
        }
        out
    }
}

/// Render `text`, with `prose` as the color of plain text. Returns one line
/// for each line of `str::lines`, and one empty line for an empty text.
/// `code_style` gives the pieces of one line of a fenced code block, from
/// the language of the fence and the line. The characters of a code line
/// must stay the same, so the line count does not depend on `code_style`.
pub fn render(
    text: &str,
    prose: Color,
    code_style: &mut dyn FnMut(&str, &str) -> Vec<Piece>,
) -> Vec<MdLine> {
    let lines: Vec<&str> = text.lines().collect();
    let base = Style::default().fg(prose);
    let mut out: Vec<MdLine> = Vec::with_capacity(lines.len().max(1));
    // The language of the open code fence, if a fence is open.
    let mut fence: Option<(String, String)> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if let Some((marker, lang)) = &fence {
            if trimmed.starts_with(marker.as_str()) && trimmed.trim_end() == marker.as_str() {
                fence = None;
                out.push(MdLine::Text(Vec::new()));
            } else {
                out.push(MdLine::Text(code_style(lang, line)));
            }
            i += 1;
            continue;
        }
        if let Some(marker) = fence_marker(trimmed) {
            let lang = trimmed[marker.len()..].trim().to_string();
            fence = Some((marker, lang));
            out.push(MdLine::Text(Vec::new()));
            i += 1;
            continue;
        }
        if let Some(rows) = table_at(&lines[i..]) {
            out.extend(table_rows(&lines[i..i + rows], base));
            i += rows;
            continue;
        }
        out.push(MdLine::Text(text_line(line, base)));
        i += 1;
    }
    if out.is_empty() {
        out.push(MdLine::Text(Vec::new()));
    }
    out
}

/// The fence (three or more backticks or tildes) that opens a code block.
fn fence_marker(trimmed: &str) -> Option<String> {
    for ch in ['`', '~'] {
        let n = trimmed.chars().take_while(|&c| c == ch).count();
        if n >= 3 {
            return Some(ch.to_string().repeat(n));
        }
    }
    None
}

/// Render one line that is not in a code block and not in a table: a
/// heading, a list item, or a paragraph line.
fn text_line(line: &str, base: Style) -> Vec<Piece> {
    let body = line.trim_start();
    let indent = &line[..line.len() - body.len()];
    let mut out = Vec::new();
    if !indent.is_empty() {
        out.push(Piece {
            text: indent.to_string(),
            style: base,
        });
    }
    // A heading: one to six `#` and a space. The text is bold.
    let hashes = body.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && body[hashes..].starts_with(' ') {
        let bold = base.add_modifier(Modifier::BOLD);
        out.extend(inline(body[hashes..].trim(), bold));
        return out;
    }
    // A list item: a bullet and a space.
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = body.strip_prefix(bullet) {
            out.push(Piece {
                text: format!("{BULLET} "),
                style: base,
            });
            out.extend(inline(rest, base));
            return out;
        }
    }
    out.extend(inline(body, base));
    out
}

// ── Tables ────────────────────────────────────────────────────────────────

/// The number of lines of the table that starts at `lines[0]`, or `None`
/// when no table starts there. A table is a row that starts with `|`, a
/// rule row (`|---|:--:|`), and the rows after them that start with `|`.
fn table_at(lines: &[&str]) -> Option<usize> {
    let is_row = |l: &str| l.trim_start().starts_with('|');
    if lines.len() < 2 || !is_row(lines[0]) || !is_rule(lines[1]) {
        return None;
    }
    Some(2 + lines[2..].iter().take_while(|l| is_row(l)).count())
}

/// True for the rule row of a table: only cells of `-` and `:`.
fn is_rule(line: &str) -> bool {
    let line = line.trim();
    if !line.starts_with('|') {
        return false;
    }
    let cells = split_cells(line);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.trim();
            c.contains('-') && c.chars().all(|ch| ch == '-' || ch == ':')
        })
}

/// The cells of one table row, without the outer pipes. A pipe inside a
/// code span or after a backslash does not split a cell.
fn split_cells(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = line.strip_suffix('|').unwrap_or(line);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut in_code = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    if next != '|' {
                        cur.push('\\');
                    }
                    cur.push(next);
                }
            }
            '`' => {
                in_code = !in_code;
                cur.push(c);
            }
            '|' if !in_code => cells.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    cells.push(cur);
    cells
}

/// Render the rows of one table. The header cells are bold.
fn table_rows(lines: &[&str], base: Style) -> Vec<MdLine> {
    let mut rows: Vec<(Vec<Vec<Piece>>, bool)> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 1 {
                return (Vec::new(), true);
            }
            let style = if i == 0 { base.add_modifier(Modifier::BOLD) } else { base };
            let cells = split_cells(line)
                .iter()
                .map(|c| inline(c.trim(), style))
                .collect();
            (cells, false)
        })
        .collect();
    let columns = rows.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for (cells, _) in &rows {
        for (w, cell) in widths.iter_mut().zip(cells) {
            *w = (*w).max(cell.iter().map(|p| p.text.chars().count()).sum());
        }
    }
    let widths: Rc<[usize]> = widths.into();
    rows.drain(..)
        .map(|(cells, rule)| {
            MdLine::Row(TableRow {
                cells,
                widths: widths.clone(),
                rule,
            })
        })
        .collect()
}

// ── Inline syntax ─────────────────────────────────────────────────────────

/// Render the inline syntax of `s`: code spans, bold, italic, strikethrough
/// and links. The renderer removes a marker only when the marker has a
/// partner that closes it, so a lone `*` or `_` stays in the text.
pub fn inline(s: &str, base: Style) -> Vec<Piece> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<Piece> = Vec::new();
    let mut cur = String::new();
    let mut bold = false;
    let mut italic = false;
    let mut strike = false;
    let style_of = |bold: bool, italic: bool, strike: bool| {
        let mut st = base;
        if bold {
            st = st.add_modifier(Modifier::BOLD);
        }
        if italic {
            st = st.add_modifier(Modifier::ITALIC);
        }
        if strike {
            st = st.add_modifier(Modifier::CROSSED_OUT);
        }
        st
    };
    let flush = |cur: &mut String, out: &mut Vec<Piece>, style: Style| {
        if !cur.is_empty() {
            out.push(Piece {
                text: std::mem::take(cur),
                style,
            });
        }
    };
    let is_word = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric());
    let is_space = |c: Option<&char>| c.is_none_or(|c| c.is_whitespace());

    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let style = style_of(bold, italic, strike);
        // A backslash before a punctuation character makes it literal.
        if c == '\\' && chars.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()) {
            cur.push(chars[i + 1]);
            i += 2;
            continue;
        }
        // A code span: a run of backticks, closed by a run of the same length.
        if c == '`' {
            let n = chars[i..].iter().take_while(|&&ch| ch == '`').count();
            if let Some(end) = find_run(&chars, i + n, '`', n) {
                flush(&mut cur, &mut out, style);
                let code: String = chars[i + n..end].iter().collect();
                let code = if code.len() > 2 && code.starts_with(' ') && code.ends_with(' ') {
                    code[1..code.len() - 1].to_string()
                } else {
                    code
                };
                out.push(Piece {
                    text: code,
                    style: style.fg(CODE_COLOR),
                });
                i = end + n;
                continue;
            }
            cur.extend(&chars[i..i + n]);
            i += n;
            continue;
        }
        // A link: `[text](url)` shows the text, underlined.
        if c == '[' {
            if let Some((text_end, url_end)) = link_at(&chars, i) {
                flush(&mut cur, &mut out, style);
                let text: String = chars[i + 1..text_end].iter().collect();
                out.extend(inline(&text, style.add_modifier(Modifier::UNDERLINED)));
                i = url_end + 1;
                continue;
            }
        }
        // Bold and strikethrough: two equal markers. `__` is not bold, so a
        // Python name such as `__init__` stays as it is.
        let pair = chars.get(i + 1) == Some(&c);
        if pair && c == '_' {
            cur.push_str("__");
            i += 2;
            continue;
        }
        if pair && (c == '*' || c == '~') {
            let prev = i.checked_sub(1).and_then(|p| chars.get(p));
            let next = chars.get(i + 2);
            let on = if c == '~' { strike } else { bold };
            let toggles = if on {
                    !is_space(prev)
                } else {
                !is_space(next) && find_pair(&chars, i + 2, c).is_some()
            };
            if toggles {
                flush(&mut cur, &mut out, style);
                if c == '~' {
                    strike = !strike;
                } else {
                    bold = !bold;
                }
                i += 2;
                continue;
            }
        }
        // Italic: one marker.
        if (c == '*' || c == '_') && !pair {
            let prev = i.checked_sub(1).and_then(|p| chars.get(p));
            let next = chars.get(i + 1);
            let word_ok = c != '_' || if italic { !is_word(next) } else { !is_word(prev) };
            let toggles = word_ok
                && if italic {
                    !is_space(prev)
                } else {
                    !is_space(next) && find_single(&chars, i + 1, c).is_some()
                };
            if toggles {
                flush(&mut cur, &mut out, style);
                italic = !italic;
                i += 1;
                continue;
            }
        }
        cur.push(c);
        i += 1;
    }
    flush(&mut cur, &mut out, style_of(bold, italic, strike));
    out
}

/// The index of the first run of exactly `n` characters `ch` at or after
/// `from`.
fn find_run(chars: &[char], from: usize, ch: char, n: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == ch {
            let run = chars[i..].iter().take_while(|&&c| c == ch).count();
            if run == n {
                return Some(i);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// The index of a closing pair `chch` after `from` that follows a
/// non-space character.
fn find_pair(chars: &[char], from: usize, ch: char) -> Option<usize> {
    (from + 1..chars.len().saturating_sub(1)).find(|&j| {
        chars[j] == ch && chars[j + 1] == ch && !chars[j - 1].is_whitespace()
    })
}

/// The index of a closing single `ch` after `from` that follows a
/// non-space character and is not part of a pair.
fn find_single(chars: &[char], from: usize, ch: char) -> Option<usize> {
    (from + 1..chars.len()).find(|&j| {
        chars[j] == ch
            && !chars[j - 1].is_whitespace()
            && chars[j - 1] != ch
            && chars.get(j + 1) != Some(&ch)
    })
}

/// For a link that starts at `chars[i] == '['`: the index of the `]` and the
/// index of the `)` that closes the URL.
fn link_at(chars: &[char], i: usize) -> Option<(usize, usize)> {
    let text_end = (i + 1..chars.len()).find(|&j| chars[j] == ']')?;
    if text_end == i + 1 || chars.get(text_end + 1) != Some(&'(') {
        return None;
    }
    let url_end = (text_end + 2..chars.len()).find(|&j| chars[j] == ')')?;
    if chars[text_end + 2..url_end].iter().any(|c| c.is_whitespace()) {
        return None;
    }
    Some((text_end, url_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(_lang: &str, line: &str) -> Vec<Piece> {
        vec![Piece {
            text: line.to_string(),
            style: Style::default().fg(CODE_COLOR),
        }]
    }

    fn text(line: &MdLine) -> String {
        line.pieces().iter().map(|p| p.text.as_str()).collect()
    }

    fn texts(md: &str) -> Vec<String> {
        render(md, Color::White, &mut plain).iter().map(text).collect()
    }

    fn piece<'a>(line: &'a MdLine, s: &str) -> &'a Piece {
        let MdLine::Text(pieces) = line else { panic!("not a text line") };
        pieces.iter().find(|p| p.text == s).unwrap_or_else(|| panic!("no piece {s:?} in {pieces:?}"))
    }

    #[test]
    fn the_renderer_removes_the_inline_markers() {
        let md = render("a **bold** b *it* c `code` d ~~old~~", Color::White, &mut plain);
        assert_eq!(text(&md[0]), "a bold b it c code d old");
        assert!(piece(&md[0], "bold").style.add_modifier.contains(Modifier::BOLD));
        assert!(piece(&md[0], "it").style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(piece(&md[0], "code").style.fg, Some(CODE_COLOR));
        assert!(piece(&md[0], "old").style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn a_lone_marker_stays_in_the_text() {
        assert_eq!(texts("2 * 3 = 6"), vec!["2 * 3 = 6"]);
        assert_eq!(texts("snake_case and __init__"), vec!["snake_case and __init__"]);
        assert_eq!(texts("a ` b"), vec!["a ` b"]);
        assert_eq!(texts("**open"), vec!["**open"]);
    }

    #[test]
    fn a_list_bullet_is_a_disk_in_the_prose_color() {
        let md = render("- one\n  * two\n+ three", Color::White, &mut plain);
        assert_eq!(md.iter().map(text).collect::<Vec<_>>(), vec!["• one", "  • two", "• three"]);
        assert_eq!(piece(&md[0], "• ").style.fg, Some(Color::White));
    }

    #[test]
    fn a_heading_is_bold_without_the_hashes() {
        let md = render("## Plan", Color::White, &mut plain);
        assert_eq!(text(&md[0]), "Plan");
        assert!(piece(&md[0], "Plan").style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_link_shows_its_text() {
        assert_eq!(texts("see [the docs](https://x.y/z) now"), vec!["see the docs now"]);
    }

    #[test]
    fn a_code_block_keeps_one_line_per_source_line() {
        let md = "before\n```rust\nlet a = **b**;\n```\nafter";
        assert_eq!(texts(md), vec!["before", "", "let a = **b**;", "", "after"]);
    }

    #[test]
    fn a_table_keeps_one_row_per_source_line() {
        let md = render("| a | bb |\n|---|---|\n| ccc | `d|e` |", Color::White, &mut plain);
        assert_eq!(md.len(), 3);
        let MdLine::Row(head) = &md[0] else { panic!("not a row") };
        assert_eq!(&*head.widths, &[3, 3]);
        let MdLine::Row(rule) = &md[1] else { panic!("not a row") };
        assert!(rule.rule);
        let MdLine::Row(body) = &md[2] else { panic!("not a row") };
        assert_eq!(body.cells.len(), 2, "a pipe in a code span does not split a cell");
    }

    #[test]
    fn the_line_count_matches_str_lines() {
        for md in ["", "a", "a\n", "a\n\nb", "```\ncode", "| a |\n|---|"] {
            assert_eq!(
                render(md, Color::White, &mut plain).len(),
                md.lines().count().max(1),
                "{md:?}"
            );
        }
    }
}
