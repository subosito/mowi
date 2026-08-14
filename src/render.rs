use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::ops::Range;

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::theme::Theme;

/// One slice of an assistant message: markdown prose or a unified-diff body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Md(String),
    Diff(String),
}

/// True when `text` is a *bare* unified diff: a `@@` hunk header outside any
/// fenced code block, plus at least one `+`/`-` hunk line. Fenced ` ```diff `
/// bodies are extracted by [`split_markdown_and_diffs`]; a `@@` sitting inside
/// a sentence is not a card.
pub fn is_unified_diff(text: &str) -> bool {
    let mut in_fence = false;
    let mut has_hunk = false;
    let mut has_body = false;
    for line in text.lines() {
        if fence_info(line).is_some() {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if line.starts_with("@@") {
            has_hunk = true;
        } else if is_hunk_body(line) {
            has_body = true;
        }
    }
    has_hunk && has_body
}

/// Split assistant text into markdown and unified-diff segments.
///
/// A fenced ` ```diff ` / ` ```udiff ` block becomes a [`Segment::Diff`];
/// surrounding prose stays markdown. With no such fence, a bare unified diff
/// (see [`is_unified_diff`]) is one Diff; everything else is one Md.
pub fn split_markdown_and_diffs(text: &str) -> Vec<Segment> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<Segment> = Vec::new();
    let mut md: Vec<&str> = Vec::new();
    let mut in_code = false;
    let mut i = 0;
    while i < lines.len() {
        if let Some(info) = fence_info(lines[i]) {
            if !in_code
                && is_diff_fence_lang(info)
                && let Some(rel) = lines[i + 1..]
                    .iter()
                    .position(|line| fence_info(line).is_some())
            {
                flush_md(&mut out, &mut md);
                let body = lines[i + 1..i + 1 + rel].join("\n");
                if !body.trim().is_empty() {
                    out.push(Segment::Diff(body));
                }
                i += rel + 2;
                continue;
            }
            in_code = !in_code;
        }
        md.push(lines[i]);
        i += 1;
    }
    flush_md(&mut out, &mut md);

    if out.iter().any(|seg| matches!(seg, Segment::Diff(_))) {
        return out;
    }
    if is_unified_diff(text) {
        vec![Segment::Diff(text.to_string())]
    } else if out.is_empty() {
        vec![Segment::Md(text.to_string())]
    } else {
        out
    }
}

fn flush_md(out: &mut Vec<Segment>, md: &mut Vec<&str>) {
    if md.is_empty() {
        return;
    }
    let body = md.join("\n");
    md.clear();
    let trimmed = body.trim_matches('\n');
    if !trimmed.is_empty() {
        out.push(Segment::Md(trimmed.to_string()));
    }
}

/// Info string after an opening/closing ` ``` ` fence, if this line is one.
fn fence_info(line: &str) -> Option<&str> {
    let rest = line.trim_end().trim_start();
    rest.strip_prefix("```").map(str::trim)
}

fn is_diff_fence_lang(info: &str) -> bool {
    let lang = info
        .split(|ch: char| ch.is_whitespace() || ch == '{' || ch == '[')
        .next()
        .unwrap_or("");
    matches!(lang, "diff" | "udiff")
}

fn is_hunk_body(line: &str) -> bool {
    (line.starts_with('+') && !line.starts_with("+++"))
        || (line.starts_with('-') && !line.starts_with("---"))
}

/// Inline emphasis state, rebuilt into a `Style` per span.
#[derive(Default, Clone, Copy)]
struct Inline {
    strong: bool,
    emphasis: bool,
    strike: bool,
}

impl Inline {
    fn style(self, theme: Theme) -> Style {
        let mut style = theme.text();
        if self.strike {
            style = theme.md_strike();
        }
        if self.strong {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.emphasis {
            style = style.add_modifier(Modifier::ITALIC);
        }
        style
    }
}

/// One open list level: `None` = bullet, `Some(n)` = next ordinal.
type ListLevel = Option<u64>;

/// Render markdown into styled lines.
///
/// Block structure (quote bars, list indents) becomes a *prefix* on every
/// emitted line, so wrapped continuations stay visually inside their block.
/// Blank lines are pushed through `push_blank`, which collapses runs — the
/// document never shows two blanks in a row.
pub fn markdown_lines(text: &str, theme: Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut inline = Inline::default();
    let mut lists: Vec<ListLevel> = Vec::new();
    let mut quote_depth = 0usize;
    let mut pending_marker: Option<Span<'static>> = None;
    let mut link: Option<(usize, String)> = None;
    let mut code: Option<String> = None;
    let mut table: Option<TableAcc> = None;
    let mut cell = String::new();

    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;

    for event in Parser::new_ext(text, options) {
        match event {
            // ---- blocks ------------------------------------------------
            Event::Start(Tag::Heading { level, .. }) => {
                flush(
                    &mut out,
                    &mut spans,
                    &mut pending_marker,
                    quote_depth,
                    &lists,
                );
                push_blank(&mut out);
                let depth = level as u8;
                spans.push(Span::styled(
                    format!("{} ", "#".repeat(depth as usize)),
                    theme.md_heading(depth),
                ));
                inline = Inline {
                    strong: true,
                    ..Inline::default()
                };
            }
            Event::End(TagEnd::Heading(level)) => {
                // Re-style the heading body to match its level.
                let depth = level as u8;
                for span in &mut spans {
                    span.style = theme.md_heading(depth);
                }
                flush(
                    &mut out,
                    &mut spans,
                    &mut pending_marker,
                    quote_depth,
                    &lists,
                );
                inline = Inline::default();
            }

            Event::Start(Tag::Paragraph) => {
                if pending_marker.is_none() {
                    push_blank(&mut out);
                }
            }
            Event::End(TagEnd::Paragraph) => {
                flush(
                    &mut out,
                    &mut spans,
                    &mut pending_marker,
                    quote_depth,
                    &lists,
                );
            }

            Event::Start(Tag::BlockQuote(_)) => {
                push_blank(&mut out);
                quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush(
                    &mut out,
                    &mut spans,
                    &mut pending_marker,
                    quote_depth,
                    &lists,
                );
                quote_depth = quote_depth.saturating_sub(1);
            }

            Event::Start(Tag::List(first)) => {
                // A nested list opens inside its parent item — flush the
                // parent's text first or the two weld into one line.
                flush(
                    &mut out,
                    &mut spans,
                    &mut pending_marker,
                    quote_depth,
                    &lists,
                );
                if lists.is_empty() {
                    push_blank(&mut out);
                }
                lists.push(first);
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                flush(
                    &mut out,
                    &mut spans,
                    &mut pending_marker,
                    quote_depth,
                    &lists,
                );
                let depth = lists.len().saturating_sub(1);
                let marker = match lists.last_mut() {
                    Some(Some(n)) => {
                        let text = format!("{n}. ");
                        *n += 1;
                        text
                    }
                    _ => {
                        let glyph = if depth == 0 { '•' } else { '◦' };
                        format!("{glyph} ")
                    }
                };
                pending_marker = Some(Span::styled(marker, theme.md_bullet()));
            }
            Event::End(TagEnd::Item) => {
                flush(
                    &mut out,
                    &mut spans,
                    &mut pending_marker,
                    quote_depth,
                    &lists,
                );
                pending_marker = None;
            }

            Event::Rule => {
                push_blank(&mut out);
                out.push(Line::styled("─".repeat(RULE_COLS), theme.md_rule()));
                push_blank(&mut out);
            }

            // ---- code --------------------------------------------------
            Event::Start(Tag::CodeBlock(kind)) => {
                flush(
                    &mut out,
                    &mut spans,
                    &mut pending_marker,
                    quote_depth,
                    &lists,
                );
                push_blank(&mut out);
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => lang.to_string(),
                    _ => String::new(),
                };
                out.push(Line::from(vec![
                    Span::styled(CODE_GUTTER.to_string(), theme.chrome()),
                    Span::styled(
                        if lang.is_empty() {
                            " code".to_string()
                        } else {
                            format!(" {lang}")
                        },
                        theme.md_code_lang(),
                    ),
                ]));
                code = Some(lang);
            }
            Event::End(TagEnd::CodeBlock) => {
                code = None;
                push_blank(&mut out);
            }

            // ---- tables ------------------------------------------------
            Event::Start(Tag::Table(_)) => {
                flush(
                    &mut out,
                    &mut spans,
                    &mut pending_marker,
                    quote_depth,
                    &lists,
                );
                push_blank(&mut out);
                table = Some(TableAcc::default());
            }
            Event::End(TagEnd::Table) => {
                if let Some(acc) = table.take() {
                    out.extend(acc.render(theme));
                }
                push_blank(&mut out);
            }
            Event::Start(Tag::TableHead) => {
                if let Some(acc) = table.as_mut() {
                    acc.in_head = true;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(acc) = table.as_mut() {
                    acc.finish_row();
                    acc.in_head = false;
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(acc) = table.as_mut() {
                    acc.finish_row();
                }
            }
            Event::Start(Tag::TableCell) => cell.clear(),
            Event::End(TagEnd::TableCell) => {
                if let Some(acc) = table.as_mut() {
                    acc.push_cell(std::mem::take(&mut cell));
                }
            }

            // ---- inline ------------------------------------------------
            Event::Start(Tag::Strong) => inline.strong = true,
            Event::End(TagEnd::Strong) => inline.strong = false,
            Event::Start(Tag::Emphasis) => inline.emphasis = true,
            Event::End(TagEnd::Emphasis) => inline.emphasis = false,
            Event::Start(Tag::Strikethrough) => inline.strike = true,
            Event::End(TagEnd::Strikethrough) => inline.strike = false,

            Event::Start(Tag::Link { dest_url, .. }) => {
                link = Some((spans.len(), dest_url.to_string()));
            }
            Event::End(TagEnd::Link) => {
                if let Some((start, url)) = link.take() {
                    let shown: String = spans[start..].iter().map(|s| s.content.as_ref()).collect();
                    for span in &mut spans[start..] {
                        span.style = theme.md_link();
                    }
                    if shown.trim() != url.trim() && !url.is_empty() {
                        spans.push(Span::styled(format!(" ({url})"), theme.note()));
                    }
                }
            }

            Event::Code(text) => {
                spans.push(Span::styled(text.to_string(), theme.md_code()));
            }

            Event::Text(text) => {
                if table.is_some() {
                    cell.push_str(&text);
                } else if code.is_some() {
                    // Code bodies are literal: one output line per source line,
                    // each on the code ground behind a gutter bar.
                    for body in text.lines() {
                        out.push(Line::from(vec![
                            Span::styled(CODE_GUTTER.to_string(), theme.chrome()),
                            Span::styled(format!(" {body}"), theme.md_code_block()),
                        ]));
                    }
                } else {
                    spans.push(Span::styled(text.to_string(), inline.style(theme)));
                }
            }

            Event::SoftBreak | Event::HardBreak => {
                if table.is_some() {
                    cell.push(' ');
                } else {
                    flush(
                        &mut out,
                        &mut spans,
                        &mut pending_marker,
                        quote_depth,
                        &lists,
                    );
                }
            }

            _ => {}
        }
    }

    flush(
        &mut out,
        &mut spans,
        &mut pending_marker,
        quote_depth,
        &lists,
    );
    while out.last().is_some_and(is_blank) {
        out.pop();
    }
    while out.first().is_some_and(is_blank) {
        out.remove(0);
    }
    if out.is_empty() {
        out.push(Line::raw(""));
    }
    out
}

/// Columns used by a thematic break.
const RULE_COLS: usize = 40;
/// Left bar drawn beside fenced code bodies.
const CODE_GUTTER: &str = "▏";

fn is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

/// Append a blank line, collapsing runs and never leading the document.
fn push_blank(out: &mut Vec<Line<'static>>) {
    if out.is_empty() || out.last().is_some_and(is_blank) {
        return;
    }
    out.push(Line::raw(""));
}

/// Emit the buffered inline spans as one line, prefixed by the open block
/// structure (quote bars, then list indent, then any pending list marker).
fn flush(
    out: &mut Vec<Line<'static>>,
    spans: &mut Vec<Span<'static>>,
    pending_marker: &mut Option<Span<'static>>,
    quote_depth: usize,
    lists: &[ListLevel],
) {
    if spans.is_empty() {
        return;
    }
    let mut line: Vec<Span<'static>> = Vec::new();
    for _ in 0..quote_depth {
        line.push(Span::raw("▏ "));
    }
    let indent = lists.len().saturating_sub(1);
    if indent > 0 {
        line.push(Span::raw("  ".repeat(indent)));
    }
    match pending_marker.take() {
        Some(marker) => line.push(marker),
        // Continuation inside a list item aligns past the marker.
        None if !lists.is_empty() => line.push(Span::raw("  ")),
        None => {}
    }
    line.append(spans);
    out.push(Line::from(line));
}

/// Accumulates table cells until the table closes and widths are known.
#[derive(Default)]
struct TableAcc {
    in_head: bool,
    head: Vec<String>,
    rows: Vec<Vec<String>>,
    row: Vec<String>,
}

impl TableAcc {
    fn push_cell(&mut self, text: String) {
        self.row.push(text.trim().to_string());
    }

    fn finish_row(&mut self) {
        let row = std::mem::take(&mut self.row);
        if row.is_empty() {
            return;
        }
        if self.in_head {
            self.head = row;
        } else {
            self.rows.push(row);
        }
    }

    /// Pad every column to its widest cell so the grid lines up.
    fn render(&self, theme: Theme) -> Vec<Line<'static>> {
        let cols = self
            .head
            .len()
            .max(self.rows.iter().map(Vec::len).max().unwrap_or(0));
        if cols == 0 {
            return Vec::new();
        }
        let mut widths = vec![0usize; cols];
        for (i, w) in widths.iter_mut().enumerate() {
            *w = self.head.get(i).map_or(0, |c| c.chars().count());
            for row in &self.rows {
                *w = (*w).max(row.get(i).map_or(0, |c| c.chars().count()));
            }
        }
        let pad = |cells: &[String]| -> String {
            (0..cols)
                .map(|i| {
                    let text = cells.get(i).map(String::as_str).unwrap_or("");
                    format!("{text:<width$}", width = widths[i])
                })
                .collect::<Vec<_>>()
                .join("  ")
        };
        let mut out = Vec::new();
        if !self.head.is_empty() {
            out.push(Line::styled(
                pad(&self.head).trim_end().to_string(),
                theme.md_table_head(),
            ));
            out.push(Line::styled(
                widths
                    .iter()
                    .map(|w| "─".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("  "),
                theme.chrome(),
            ));
        }
        for row in &self.rows {
            out.push(Line::styled(pad(row).trim_end().to_string(), theme.text()));
        }
        out
    }
}

/// File path of a unified diff, from `+++` / `---` / `diff --git` when present.
pub fn diff_file(text: &str) -> Option<String> {
    let mut from_plus = None;
    let mut from_minus = None;
    let mut from_git = None;
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(path) = line.strip_prefix("+++ ") {
            from_plus = from_plus.or_else(|| clean_diff_path(path));
        } else if let Some(path) = line.strip_prefix("--- ") {
            from_minus = from_minus.or_else(|| clean_diff_path(path));
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            from_git = from_git.or_else(|| git_diff_path(rest));
        }
    }
    from_plus.or(from_minus).or(from_git)
}

/// Card title: a path from the hunk headers, or `hunk` when the body is file-less.
pub fn diff_title(text: &str) -> String {
    diff_file(text).unwrap_or_else(|| "hunk".to_string())
}

fn clean_diff_path(path: &str) -> Option<String> {
    let path = path.split('\t').next().unwrap_or(path).trim();
    let path = path
        .strip_prefix("b/")
        .or_else(|| path.strip_prefix("a/"))
        .unwrap_or(path);
    if path.is_empty() || path == "/dev/null" {
        None
    } else {
        Some(path.to_string())
    }
}

fn git_diff_path(rest: &str) -> Option<String> {
    let mut parts = rest.split_whitespace();
    let a = parts.next().unwrap_or("");
    let b = parts.next().unwrap_or(a);
    clean_diff_path(b).or_else(|| clean_diff_path(a))
}

/// Render a unified diff as flashdiff-style full-width bands.
///
/// `width` is the transcript pane width: add/del rows are padded so the wash
/// is a rectangle rather than a ragged stripe. Hunk and file headers stay
/// muted meta; context rows carry no wash. When a `-` row is followed by a
/// `+` row, the changed span of each is inverted as a word chip.
pub fn diff_lines(text: &str, theme: Theme, width: u16) -> Vec<Line<'static>> {
    let rows: Vec<String> = text.lines().map(|row| expand_tabs(row, 0)).collect();
    let mut out = Vec::with_capacity(rows.len());
    let mut index = 0;
    while index < rows.len() {
        let row = rows[index].as_str();
        if row.is_empty() {
            // Blank source lines are not painted: they show up as a hole after
            // `@@` and break the card. A real empty file line is ` ` (space).
            index += 1;
            continue;
        }
        if row.starts_with("+++") || row.starts_with("---") || row.starts_with("@@") {
            out.push(Line::from(Span::styled(row.to_string(), theme.diff_meta())));
            index += 1;
            continue;
        }
        let del_style = (theme.del(), theme.del_sign(), theme.del_chip());
        let add_style = (theme.add(), theme.add_sign(), theme.add_chip());
        match (row.strip_prefix('-'), row.strip_prefix('+')) {
            (Some(old_body), None) => {
                // Pair with the next row to word-diff a single-line edit.
                let new_body = rows
                    .get(index + 1)
                    .map(|s| s.as_str())
                    .filter(|next| !next.starts_with("+++"))
                    .and_then(|next| next.strip_prefix('+'));
                match new_body {
                    Some(new_body) => {
                        let (old_chip, new_chip) = match changed_span(old_body, new_body) {
                            Some((old_range, new_range)) => (Some(old_range), Some(new_range)),
                            None => (None, None),
                        };
                        out.push(band('−', old_body, old_chip, del_style, width));
                        out.push(band('+', new_body, new_chip, add_style, width));
                        index += 2;
                    }
                    None => {
                        out.push(band('−', old_body, None, del_style, width));
                        index += 1;
                    }
                }
            }
            (None, Some(new_body)) => {
                out.push(band('+', new_body, None, add_style, width));
                index += 1;
            }
            _ => {
                out.push(Line::from(Span::styled(row.to_string(), theme.context())));
                index += 1;
            }
        }
    }
    out
}

/// Replace tabs with spaces at tabstops of 8, starting at `col`.
fn expand_tabs(text: &str, mut col: usize) -> String {
    const TAB: usize = 8;
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '\t' {
            let n = TAB - (col % TAB);
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    out
}

fn clip_cols(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// One washed diff row: sign column, body, and padding out to `width`.
///
/// `styles` is `(band, sign, chip)`. `chip` marks the changed byte range of a
/// word-level edit; `None` renders a plain band.
fn band(
    sign: char,
    body: &str,
    chip: Option<Range<usize>>,
    styles: (Style, Style, Style),
    width: u16,
) -> Line<'static> {
    let (band, sign_style, chip_style) = styles;
    let max_body = (width as usize).saturating_sub(1);
    let body = clip_cols(body, max_body);
    let chip = chip.filter(|range| {
        range.start <= body.len() && range.end <= body.len() && range.start <= range.end
    });
    let mut spans = vec![Span::styled(sign.to_string(), sign_style)];
    match chip
        .filter(|range| body.is_char_boundary(range.start) && body.is_char_boundary(range.end))
    {
        Some(range) => {
            spans.push(Span::styled(body[..range.start].to_string(), band));
            spans.push(Span::styled(body[range.clone()].to_string(), chip_style));
            spans.push(Span::styled(body[range.end..].to_string(), band));
        }
        None => spans.push(Span::styled(body.to_string(), band)),
    }
    let painted: usize = spans.iter().map(Span::width).sum();
    let pad = (width as usize).saturating_sub(painted);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), band));
    }
    Line::from(spans).style(band)
}

/// Byte range of the changed span in each row of a `-`/`+` pair.
///
/// Shared prefix and suffix are peeled off; what remains in the middle is the
/// word-level edit. Offsets land on char boundaries so slicing is safe for
/// non-ASCII source.
///
/// Returns `None` when the rows barely overlap: a near-full-line chip is
/// noisier than a plain band, and an incidental shared letter (`alpha` /
/// `omega`) is not a word-level edit.
fn changed_span(old: &str, new: &str) -> Option<(Range<usize>, Range<usize>)> {
    let head = old
        .char_indices()
        .zip(new.char_indices())
        .take_while(|((_, a), (_, b))| a == b)
        .map(|((index, ch), _)| index + ch.len_utf8())
        .last()
        .unwrap_or(0);
    let tail = old[head..]
        .char_indices()
        .rev()
        .zip(new[head..].char_indices().rev())
        .take_while(|((_, a), (_, b))| a == b)
        .map(|((index, _), _)| old[head..].len() - index)
        .last()
        .unwrap_or(0);
    let shortest = old.len().min(new.len());
    if shortest == 0 || head + tail < shortest.div_ceil(4).max(2) {
        return None;
    }
    let old_end = old.len().checked_sub(tail)?;
    let new_end = new.len().checked_sub(tail)?;
    if head >= old_end || head >= new_end {
        return None;
    }
    Some((head..old_end, head..new_end))
}

#[cfg(test)]
mod tests {
    use super::{
        Line, Segment, Theme, diff_lines, is_unified_diff, markdown_lines, split_markdown_and_diffs,
    };
    use crate::theme::ThemeName;
    use ratatui::style::Modifier;

    #[test]
    fn requires_a_hunk_header() {
        assert!(is_unified_diff("@@ -1,1 +1,1 @@\n-old\n+new"));
    }

    #[test]
    fn prose_plus_minus_is_not_a_diff() {
        assert!(!is_unified_diff("use + and - in prose\nsecond line"));
    }

    #[test]
    fn markdown_list_is_not_a_diff() {
        assert!(!is_unified_diff("- item\n- item2"));
    }

    #[test]
    fn at_at_in_a_sentence_is_not_a_diff() {
        assert!(!is_unified_diff("see the @@ marker in the log"));
        assert!(!is_unified_diff("@@ -1 +1 @@\nno plus or minus body"));
    }

    #[test]
    fn fenced_in_prose_is_not_a_whole_message_diff() {
        let text = "Edited `cfg.go`:\n\n```diff\n@@ -1 +1 @@\n-a\n+b\n```\n\nDone.";
        assert!(
            !is_unified_diff(text),
            "fences are extracted by the splitter, not carded as the whole message"
        );
    }

    #[test]
    fn split_prose_fence_prose() {
        let text = "before\n\n```diff\n@@ -1 +1 @@\n-a\n+b\n```\n\nafter";
        assert_eq!(
            split_markdown_and_diffs(text),
            vec![
                Segment::Md("before".into()),
                Segment::Diff("@@ -1 +1 @@\n-a\n+b".into()),
                Segment::Md("after".into()),
            ]
        );
    }

    #[test]
    fn split_udiff_fence() {
        let segs = split_markdown_and_diffs("```udiff\n@@\n-a\n+b\n```");
        assert_eq!(segs, vec![Segment::Diff("@@\n-a\n+b".into())]);
    }

    #[test]
    fn split_bare_unified_diff() {
        assert_eq!(
            split_markdown_and_diffs("@@\n-a\n+b"),
            vec![Segment::Diff("@@\n-a\n+b".into())]
        );
    }

    #[test]
    fn split_markdown_list_stays_md() {
        assert_eq!(
            split_markdown_and_diffs("- a"),
            vec![Segment::Md("- a".into())]
        );
    }

    #[test]
    fn split_at_at_in_a_sentence_stays_md() {
        let text = "see the @@ marker in the log";
        assert_eq!(
            split_markdown_and_diffs(text),
            vec![Segment::Md(text.into())]
        );
    }

    #[test]
    fn diff_title_comes_from_the_plus_header() {
        let diff = "--- a/src/app.rs\n+++ b/src/app.rs\n@@ -1 +1 @@\n-old\n+new";
        assert_eq!(super::diff_file(diff).as_deref(), Some("src/app.rs"));
        assert_eq!(super::diff_file("@@ -1 +1 @@\n+new"), None);
        assert_eq!(super::diff_title("@@ -1 +1 @@\n+new"), "hunk");
        assert_eq!(
            super::diff_file("+++ cfg.go\n@@ -1 +1 @@\n+x").as_deref(),
            Some("cfg.go")
        );
        assert_eq!(
            super::diff_file("diff --git a/cfg.go b/cfg.go\n@@ -1 +1 @@\n+x").as_deref(),
            Some("cfg.go")
        );
        assert_eq!(
            super::diff_file("--- a/gone.go\n+++ /dev/null\n@@ -1 +0 @@\n-x").as_deref(),
            Some("gone.go")
        );
    }

    #[test]
    fn empty_source_lines_inside_a_hunk_are_not_painted() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines("@@ -1 +1 @@\n\n-old\n+new", theme, 20);
        assert_eq!(
            lines.len(),
            3,
            "{:?}",
            lines.iter().map(plain).collect::<Vec<_>>()
        );
        assert!(plain(&lines[0]).starts_with("@@"));
        assert!(plain(&lines[1]).starts_with('−'));
        assert!(plain(&lines[2]).starts_with('+'));
    }

    fn plain(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    // ---- markdown ------------------------------------------------------

    fn md(text: &str) -> Vec<Line<'static>> {
        markdown_lines(text, Theme::colored(ThemeName::CatppuccinMocha))
    }

    fn md_text(text: &str) -> Vec<String> {
        md(text).iter().map(plain).collect()
    }

    #[test]
    fn headings_are_styled_by_level_and_spaced() {
        let lines = md("# One\n\ntext\n\n## Two");
        let joined = md_text("# One\n\ntext\n\n## Two").join("\n");
        assert!(joined.contains("# One"), "{joined}");
        assert!(joined.contains("## Two"), "{joined}");
        // h1 and h2 must not paint identically.
        let h1 = lines[0].spans[0].style;
        let h2 = lines.last().unwrap().spans[0].style;
        assert_ne!(h1.fg, h2.fg);
        // No leading blank before the first heading.
        assert!(!plain(&lines[0]).trim().is_empty());
    }

    #[test]
    fn inline_emphasis_maps_to_modifiers() {
        let lines = md("**bold** _it_ ~~gone~~ `code`");
        let spans = &lines[0].spans;
        let find = |needle: &str| {
            spans
                .iter()
                .find(|s| s.content.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle}"))
                .style
        };
        assert!(find("bold").add_modifier.contains(Modifier::BOLD));
        assert!(find("it").add_modifier.contains(Modifier::ITALIC));
        assert!(find("gone").add_modifier.contains(Modifier::CROSSED_OUT));
        // Inline code is a tinted chip, not a modifier.
        assert!(find("code").bg.is_some());
    }

    #[test]
    fn fenced_code_keeps_a_gutter_and_shows_the_language() {
        let lines = md_text("```rust\nlet x = 1;\nlet y = 2;\n```");
        let joined = lines.join("\n");
        assert!(joined.contains("rust"), "language tag missing: {joined}");
        assert!(joined.contains("let x = 1;"), "{joined}");
        assert!(joined.contains("let y = 2;"), "{joined}");
        // Every body row carries the gutter bar.
        for line in lines.iter().filter(|l| l.contains("let ")) {
            assert!(line.starts_with('▏'), "no gutter: {line}");
        }
    }

    #[test]
    fn code_block_body_is_literal_not_reparsed() {
        // `*` and `#` inside a fence must survive verbatim.
        let joined = md_text("```\n# not a heading\n**not bold**\n```").join("\n");
        assert!(joined.contains("# not a heading"), "{joined}");
        assert!(joined.contains("**not bold**"), "{joined}");
    }

    #[test]
    fn blockquotes_get_a_bar_per_level() {
        let joined = md_text("> quoted").join("\n");
        assert!(joined.contains('▏'), "{joined}");
        assert!(joined.contains("quoted"), "{joined}");
    }

    #[test]
    fn bullets_and_ordinals_indent_by_depth() {
        let lines = md_text("- one\n- two\n  - deep");
        let joined = lines.join("\n");
        assert!(joined.contains("• one"), "{joined}");
        assert!(joined.contains("• two"), "{joined}");
        // Nested item uses the deeper glyph and is indented.
        let deep = lines.iter().find(|l| l.contains("deep")).unwrap();
        assert!(deep.contains('◦'), "{deep}");
        assert!(deep.starts_with(' '), "not indented: {deep:?}");

        let ordered = md_text("1. first\n2. second").join("\n");
        assert!(ordered.contains("1. first"), "{ordered}");
        assert!(ordered.contains("2. second"), "{ordered}");
    }

    #[test]
    fn links_render_text_and_append_a_differing_url() {
        let joined = md_text("[docs](https://x.dev)").join("\n");
        assert!(joined.contains("docs"), "{joined}");
        assert!(joined.contains("https://x.dev"), "{joined}");
        // A bare autolink should not repeat itself.
        let bare = md_text("<https://x.dev>").join("\n");
        assert_eq!(bare.matches("https://x.dev").count(), 1, "{bare}");
    }

    #[test]
    fn thematic_break_is_a_rule() {
        let joined = md_text("a\n\n---\n\nb").join("\n");
        assert!(joined.contains("─────"), "{joined}");
    }

    #[test]
    fn tables_align_columns_under_a_header() {
        let lines = md_text("| id | name |\n|----|------|\n| 1 | ada |\n| 20 | bo |");
        let joined = lines.join("\n");
        assert!(joined.contains("id"), "{joined}");
        assert!(joined.contains("ada"), "{joined}");
        // Column 2 starts at the same offset on both body rows.
        let row1 = lines.iter().find(|l| l.contains("ada")).unwrap();
        let row2 = lines.iter().find(|l| l.contains("bo")).unwrap();
        assert_eq!(
            row1.find("ada").unwrap(),
            row2.find("bo").unwrap(),
            "columns not aligned:\n{row1}\n{row2}"
        );
    }

    #[test]
    fn never_emits_two_blank_lines_in_a_row() {
        let doc = "# H\n\n\npara one\n\n\n\n- a\n- b\n\n\n> q\n\n```\ncode\n```\n\n\nend";
        let lines = md_text(doc);
        let mut prev_blank = false;
        for line in &lines {
            let blank = line.trim().is_empty();
            assert!(!(blank && prev_blank), "double blank in:\n{lines:#?}");
            prev_blank = blank;
        }
        // And no blank at either edge.
        assert!(!lines.first().unwrap().trim().is_empty());
        assert!(!lines.last().unwrap().trim().is_empty());
    }

    #[test]
    fn empty_input_still_yields_one_line() {
        assert_eq!(md("").len(), 1);
    }

    #[test]
    fn add_and_del_rows_are_padded_into_full_width_bands() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines("@@ -1 +1 @@\n-old\n+new", theme, 20);
        // hunk header, del band, add band
        assert_eq!(lines.len(), 3);
        for row in &lines[1..] {
            assert_eq!(plain(row).chars().count(), 20, "{:?}", plain(row));
        }
        // Padding carries the band background, not a default style.
        let tail = lines[1].spans.last().unwrap();
        assert_eq!(tail.style.bg, theme.del().bg);
        assert!(
            tail.content.chars().all(|ch| ch == ' '),
            "pad with spaces, not blocks: {:?}",
            tail.content
        );
    }

    #[test]
    fn tab_indented_rows_expand_and_stay_within_width() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines("@@ -1 +1 @@\n-\ttimeout := 30\n+\ttimeout := 60", theme, 24);
        assert_eq!(lines.len(), 3);
        for row in &lines[1..] {
            let text = plain(row);
            assert!(!text.contains('\t'), "{text:?}");
            assert_eq!(text.chars().count(), 24, "{text:?}");
        }
        // The add band is not a wrap leftover of the del wash.
        // A word-chip on the add row is still add-family, never the del band.
        let add_family = [theme.add().bg, theme.add_sign().bg, theme.add_chip().bg];
        assert!(
            lines[2]
                .spans
                .iter()
                .all(|span| add_family.contains(&span.style.bg)),
            "add row leaked another band: {:?}",
            lines[2]
                .spans
                .iter()
                .map(|span| span.style.bg)
                .collect::<Vec<_>>()
        );
        assert!(
            !lines[2]
                .spans
                .iter()
                .any(|span| span.style.bg == theme.del().bg)
        );
    }

    #[test]
    fn context_rows_are_not_padded_or_washed() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines("@@ -1 +1 @@\n unchanged", theme, 30);
        assert_eq!(plain(&lines[1]), " unchanged");
        assert_eq!(lines[1].spans[0].style.bg, None);
    }

    #[test]
    fn hunk_and_file_headers_are_meta_not_add_or_del() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines("--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n+added", theme, 40);
        for row in &lines[..3] {
            assert_eq!(row.spans[0].style, theme.diff_meta(), "{:?}", plain(row));
            assert_ne!(row.spans[0].style, theme.add());
        }
        // The real add row still gets the wash.
        assert_eq!(lines[3].spans[0].style, theme.add_sign());
    }

    #[test]
    fn signs_use_minus_and_survive_no_color() {
        let theme = Theme::plain(ThemeName::CatppuccinMocha);
        let lines = diff_lines("@@ -1 +1 @@\n-old\n+new", theme, 12);
        assert!(plain(&lines[1]).starts_with('−'), "want U+2212 minus");
        assert!(plain(&lines[2]).starts_with('+'));
        // No RGB when colour is off.
        assert_eq!(lines[1].spans[1].style.bg, None);
    }

    #[test]
    fn paired_edit_inverts_only_the_changed_span() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines("@@ -1 +1 @@\n-let x = 1;\n+let x = 2;", theme, 40);
        let del: Vec<&str> = lines[1]
            .spans
            .iter()
            .filter(|span| span.style == theme.del_chip())
            .map(|span| span.content.as_ref())
            .collect();
        let add: Vec<&str> = lines[2]
            .spans
            .iter()
            .filter(|span| span.style == theme.add_chip())
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(del, vec!["1"]);
        assert_eq!(add, vec!["2"]);
    }

    #[test]
    fn unrelated_rewrite_stays_a_plain_band() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines("@@ -1 +1 @@\n-alpha\n+omega", theme, 20);
        assert!(
            !lines[1]
                .spans
                .iter()
                .any(|span| span.style == theme.del_chip())
        );
    }
}
