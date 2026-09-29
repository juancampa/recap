//! Prompt text rendering: Markdown syntax highlighting plus a word-wrap that
//! keeps the highlight styles.
//!
//! A prompt is Markdown, so [`highlight`] runs it through syntect (the engine
//! that `bat` uses) with the Markdown syntax and a dark theme. The result is
//! one [`Piece`] list per source line. [`wrap`] then folds those lines into
//! panel-width lines and preserves each piece's style, because the History
//! panel must apply its own indent rules that no highlighter knows about.
//!
//! The wrap step is separate from the highlight step on purpose. `bat` (and
//! syntect) offer their own wrapping, but the History panel indents every line
//! after the first and marks soft wraps, so the layout has to stay here.

use ratatui::style::{Color, Modifier, Style};
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Color of prompt prose. The highlighter maps the theme's own default
/// foreground onto this color, so plain text stays white and only Markdown
/// constructs (code, headings, emphasis) take a theme color.
pub const PROMPT_COLOR: Color = Color::White;

/// Color of response prose: the light blue that tells the agent's final
/// response apart from the prompt above it.
pub const RESPONSE_COLOR: Color = Color::Rgb(135, 206, 250);

/// Indent given to every prompt line after the very first one, so the whole
/// prompt reads as one block that hangs under its first line.
pub const HANG_INDENT: usize = 2;

/// Extra indent a soft-wrapped line takes beyond the line it continues. The
/// wrapped line therefore nests under its own source line, list item indent
/// included.
pub const WRAP_INDENT: usize = 2;

/// Glyph drawn in the last indent column of a soft-wrapped line, so a soft
/// wrap stays distinct from a real linebreak in the prompt. The same arrow
/// joins the project names in a row's project chain.
pub const WRAP_MARKER: &str = "→";

/// Color of [`WRAP_MARKER`]. Faint gray, quieter than the prompt text.
pub const WRAP_MARKER_COLOR: Color = Color::Rgb(100, 100, 100);

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
    /// Columns of indent the renderer draws in front of `pieces`.
    pub indent: usize,
    /// True when a soft wrap produced this line. The renderer then draws
    /// [`WRAP_MARKER`] in the last indent column. False for the first line
    /// and for a line that follows a real linebreak.
    pub continuation: bool,
}

impl WrappedLine {
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

/// The theme's own default foreground. Text that carries this color is plain
/// prose, so the mapper replaces it with [`PROMPT_COLOR`].
static THEME_FG: LazyLock<Option<(u8, u8, u8)>> = LazyLock::new(|| {
    THEME
        .settings
        .foreground
        .map(|c| (c.r, c.g, c.b))
});

fn to_ratatui(style: syntect::highlighting::Style, prose: Color) -> Style {
    let fg = style.foreground;
    let color = if *THEME_FG == Some((fg.r, fg.g, fg.b)) {
        prose
    } else {
        Color::Rgb(fg.r, fg.g, fg.b)
    };
    let mut out = Style::default().fg(color);
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
    static CACHE: RefCell<HashMap<u64, Rc<Vec<Vec<Piece>>>>> =
        RefCell::new(HashMap::new());
}

fn hash_of(text: &str, prose: Color) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    prose.hash(&mut h);
    h.finish()
}

/// Highlight `text` as Markdown, with `prose` as the color of plain text.
/// Returns one piece list per source line, with the trailing newline removed.
/// The result is cached by content hash and prose color.
pub fn highlight(text: &str, prose: Color) -> Rc<Vec<Vec<Piece>>> {
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

fn highlight_uncached(text: &str, prose: Color) -> Vec<Vec<Piece>> {
    let syntax = SYNTAXES
        .find_syntax_by_extension("md")
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text());
    let mut hl = HighlightLines::new(syntax, &THEME);
    let base = Style::default().fg(prose);
    let mut out: Vec<Vec<Piece>> = Vec::new();
    for line in LinesWithEndings::from(text) {
        let Ok(ranges) = hl.highlight_line(line, &SYNTAXES) else {
            // A highlighter failure must not lose the prompt: fall back to
            // plain prose for this line.
            out.push(vec![Piece {
                text: line.trim_end_matches('\n').to_string(),
                style: base,
            }]);
            continue;
        };
        let pieces = ranges
            .into_iter()
            .map(|(s, t)| Piece {
                text: t.trim_end_matches('\n').to_string(),
                style: to_ratatui(s, prose),
            })
            .filter(|p| !p.text.is_empty())
            .collect();
        out.push(pieces);
    }
    if out.is_empty() {
        out.push(Vec::new());
    }
    out
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

/// Append the rendered lines of one source line to `out`.
///
/// The line's own indent is [`HANG_INDENT`] (unless it is the prompt's first
/// line) plus its leading whitespace. A soft wrap of it takes
/// [`WRAP_INDENT`] more. Whitespace inside the line collapses to single
/// spaces, and a word longer than the line splits at the column edge.
///
/// Returns false when `out` grew past `max_lines`, so the caller stops: one
/// line more than we keep is enough to know the text overflowed, and it
/// bounds the work on a huge prompt.
fn wrap_into(
    out: &mut Vec<WrappedLine>,
    src: &[Piece],
    width: usize,
    hang: bool,
    max_lines: usize,
) -> bool {
    let base = (if hang { HANG_INDENT } else { 0 }) + leading_indent(src, width);
    let cont_indent = base + WRAP_INDENT;
    let words = words_of(src);
    if words.is_empty() {
        return true;
    }

    let line_width = |soft: bool| {
        let indent = if soft { cont_indent } else { base };
        width.saturating_sub(indent).max(1)
    };
    let mut cur: Vec<Piece> = Vec::new();
    let mut cur_len = 0usize;
    // Whether the line under construction is a soft wrap of this source line.
    let mut soft = false;

    for word in words {
        let mut rest = word;
        loop {
            let lw = line_width(soft);
            let wlen = rest.len();
            if cur_len == 0 {
                if wlen <= lw {
                    cur = rest.pieces;
                    cur_len = wlen;
                    break;
                }
                // The word is longer than a whole line: cut it.
                let (head, tail) = rest.split_at_chars(lw);
                out.push(WrappedLine {
                    pieces: head,
                    indent: if soft { cont_indent } else { base },
                    continuation: soft,
                });
                if out.len() > max_lines {
                    return false;
                }
                rest = tail;
                soft = true;
                continue;
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
                out.push(WrappedLine {
                    pieces: std::mem::take(&mut cur),
                    indent: if soft { cont_indent } else { base },
                    continuation: soft,
                });
                cur_len = 0;
                soft = true;
                if out.len() > max_lines {
                    return false;
                }
                // Place this word on the fresh line.
                continue;
            }
        }
    }
    if cur_len > 0 {
        out.push(WrappedLine {
            pieces: cur,
            indent: if soft { cont_indent } else { base },
            continuation: soft,
        });
        if out.len() > max_lines {
            return false;
        }
    }
    true
}

/// Cap `out` at `max_lines` lines, marking the cut with a trailing `…`.
fn cap(out: &mut Vec<WrappedLine>, width: usize, max_lines: usize) {
    if out.len() <= max_lines {
        return;
    }
    out.truncate(max_lines);
    if let Some(last) = out.last_mut() {
        let lw = width.saturating_sub(last.indent).max(1);
        truncate_with_ellipsis(&mut last.pieces, lw);
    }
}

/// Word-wrap the highlighted source `lines` of one prompt into at most
/// `max_lines` lines of `width` columns.
///
/// The first rendered line starts at column 0. Every later line hangs by
/// [`HANG_INDENT`], and each carries its own source indent on top; see
/// [`wrap_into`]. Blank source lines drop out, because the History panel has
/// only a few lines per row to spend. The function always returns between 1
/// and `max_lines` lines.
pub fn wrap(lines: &[Vec<Piece>], width: usize, max_lines: usize) -> Vec<WrappedLine> {
    let width = width.max(1);
    let max_lines = max_lines.max(1);
    let mut out: Vec<WrappedLine> = Vec::new();
    for src in lines {
        let hang = !out.is_empty();
        if !wrap_into(&mut out, src, width, hang, max_lines) {
            break;
        }
    }
    if out.is_empty() {
        out.push(WrappedLine {
            pieces: Vec::new(),
            indent: 0,
            continuation: false,
        });
    }
    cap(&mut out, width, max_lines);
    out
}

/// Wrap one source line on its own. The Diff panel scrolls by source line, so
/// it formats one line per step and passes `hang: false` only for the
/// prompt's first line. A blank line yields one blank rendered line, so the
/// paragraph break stays visible where there is room to show it.
pub fn wrap_line(
    src: &[Piece],
    width: usize,
    hang: bool,
    max_lines: usize,
) -> Vec<WrappedLine> {
    let width = width.max(1);
    let max_lines = max_lines.max(1);
    let mut out: Vec<WrappedLine> = Vec::new();
    wrap_into(&mut out, src, width, hang, max_lines);
    if out.is_empty() {
        out.push(WrappedLine {
            pieces: Vec::new(),
            indent: if hang { HANG_INDENT } else { 0 },
            continuation: false,
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
/// produced: every line hangs by [`HANG_INDENT`], the way a prompt line after
/// the first does. The block gets its own `max_lines` cap, so a long prompt
/// does not push the response out of the row.
fn wrap_response(lines: &[Vec<Piece>], width: usize, max_lines: usize) -> Vec<WrappedLine> {
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
    prompt: &[Vec<Piece>],
    response: Option<&[Vec<Piece>]>,
    width: usize,
    max_lines: usize,
) -> Vec<WrappedLine> {
    let mut out = wrap(prompt, width, max_lines);
    if let Some(response) = response {
        out.extend(wrap_response(response, width, max_lines));
    }
    out
}

/// Split `text` into one single-piece line per source line, all in `style`.
fn plain_lines(text: &str, style: Style) -> Vec<Vec<Piece>> {
    text.split('\n')
        .map(|l| {
            vec![Piece {
                text: l.to_string(),
                style,
            }]
        })
        .collect()
}

/// Wrap plain `text` with one uniform style. The History panel uses this for
/// row-height math: highlighting never changes the characters, so the line
/// count matches what [`wrap`] returns for the highlighted text.
pub fn wrap_plain(text: &str, width: usize, max_lines: usize) -> Vec<WrappedLine> {
    let lines = plain_lines(text, Style::default().fg(PROMPT_COLOR));
    wrap(&lines, width, max_lines)
}

/// Wrap a plain prompt and its plain response, for row-height math. The line
/// count matches what [`wrap_turn_highlighted`] returns for the same texts.
pub fn wrap_turn_plain(
    prompt: &str,
    response: Option<&str>,
    width: usize,
    max_lines: usize,
) -> Vec<WrappedLine> {
    let prompt = plain_lines(prompt, Style::default().fg(PROMPT_COLOR));
    let response = response.map(|r| plain_lines(r, Style::default().fg(RESPONSE_COLOR)));
    wrap_turn(&prompt, response.as_deref(), width, max_lines)
}

/// Wrap `text` with Markdown highlighting applied, in the prompt colors.
pub fn wrap_highlighted(text: &str, width: usize, max_lines: usize) -> Vec<WrappedLine> {
    let lines = highlight(text, PROMPT_COLOR);
    wrap(&lines, width, max_lines)
}

/// Wrap a prompt and its response with Markdown highlighting applied. The
/// prompt prose is [`PROMPT_COLOR`], the response prose [`RESPONSE_COLOR`].
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
        let out = wrap_turn_plain(prompt, Some(response), 12, 2);
        // Two prompt lines (the second cut with an ellipsis), then two
        // response lines that hang like a continuation.
        assert_eq!(out.len(), 4);
        assert!(out[1].text().ends_with('…'));
        assert_eq!(out[0].indent, 0);
        assert_eq!(out[2].indent, HANG_INDENT);
        assert!(!out[2].continuation, "the response starts a fresh line");
        assert_eq!(out[2].pieces[0].style, Style::default().fg(RESPONSE_COLOR));
        // Without a response the block is the prompt alone.
        assert_eq!(wrap_turn_plain(prompt, None, 12, 2).len(), 2);
        // The highlighted wrap gives the same line count.
        assert_eq!(wrap_turn_highlighted(prompt, Some(response), 12, 2).len(), 4);
    }

    #[test]
    fn wrap_fits_single_line() {
        let out = wrap_plain("hello world", 40, 5);
        assert_eq!(texts(&out), vec!["hello world"]);
        assert!(!out[0].continuation);
    }

    #[test]
    fn wrap_wraps_on_words() {
        // Width 11 holds "hello world"; " foo" does not fit, so it wraps.
        let out = wrap_plain("hello world foo", 11, 5);
        assert_eq!(texts(&out), vec!["hello world", "foo"]);
        assert!(!out[0].continuation);
        assert!(out[1].continuation);
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
        // The first line uses the full width (3). Every later line loses
        // WRAP_INDENT columns, which leaves 1 column for the rest.
        assert_eq!(texts(&out), vec!["abc", "d", "e", "f", "g", "h"]);
        assert!(!out[0].continuation);
        assert!(out[1..].iter().all(|l| l.continuation));
    }

    #[test]
    fn wrap_empty_yields_one_line() {
        let out = wrap_plain("", 10, 5);
        assert_eq!(texts(&out), vec![""]);
        assert!(!out[0].continuation);
    }

    #[test]
    fn wrap_marks_real_linebreaks_as_hard() {
        let out = wrap_plain("first line\nsecond line", 40, 5);
        assert_eq!(texts(&out), vec!["first line", "second line"]);
        assert!(!out[0].continuation);
        assert!(!out[1].continuation, "a line after \\n is not a soft wrap");
    }

    #[test]
    fn wrap_narrows_every_line_after_the_first() {
        // Width 6: the first line takes 6 columns, later lines take 4.
        let out = wrap_plain("aaaaaa\nbbbbbb", 6, 5);
        assert_eq!(texts(&out), vec!["aaaaaa", "bbbb", "bb"]);
        assert!(!out[1].continuation, "the hard break keeps its own line");
        assert!(out[2].continuation, "the overflow of that line is a soft wrap");
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
        assert!(!out[0].continuation);
        assert!(out[1].continuation, "soft wrap of the first line");
        assert!(!out[2].continuation, "real linebreak");
    }

    #[test]
    fn indents_hang_after_the_first_line() {
        let out = wrap_plain("one\ntwo\nthree", 40, 5);
        assert_eq!(out[0].indent, 0, "the first line starts at column 0");
        assert_eq!(out[1].indent, HANG_INDENT);
        assert_eq!(out[2].indent, HANG_INDENT);
        assert!(out.iter().all(|l| !l.continuation));
    }

    #[test]
    fn a_soft_wrap_nests_under_its_own_line() {
        // The list item hangs by HANG_INDENT and carries two spaces of its
        // own, so its soft wrap indents WRAP_INDENT past that.
        let out = wrap_plain("head\n  - alpha bravo charlie", 18, 6);
        assert_eq!(texts(&out), vec!["head", "- alpha bravo", "charlie"]);
        assert_eq!(out[0].indent, 0);
        assert_eq!(out[1].indent, HANG_INDENT + 2, "the item keeps its own indent");
        assert!(!out[1].continuation);
        assert_eq!(out[2].indent, HANG_INDENT + 2 + WRAP_INDENT);
        assert!(out[2].continuation);
    }

    #[test]
    fn leading_indent_counts_a_tab_as_four_columns() {
        let out = wrap_plain("head\n\tindented", 40, 5);
        assert_eq!(out[1].indent, HANG_INDENT + TAB_WIDTH);
    }

    #[test]
    fn a_lone_line_keeps_its_own_indent() {
        let out = wrap_plain("    deep", 40, 5);
        assert_eq!(out[0].indent, 4, "the very first line still shows its indent");
    }

    #[test]
    fn wrap_line_keeps_a_blank_line() {
        let out = wrap_line(&[], 40, true, 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text(), "");
        assert_eq!(out[0].indent, HANG_INDENT);
    }

    #[test]
    fn wrap_line_matches_wrap_for_one_source_line() {
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliett";
        let whole = wrap_plain(text, 20, 20);
        let hl = highlight(text, PROMPT_COLOR);
        let one = wrap_line(&hl[0], 20, false, 20);
        assert_eq!(
            whole.iter().map(|l| (l.text(), l.indent, l.continuation)).collect::<Vec<_>>(),
            one.iter().map(|l| (l.text(), l.indent, l.continuation)).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn highlight_keeps_every_character() {
        let text = "Fix `main.rs`\n\n- **bold** item\n";
        let lines = highlight(text, PROMPT_COLOR);
        let joined: String = lines
            .iter()
            .map(|l| l.iter().map(|p| p.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(joined.trim_end(), text.trim_end());
    }

    #[test]
    fn highlight_colors_code_apart_from_prose() {
        let lines = highlight("run `cargo test` now", PROMPT_COLOR);
        let styles: Vec<Color> = lines[0]
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
