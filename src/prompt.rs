//! Prompt text rendering: Markdown rendering plus a word-wrap that keeps the
//! styles.
//!
//! A prompt is Markdown, so [`highlight`] renders it with [`crate::markdown`]
//! and colors the fenced code blocks with syntect (the engine that `bat`
//! uses). The result is one [`MdLine`] per source line. [`wrap`] then folds
//! those lines into panel-width lines and preserves each piece's style.
//!
//! The wrap step is separate from the render step on purpose: the panels put
//! a gutter in front of each response line, and a table row needs the width
//! of the panel to align its columns.

use crate::markdown::{self, MdLine, TableRow};

use ratatui::style::{Color, Modifier, Style};
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Color of prompt prose. The highlighter maps the theme's own default
/// foreground onto this color, so plain text has this color and only
/// Markdown constructs (code, headings, emphasis) take a theme color. The
/// prompt is a dim white, so the response below it is the brighter text.
pub const PROMPT_COLOR: Color = Color::Rgb(170, 170, 170);

/// Color of response prose: a bright white, brighter than [`PROMPT_COLOR`].
pub const RESPONSE_COLOR: Color = Color::White;

/// The vertical line in the gutter of a response line. The line separates
/// the response from the prompt above it.
pub const RESPONSE_BAR: &str = "│";

/// Color of [`RESPONSE_BAR`]. Faint gray, quieter than the text.
pub const RESPONSE_BAR_COLOR: Color = Color::Rgb(100, 100, 100);

/// Blank columns in front of [`RESPONSE_BAR`].
const RESPONSE_BAR_INDENT: usize = 2;

/// Columns of the gutter in front of each response line: the bar indent,
/// the bar, and one blank column.
pub const RESPONSE_GUTTER: usize = RESPONSE_BAR_INDENT + 2;

/// Columns a tab takes when the wrapper measures a line's own indent.
const TAB_WIDTH: usize = 4;

/// A run of prompt text that shares one style.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub text: String,
    pub style: Style,
}

impl Piece {
    fn len(&self) -> usize {
        self.text.chars().count()
    }
}

/// One physical line produced by [`wrap`] or [`wrap_line`].
#[derive(Debug, Clone, PartialEq)]
pub struct WrappedLine {
    pub pieces: Vec<Piece>,
    /// Columns of indent the renderer draws in front of `pieces`: the
    /// leading whitespace of the source line.
    pub indent: usize,
    /// True for a line of the response. The renderer then draws the
    /// response gutter (see [`RESPONSE_GUTTER`]) in front of the indent.
    pub response: bool,
}

impl WrappedLine {
    /// Columns in front of `pieces`: the response gutter and the indent.
    pub fn offset(&self) -> usize {
        self.indent + if self.response { RESPONSE_GUTTER } else { 0 }
    }

    /// The line's characters without their styles.
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.pieces.iter().map(|p| p.text.as_str()).collect()
    }
}

// ── Highlighting ──────────────────────────────────────────────────────────

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME: LazyLock<Theme> = LazyLock::new(|| {
    let mut themes = ThemeSet::load_defaults();
    themes
        .themes
        .remove("base16-ocean.dark")
        .or_else(|| themes.themes.values().next().cloned())
        .expect("syntect ships at least one default theme")
});

/// The theme's own default foreground. Plain code in a code block has this
/// color.
static THEME_FG: LazyLock<Option<(u8, u8, u8)>> = LazyLock::new(|| {
    THEME
        .settings
        .foreground
        .map(|c| (c.r, c.g, c.b))
});

fn to_ratatui(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Maximum number of highlighted prompts the process keeps. The History panel
/// re-renders every frame, so a cache is what keeps syntect off the hot path.
const CACHE_CAP: usize = 512;

thread_local! {
    static CACHE: RefCell<HashMap<u64, Rc<Vec<MdLine>>>> =
        RefCell::new(HashMap::new());
}

fn hash_of(text: &str, prose: Color) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    prose.hash(&mut h);
    h.finish()
}

/// Render `text` as Markdown, with `prose` as the color of plain text, and
/// color the fenced code blocks. Returns one line per source line. The
/// result is cached by content hash and prose color.
pub fn highlight(text: &str, prose: Color) -> Rc<Vec<MdLine>> {
    let key = hash_of(text, prose);
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let out = Rc::new(highlight_uncached(text, prose));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_CAP {
            c.clear();
        }
        c.insert(key, out.clone());
    });
    out
}

fn highlight_uncached(text: &str, prose: Color) -> Vec<MdLine> {
    // One highlighter per code block. A new language starts a new block.
    let mut block: Option<(String, HighlightLines<'static>)> = None;
    let mut code_style = |lang: &str, line: &str| -> Vec<Piece> {
        if block.as_ref().is_none_or(|(l, _)| l != lang) {
            let syntax = SYNTAXES
                .find_syntax_by_token(lang)
                .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text());
            block = Some((lang.to_string(), HighlightLines::new(syntax, &THEME)));
        }
        let (_, hl) = block.as_mut().expect("the block was set above");
        match hl.highlight_line(&format!("{line}\n"), &SYNTAXES) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(s, t)| Piece {
                    text: t.trim_end_matches('\n').to_string(),
                    style: to_ratatui(s),
                })
                .filter(|p| !p.text.is_empty())
                .collect(),
            // A highlighter failure must not lose the line.
            Err(_) => plain_code(lang, line),
        }
    };
    markdown::render(text, prose, &mut code_style)
}

/// One code line in one color, for the row-height math. It has the same
/// characters as the highlighted line.
fn plain_code(_lang: &str, line: &str) -> Vec<Piece> {
    let fg = THEME_FG.map_or(markdown::CODE_COLOR, |(r, g, b)| Color::Rgb(r, g, b));
    vec![Piece {
        text: line.to_string(),
        style: Style::default().fg(fg),
    }]
}

// ── Wrapping ──────────────────────────────────────────────────────────────

/// One word: the pieces between two whitespace runs. A word can hold more
/// than one piece when the highlighter changes style inside it.
#[derive(Debug, Clone)]
struct Word {
    pieces: Vec<Piece>,
}

impl Word {
    fn len(&self) -> usize {
        self.pieces.iter().map(Piece::len).sum()
    }

    /// Split off the first `n` characters. Returns (head, tail).
    fn split_at_chars(self, n: usize) -> (Vec<Piece>, Word) {
        let mut head: Vec<Piece> = Vec::new();
        let mut tail: Vec<Piece> = Vec::new();
        let mut taken = 0usize;
        for p in self.pieces {
            let plen = p.len();
            if taken >= n {
                tail.push(p);
            } else if taken + plen <= n {
                taken += plen;
                head.push(p);
            } else {
                let cut = n - taken;
                let byte = p
                    .text
                    .char_indices()
                    .nth(cut)
                    .map(|(i, _)| i)
                    .unwrap_or(p.text.len());
                head.push(Piece {
                    text: p.text[..byte].to_string(),
                    style: p.style,
                });
                tail.push(Piece {
                    text: p.text[byte..].to_string(),
                    style: p.style,
                });
                taken = n;
            }
        }
        (head, Word { pieces: tail })
    }
}

/// Split one source line into words, dropping every whitespace run.
fn words_of(line: &[Piece]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    // True while the last character read was whitespace, so the next
    // non-blank fragment opens a new word.
    let mut boundary = true;
    for piece in line {
        // `split_whitespace` loses the boundary between two pieces, so walk
        // the whitespace runs and the word runs one after the other instead.
        let mut rest = piece.text.as_str();
        while !rest.is_empty() {
            let ws_len = rest
                .char_indices()
                .find(|(_, c)| !c.is_whitespace())
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            if ws_len > 0 {
                boundary = true;
                rest = &rest[ws_len..];
                continue;
            }
            let word_len = rest
                .char_indices()
                .find(|(_, c)| c.is_whitespace())
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            let frag = Piece {
                text: rest[..word_len].to_string(),
                style: piece.style,
            };
            if boundary || words.is_empty() {
                words.push(Word { pieces: vec![frag] });
            } else {
                words.last_mut().unwrap().pieces.push(frag);
            }
            boundary = false;
            rest = &rest[word_len..];
        }
    }
    words
}

/// Columns of leading whitespace on a source line. The wrapper adds this to
/// the line's indent, so a wrapped list item nests under its own text rather
/// than under the bullet. Capped at half the width so a pathological line
/// cannot squeeze the text out of the panel.
fn leading_indent(line: &[Piece], width: usize) -> usize {
    let mut n = 0usize;
    for piece in line {
        for ch in piece.text.chars() {
            match ch {
                '\t' => n += TAB_WIDTH,
                c if c.is_whitespace() => n += 1,
                _ => return n.min(width / 2),
            }
        }
    }
    n.min(width / 2)
}

/// Fold `src` into lines of at most `lw` columns. Whitespace collapses to
/// single spaces, and a word longer than the line splits at the column edge.
/// Stops after `limit` lines, so a huge prompt costs little.
fn fill(src: &[Piece], lw: usize, limit: usize) -> Vec<Vec<Piece>> {
    let lw = lw.max(1);
    let mut lines: Vec<Vec<Piece>> = Vec::new();
    let mut cur: Vec<Piece> = Vec::new();
    let mut cur_len = 0usize;
    for word in words_of(src) {
        let mut rest = word;
        loop {
            if lines.len() >= limit {
                return lines;
            }
            let wlen = rest.len();
            if cur_len == 0 {
                if wlen <= lw {
                    cur = rest.pieces;
                    cur_len = wlen;
                    break;
                }
                // The word is longer than a whole line: cut it.
                let (head, tail) = rest.split_at_chars(lw);
                lines.push(head);
                rest = tail;
            } else if cur_len + 1 + wlen <= lw {
                let style = cur.last().map(|p| p.style).unwrap_or_default();
                cur.push(Piece {
                    text: " ".to_string(),
                    style,
                });
                cur.extend(rest.pieces);
                cur_len += 1 + wlen;
                break;
            } else {
                lines.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
        }
    }
    if cur_len > 0 && lines.len() < limit {
        lines.push(cur);
    }
    lines
}

/// Append the rendered lines of one source line to `out`. A response line
/// also has the response gutter; see [`RESPONSE_GUTTER`].
///
/// Returns false when `out` grew past `max_lines`, so the caller stops: one
/// line more than we keep is enough to know the text overflowed, and it
/// bounds the work on a huge prompt.
fn wrap_into(
    out: &mut Vec<WrappedLine>,
    src: &MdLine,
    width: usize,
    response: bool,
    max_lines: usize,
) -> bool {
    let gutter = if response { RESPONSE_GUTTER } else { 0 };
    let avail = width.saturating_sub(gutter).max(1);
    let limit = (max_lines + 1).saturating_sub(out.len());
    match src {
        MdLine::Text(pieces) => {
            // Every line of the source line, soft wraps included, has the
            // leading whitespace of the source line as its indent.
            let indent = leading_indent(pieces, avail);
            for line in fill(pieces, avail.saturating_sub(indent), limit) {
                out.push(WrappedLine {
                    pieces: line,
                    indent,
                    response,
                });
            }
        }
        MdLine::Row(row) => match table_row_lines(row, avail, limit) {
            Some(lines) => out.extend(lines.into_iter().map(|pieces| WrappedLine {
                pieces,
                indent: 0,
                response,
            })),
            // The panel is too narrow for the columns: wrap the row as text.
            None => return wrap_into(out, &MdLine::Text(row.pieces_joined()), width, response, max_lines),
        },
    }
    out.len() <= max_lines
}

/// The columns between two table cells: a blank, the rule and a blank.
const CELL_GAP: usize = 3;

/// Color of the rules and the column separators of a table.
const TABLE_RULE_COLOR: Color = Color::Rgb(100, 100, 100);

/// The width of each column of a table in `avail` columns. A table that
/// fits keeps the width of its widest cells. A wider table shrinks its
/// widest columns first, so a narrow column keeps its width. `None` when
/// the panel cannot give each column one character.
fn column_widths(natural: &[usize], avail: usize) -> Option<Vec<usize>> {
    let n = natural.len();
    let gaps = CELL_GAP * n.saturating_sub(1);
    let budget = avail.checked_sub(gaps)?;
    if budget < n {
        return None;
    }
    let natural: Vec<usize> = natural.iter().map(|w| (*w).max(1)).collect();
    if natural.iter().sum::<usize>() <= budget {
        return Some(natural);
    }
    // Give each column its fair share of the budget, narrowest first. A
    // column that needs less than its share leaves the rest to the others.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| natural[i]);
    let mut widths = vec![0usize; n];
    let mut left = budget;
    for (k, &i) in order.iter().enumerate() {
        let share = left / (n - k);
        widths[i] = natural[i].min(share).max(1);
        left -= widths[i];
    }
    Some(widths)
}

/// The lines of one table row in `avail` columns, at most `limit` of them.
/// Each cell wraps inside its column, so the row is as tall as its tallest
/// cell. `None` when the panel is too narrow for the columns.
fn table_row_lines(row: &TableRow, avail: usize, limit: usize) -> Option<Vec<Vec<Piece>>> {
    let widths = column_widths(&row.widths, avail)?;
    let rule_style = Style::default().fg(TABLE_RULE_COLOR);
    if row.rule {
        let text = widths
            .iter()
            .map(|w| "─".repeat(*w))
            .collect::<Vec<_>>()
            .join("─┼─");
        return Some(vec![vec![Piece { text, style: rule_style }]]);
    }
    let empty: Vec<Piece> = Vec::new();
    let cells: Vec<Vec<Vec<Piece>>> = widths
        .iter()
        .enumerate()
        .map(|(c, w)| fill(row.cells.get(c).unwrap_or(&empty), *w, limit))
        .collect();
    let height = cells.iter().map(Vec::len).max().unwrap_or(0).clamp(1, limit.max(1));
    let mut lines = Vec::with_capacity(height);
    for l in 0..height {
        let mut line: Vec<Piece> = Vec::new();
        for (c, w) in widths.iter().enumerate() {
            if c > 0 {
                line.push(Piece {
                    text: " │ ".to_string(),
                    style: rule_style,
                });
            }
            let part = cells[c].get(l).cloned().unwrap_or_default();
            let used: usize = part.iter().map(Piece::len).sum();
            line.extend(part);
            // The last column needs no pad.
            if c + 1 < widths.len() && used < *w {
                line.push(Piece {
                    text: " ".repeat(w - used),
                    style: Style::default(),
                });
            }
        }
        lines.push(line);
    }
    Some(lines)
}

/// Cap `out` at `max_lines` lines, marking the cut with a trailing `…`.
fn cap(out: &mut Vec<WrappedLine>, width: usize, max_lines: usize) {
    if out.len() <= max_lines {
        return;
    }
    out.truncate(max_lines);
    if let Some(last) = out.last_mut() {
        let lw = width.saturating_sub(last.offset()).max(1);
        truncate_with_ellipsis(&mut last.pieces, lw);
    }
}

/// Word-wrap the highlighted source `lines` of one prompt into at most
/// `max_lines` lines of `width` columns.
///
/// Every line starts at the column of the first line, plus the leading
/// whitespace of its source line; see [`wrap_into`]. Blank source lines drop
/// out, because the History panel has only a few lines per row to spend.
/// The function always returns between 1 and `max_lines` lines.
pub fn wrap(lines: &[MdLine], width: usize, max_lines: usize) -> Vec<WrappedLine> {
    let width = width.max(1);
    let max_lines = max_lines.max(1);
    let mut out: Vec<WrappedLine> = Vec::new();
    for src in lines {
        if !wrap_into(&mut out, src, width, false, max_lines) {
            break;
        }
    }
    if out.is_empty() {
        out.push(WrappedLine {
            pieces: Vec::new(),
            indent: 0,
            response: false,
        });
    }
    cap(&mut out, width, max_lines);
    out
}

/// Wrap one source line on its own. The Diff panel scrolls by source line, so
/// it formats one line per step, and passes `response: true` for a line of
/// the response. A blank line yields one blank rendered line, so the
/// paragraph break stays visible where there is room to show it. A blank
/// response line keeps the gutter, so the bar has no gap.
pub fn wrap_line(
    src: &MdLine,
    width: usize,
    response: bool,
    max_lines: usize,
) -> Vec<WrappedLine> {
    let width = width.max(1);
    let max_lines = max_lines.max(1);
    let mut out: Vec<WrappedLine> = Vec::new();
    wrap_into(&mut out, src, width, response, max_lines);
    if out.is_empty() {
        out.push(WrappedLine {
            pieces: Vec::new(),
            indent: 0,
            response,
        });
    }
    cap(&mut out, width, max_lines);
    out
}

/// Cut `pieces` so the line plus a trailing `…` fits in `lw` columns.
fn truncate_with_ellipsis(pieces: &mut Vec<Piece>, lw: usize) {
    let keep = lw.saturating_sub(1);
    let mut used = 0usize;
    let mut cut_at: Option<(usize, usize)> = None;
    for (i, p) in pieces.iter().enumerate() {
        let plen = p.len();
        if used + plen > keep {
            cut_at = Some((i, keep - used));
            break;
        }
        used += plen;
    }
    if let Some((i, chars)) = cut_at {
        let byte = pieces[i]
            .text
            .char_indices()
            .nth(chars)
            .map(|(b, _)| b)
            .unwrap_or(pieces[i].text.len());
        pieces[i].text.truncate(byte);
        pieces.truncate(i + 1);
        if pieces[i].text.is_empty() {
            pieces.pop();
        }
    }
    let style = pieces
        .last()
        .map(|p| p.style)
        .unwrap_or_else(|| Style::default().fg(PROMPT_COLOR));
    pieces.push(Piece {
        text: "…".to_string(),
        style,
    });
}

/// Wrap the `response` lines so they continue a prompt block that [`wrap`]
/// produced: every line has the response gutter. The block gets its own
/// `max_lines` cap, so a long prompt does not push the response out of the
/// row.
fn wrap_response(lines: &[MdLine], width: usize, max_lines: usize) -> Vec<WrappedLine> {
    let width = width.max(1);
    let max_lines = max_lines.max(1);
    let mut out: Vec<WrappedLine> = Vec::new();
    for src in lines {
        if !wrap_into(&mut out, src, width, true, max_lines) {
            break;
        }
    }
    cap(&mut out, width, max_lines);
    out
}

/// Wrap one turn: the prompt block, then the response block when there is
/// one. Each block takes at most `max_lines` lines.
fn wrap_turn(
    prompt: &[MdLine],
    response: Option<&[MdLine]>,
    width: usize,
    max_lines: usize,
) -> Vec<WrappedLine> {
    let mut out = wrap(prompt, width, max_lines);
    if let Some(response) = response {
        out.extend(wrap_response(response, width, max_lines));
    }
    out
}

/// Render `text` without the code colors. The History panel uses this for
/// row-height math: the code colors never change the characters, so the
/// line count matches what the highlighted render gives, at a lower cost.
fn plain_render(text: &str, prose: Color) -> Vec<MdLine> {
    markdown::render(text, prose, &mut plain_code)
}

/// Wrap plain `text`, for row-height math. The line count matches what
/// [`wrap_highlighted`] returns for the same text.
pub fn wrap_plain(text: &str, width: usize, max_lines: usize) -> Vec<WrappedLine> {
    wrap(&plain_render(text, PROMPT_COLOR), width, max_lines)
}

/// Wrap a plain prompt and its plain response, for row-height math. The line
/// count matches what [`wrap_turn_highlighted`] returns for the same texts.
pub fn wrap_turn_plain(
    prompt: &str,
    response: Option<&str>,
    width: usize,
    max_lines: usize,
) -> Vec<WrappedLine> {
    let prompt = plain_render(prompt, PROMPT_COLOR);
    let response = response.map(|r| plain_render(r, RESPONSE_COLOR));
    wrap_turn(&prompt, response.as_deref(), width, max_lines)
}

/// Wrap `text` with the Markdown styles, in the prompt colors.
pub fn wrap_highlighted(text: &str, width: usize, max_lines: usize) -> Vec<WrappedLine> {
    wrap(&highlight(text, PROMPT_COLOR), width, max_lines)
}

/// Wrap a prompt and its response with the Markdown styles. The prompt prose
/// is [`PROMPT_COLOR`], the response prose [`RESPONSE_COLOR`].
pub fn wrap_turn_highlighted(
    prompt: &str,
    response: Option<&str>,
    width: usize,
    max_lines: usize,
) -> Vec<WrappedLine> {
    let prompt = highlight(prompt, PROMPT_COLOR);
    let response = response.map(|r| highlight(r, RESPONSE_COLOR));
    wrap_turn(&prompt, response.as_deref().map(|r| r.as_slice()), width, max_lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[WrappedLine]) -> Vec<String> {
        lines.iter().map(WrappedLine::text).collect()
    }

    #[test]
    fn a_turn_caps_the_prompt_and_the_response_apart() {
        let prompt = "one two three four five six seven eight";
        let response = "alpha beta gamma delta epsilon zeta";
        let out = wrap_turn_plain(prompt, Some(response), 16, 2);
        // Two prompt lines (the second cut with an ellipsis), then two
        // response lines in the response gutter.
        assert_eq!(out.len(), 4);
        assert!(out[1].text().ends_with('…'));
        assert!(!out[0].response && !out[1].response);
        assert_eq!(out[1].indent, 0, "a prompt line has no hang indent");
        assert!(out[2].response && out[3].response);
        assert_eq!(out[2].indent, 0);
        assert_eq!(out[2].offset(), RESPONSE_GUTTER);
        assert_eq!(out[2].pieces[0].style, Style::default().fg(RESPONSE_COLOR));
        // Without a response the block is the prompt alone.
        assert_eq!(wrap_turn_plain(prompt, None, 16, 2).len(), 2);
        // The highlighted wrap gives the same line count.
        assert_eq!(wrap_turn_highlighted(prompt, Some(response), 16, 2).len(), 4);
    }

    #[test]
    fn a_response_line_wraps_inside_its_gutter() {
        // Width 10 leaves 6 columns after the gutter.
        let out = wrap_turn_plain("p", Some("abc def ghi"), 10, 5);
        assert_eq!(texts(&out), vec!["p", "abc", "def", "ghi"]);
        assert!(out[1..].iter().all(|l| l.offset() == RESPONSE_GUTTER));
    }

    #[test]
    fn wrap_fits_single_line() {
        let out = wrap_plain("hello world", 40, 5);
        assert_eq!(texts(&out), vec!["hello world"]);
        assert_eq!(out[0].indent, 0);
    }

    #[test]
    fn wrap_wraps_on_words() {
        // Width 11 holds "hello world"; " foo" does not fit, so it wraps.
        let out = wrap_plain("hello world foo", 11, 5);
        assert_eq!(texts(&out), vec!["hello world", "foo"]);
        assert_eq!(out[1].indent, 0, "a soft wrap has no extra indent");
    }

    #[test]
    fn wrap_caps_at_max_lines_with_ellipsis() {
        let out = wrap_plain("aa bb cc dd", 2, 2);
        assert_eq!(out.len(), 2);
        assert!(out.last().unwrap().text().ends_with('…'));
    }

    #[test]
    fn wrap_hard_splits_long_word() {
        let out = wrap_plain("abcdefgh", 3, 8);
        // Every line uses the full width.
        assert_eq!(texts(&out), vec!["abc", "def", "gh"]);
        assert!(out.iter().all(|l| l.indent == 0));
    }

    #[test]
    fn wrap_empty_yields_one_line() {
        let out = wrap_plain("", 10, 5);
        assert_eq!(texts(&out), vec![""]);
    }

    #[test]
    fn wrap_starts_a_line_at_a_real_linebreak() {
        let out = wrap_plain("first line\nsecond line", 40, 5);
        assert_eq!(texts(&out), vec!["first line", "second line"]);
    }

    #[test]
    fn wrap_drops_blank_lines() {
        let out = wrap_plain("a\n\n\nb", 40, 5);
        assert_eq!(texts(&out), vec!["a", "b"]);
    }

    #[test]
    fn wrap_mixes_linebreaks_and_wrapping() {
        let out = wrap_plain("hello world foo\nbye", 11, 5);
        assert_eq!(texts(&out), vec!["hello world", "foo", "bye"]);
        assert!(out.iter().all(|l| l.indent == 0));
    }

    #[test]
    fn every_prompt_line_starts_at_the_first_column() {
        let out = wrap_plain("one\ntwo\nthree", 40, 5);
        assert!(out.iter().all(|l| l.indent == 0 && !l.response));
    }

    #[test]
    fn a_soft_wrap_keeps_the_indent_of_its_own_line() {
        // The list item carries two spaces of its own. Its soft wrap has
        // the same indent and no more.
        let out = wrap_plain("head\n  - alpha bravo charlie", 16, 6);
        assert_eq!(texts(&out), vec!["head", "• alpha bravo", "charlie"]);
        assert_eq!(out[0].indent, 0);
        assert_eq!(out[1].indent, 2, "the item keeps its own indent");
        assert_eq!(out[2].indent, 2);
    }

    #[test]
    fn leading_indent_counts_a_tab_as_four_columns() {
        let out = wrap_plain("head\n\tindented", 40, 5);
        assert_eq!(out[1].indent, TAB_WIDTH);
    }

    #[test]
    fn a_lone_line_keeps_its_own_indent() {
        let out = wrap_plain("    deep", 40, 5);
        assert_eq!(out[0].indent, 4, "the very first line still shows its indent");
    }

    #[test]
    fn wrap_line_keeps_a_blank_response_line_in_the_gutter() {
        let out = wrap_line(&MdLine::Text(Vec::new()), 40, true, 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text(), "");
        assert!(out[0].response);
    }

    #[test]
    fn wrap_line_matches_wrap_for_one_source_line() {
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliett";
        let whole = wrap_plain(text, 20, 20);
        let hl = highlight(text, PROMPT_COLOR);
        let one = wrap_line(&hl[0], 20, false, 20);
        assert_eq!(
            whole.iter().map(|l| (l.text(), l.offset())).collect::<Vec<_>>(),
            one.iter().map(|l| (l.text(), l.offset())).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn highlight_renders_the_markdown() {
        let text = "Fix `main.rs`\n\n- **bold** item\n";
        let lines = highlight(text, PROMPT_COLOR);
        let joined: String = lines
            .iter()
            .map(|l| l.pieces().iter().map(|p| p.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(joined, "Fix main.rs\n\n• bold item");
    }

    #[test]
    fn a_table_aligns_its_columns() {
        let out = wrap_plain("| a | b |\n|---|---|\n| long cell | x |", 40, 5);
        assert_eq!(texts(&out), vec!["a         │ b", "──────────┼──", "long cell │ x"]);
    }

    #[test]
    fn a_wide_table_wraps_its_cells() {
        let md = "| key | value |\n|---|---|\n| k | alpha bravo charlie delta |";
        let out = wrap_plain(md, 16, 10);
        // 16 columns: the gap takes 3, `key` keeps 3, `value` gets 10.
        assert_eq!(
            texts(&out),
            vec!["key │ value", "────┼───────────", "k   │ alpha", "    │ bravo", "    │ charlie", "    │ delta"],
        );
        assert!(out.iter().all(|l| l.text().chars().count() <= 16));
    }

    #[test]
    fn a_code_block_counts_the_same_lines_plain_and_highlighted() {
        let text = "look:\n```rust\nfn main() { let x = 1; println!(\"{x}\"); }\n```";
        for width in [10usize, 25, 80] {
            assert_eq!(
                wrap_plain(text, width, 20).len(),
                wrap_highlighted(text, width, 20).len(),
                "line counts diverged at width {width}"
            );
        }
    }

    #[test]
    fn highlight_colors_code_apart_from_prose() {
        let lines = highlight("run `cargo test` now", PROMPT_COLOR);
        let styles: Vec<Color> = lines[0]
            .pieces()
            .iter()
            .filter_map(|p| p.style.fg)
            .collect();
        assert!(
            styles.iter().any(|c| *c != PROMPT_COLOR),
            "inline code should not use the prose color: {styles:?}"
        );
    }

    #[test]
    fn wrapped_line_count_matches_between_plain_and_highlighted() {
        let text = "Please refactor `src/app.rs` so the **agent** list rebuilds \
                    from the entries, and keep the tests green.";
        for width in [12usize, 20, 33, 60] {
            assert_eq!(
                wrap_plain(text, width, 20).len(),
                wrap_highlighted(text, width, 20).len(),
                "line counts diverged at width {width}"
            );
        }
    }
}
