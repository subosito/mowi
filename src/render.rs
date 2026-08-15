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
    let (action, body) = peel_diff_action(text);
    let text = if action.is_some() { body } else { text };
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

/// Leading host line (`edited src/app.rs`) plus the unified-diff body.
///
/// Write/edit results often prefix the hunk with the same path the card
/// header already shows. Peel that verb off so the body is not a second
/// path row.
pub fn peel_diff_action(text: &str) -> (Option<&'static str>, &str) {
    let trimmed = text.trim_start_matches('\n');
    let Some((first, rest)) = trimmed.split_once('\n') else {
        return (None, text);
    };
    let first = first.trim();
    let Some((verb, path)) = first.split_once(char::is_whitespace) else {
        return (None, text);
    };
    let verb = verb.to_ascii_lowercase();
    let action = match verb.as_str() {
        "edit" | "edited" | "update" | "updated" => Some("edit"),
        "write" | "wrote" | "create" | "created" => Some("write"),
        "delete" | "deleted" | "remove" | "removed" => Some("delete"),
        _ => None,
    };
    let Some(action) = action else {
        return (None, text);
    };
    let path = path.trim();
    if path.is_empty() {
        return (None, text);
    }
    // Only peel when the rest is (or contains) a unified diff — a lone
    // "edited notes.txt" note should stay prose.
    if !is_unified_diff(rest) && !rest.lines().any(|line| line.starts_with("@@") || line.starts_with("---") || line.starts_with("+++"))
    {
        return (None, text);
    }
    let _ = path;
    (Some(action), rest.trim_start_matches('\n'))
}

/// Header glyph for a peeled write/edit verb. Path stays on the title rail.
pub fn diff_action_glyph(action: &str) -> &'static str {
    match action {
        "write" => "✦",
        "delete" => "✕",
        _ => "✎",
    }
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
/// is a rectangle rather than a ragged stripe. File and hunk headers (`---`,
/// `+++`, `@@`, `diff --git`) stay out of the body — the card title already
/// names the file. Context rows carry no wash. When a `-` row is followed by a
/// `+` row, the changed span of each is inverted as a word chip.
pub fn diff_lines(text: &str, theme: Theme, width: u16) -> Vec<Line<'static>> {
    let rows: Vec<String> = text.lines().map(|row| expand_tabs(row, 0)).collect();
    let number_width = diff_number_width(&rows);
    let gutter_width = number_width + 3; // number + " │ "
    let body_width = width.saturating_sub(gutter_width as u16).max(1);
    let mut old_line: Option<usize> = None;
    let mut new_line: Option<usize> = None;
    let mut out = Vec::with_capacity(rows.len());
    let mut index = 0;
    let mut pending_context: Vec<(Option<usize>, String)> = Vec::new();
    while index < rows.len() {
        let row = rows[index].as_str();
        if row.is_empty() {
            index += 1;
            continue;
        }
        if row.starts_with("@@") {
            flush_context(&mut pending_context, &mut out, number_width, theme);
            if let Some((old, new, _, _)) = parse_hunk(row) {
                if !out.is_empty() {
                    let skipped = hunk_gap(old_line, new_line, old, new);
                    if skipped > 0 {
                        out.push(hunk_gap_line(skipped, theme));
                    }
                }
                old_line = Some(old);
                new_line = Some(new);
            }
            index += 1;
            let mut hunk: Vec<String> = Vec::new();
            while index < rows.len() {
                let next = rows[index].as_str();
                if next.starts_with("@@")
                    || next.starts_with("diff --git")
                    || next.starts_with("+++")
                    || next.starts_with("---")
                {
                    break;
                }
                hunk.push(rows[index].clone());
                index += 1;
            }
            paint_hunk(
                &hunk,
                &mut old_line,
                &mut new_line,
                &mut pending_context,
                &mut out,
                number_width,
                body_width,
                theme,
            );
            continue;
        }
        if row.starts_with("+++") || row.starts_with("---") || row.starts_with("diff --git") {
            index += 1;
            continue;
        }
        if row.starts_with("\\ No newline") {
            flush_context(&mut pending_context, &mut out, number_width, theme);
            out.push(numbered_context(None, row, number_width, theme));
            index += 1;
            continue;
        }
        let del_style = (theme.del(), theme.del_sign(), theme.del_chip());
        let add_style = (theme.add(), theme.add_sign(), theme.add_chip());
        match (row.strip_prefix('-'), row.strip_prefix('+')) {
            (Some(old_body), None) => {
                flush_context(&mut pending_context, &mut out, number_width, theme);
                let new_body = rows
                    .get(index + 1)
                    .map(|s| s.as_str())
                    .filter(|next| !next.starts_with("+++"))
                    .and_then(|next| next.strip_prefix('+'));
                let old_no = old_line;
                old_line = old_line.map(|n| n + 1);
                match new_body {
                    Some(new_body) if new_body == old_body => {
                        // Host often emits `-same` / `+same` for alignment.
                        // That is context, not a change — do not paint both signs.
                        let no = old_no.or(new_line);
                        new_line = new_line.map(|n| n + 1);
                        pending_context.push((no, old_body.to_string()));
                        index += 2;
                    }
                    Some(new_body) => {
                        let new_no = new_line;
                        new_line = new_line.map(|n| n + 1);
                        let (old_chip, new_chip) = match changed_span(old_body, new_body) {
                            Some((old_range, new_range)) => (Some(old_range), Some(new_range)),
                            None => (None, None),
                        };
                        out.push(numbered_band(
                            (old_no, theme.diff_old_no()),
                            '−',
                            old_body,
                            old_chip,
                            del_style,
                            number_width,
                            body_width,
                            theme,
                        ));
                        out.push(numbered_band(
                            (new_no, theme.diff_new_no()),
                            '+',
                            new_body,
                            new_chip,
                            add_style,
                            number_width,
                            body_width,
                            theme,
                        ));
                        index += 2;
                    }
                    None => {
                        out.push(numbered_band(
                            (old_no, theme.diff_old_no()),
                            '−',
                            old_body,
                            None,
                            del_style,
                            number_width,
                            body_width,
                            theme,
                        ));
                        index += 1;
                    }
                }
            }
            (None, Some(new_body)) => {
                flush_context(&mut pending_context, &mut out, number_width, theme);
                let new_no = new_line;
                new_line = new_line.map(|n| n + 1);
                out.push(numbered_band(
                    (new_no, theme.diff_new_no()),
                    '+',
                    new_body,
                    None,
                    add_style,
                    number_width,
                    body_width,
                    theme,
                ));
                index += 1;
            }
            _ => {
                let body = row.strip_prefix(' ').unwrap_or(row);
                let old_no = old_line;
                let new_no = new_line;
                old_line = old_line.map(|n| n + 1);
                new_line = new_line.map(|n| n + 1);
                pending_context.push((old_no.or(new_no), body.to_string()));
                index += 1;
            }
        }
    }
    flush_context(&mut pending_context, &mut out, number_width, theme);
    out
}

fn flush_context(
    pending: &mut Vec<(Option<usize>, String)>,
    out: &mut Vec<Line<'static>>,
    number_width: usize,
    theme: Theme,
) {
    const KEEP: usize = 2;
    if pending.len() > KEEP * 2 {
        let skipped = pending.len() - KEEP * 2;
        let head: Vec<_> = pending.drain(..KEEP).collect();
        let tail: Vec<_> = pending.split_off(pending.len().saturating_sub(KEEP));
        pending.clear();
        for (no, body) in head {
            out.push(numbered_context(no, &body, number_width, theme));
        }
        out.push(hunk_gap_line(skipped, theme));
        for (no, body) in tail {
            out.push(numbered_context(no, &body, number_width, theme));
        }
    } else {
        for (no, body) in pending.drain(..) {
            out.push(numbered_context(no, &body, number_width, theme));
        }
    }
}

fn paint_hunk(
    hunk: &[String],
    old_line: &mut Option<usize>,
    new_line: &mut Option<usize>,
    pending_context: &mut Vec<(Option<usize>, String)>,
    out: &mut Vec<Line<'static>>,
    number_width: usize,
    body_width: u16,
    theme: Theme,
) {
    let mut old_rows = Vec::new();
    let mut new_rows = Vec::new();
    let flush_change = |old_rows: &mut Vec<String>,
                        new_rows: &mut Vec<String>,
                        old_line: &mut Option<usize>,
                        new_line: &mut Option<usize>,
                        pending_context: &mut Vec<(Option<usize>, String)>,
                        out: &mut Vec<Line<'static>>| {
        replay_ops(
            align_lines(old_rows, new_rows),
            old_line,
            new_line,
            pending_context,
            out,
            number_width,
            body_width,
            theme,
        );
        old_rows.clear();
        new_rows.clear();
    };
    for row in hunk {
        if row.starts_with('\\') {
            continue;
        }
        if let Some(body) = row.strip_prefix('-') {
            old_rows.push(body.to_string());
        } else if let Some(body) = row.strip_prefix('+') {
            new_rows.push(body.to_string());
        } else {
            flush_change(
                &mut old_rows,
                &mut new_rows,
                old_line,
                new_line,
                pending_context,
                out,
            );
            let body = row.strip_prefix(' ').unwrap_or(row.as_str()).to_string();
            let no = *old_line;
            *old_line = old_line.map(|n| n + 1);
            *new_line = new_line.map(|n| n + 1);
            pending_context.push((no.or(*new_line), body));
        }
    }
    flush_change(
        &mut old_rows,
        &mut new_rows,
        old_line,
        new_line,
        pending_context,
        out,
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AlignKind {
    Equal,
    Del,
    Add,
}

fn align_lines(old: &[String], new: &[String]) -> Vec<(AlignKind, String)> {
    let n = old.len();
    let m = new.len();
    if n == 0 && m == 0 {
        return Vec::new();
    }
    // LCS — same idea as mow filediff.go / sergi go-diff. Hunks are small.
    if n.saturating_mul(m) > 80_000 {
        let mut ops = Vec::with_capacity(n + m);
        for row in old {
            ops.push((AlignKind::Del, row.clone()));
        }
        for row in new {
            ops.push((AlignKind::Add, row.clone()));
        }
        return ops;
    }
    let mut lcs = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if old[i] == new[j] {
                lcs[i + 1][j + 1].saturating_add(1)
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old[i] == new[j] {
            ops.push((AlignKind::Equal, old[i].clone()));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            ops.push((AlignKind::Del, old[i].clone()));
            i += 1;
        } else {
            ops.push((AlignKind::Add, new[j].clone()));
            j += 1;
        }
    }
    while i < n {
        ops.push((AlignKind::Del, old[i].clone()));
        i += 1;
    }
    while j < m {
        ops.push((AlignKind::Add, new[j].clone()));
        j += 1;
    }
    ops
}

fn replay_ops(
    ops: Vec<(AlignKind, String)>,
    old_line: &mut Option<usize>,
    new_line: &mut Option<usize>,
    pending_context: &mut Vec<(Option<usize>, String)>,
    out: &mut Vec<Line<'static>>,
    number_width: usize,
    body_width: u16,
    theme: Theme,
) {
    let del_style = (theme.del(), theme.del_sign(), theme.del_chip());
    let add_style = (theme.add(), theme.add_sign(), theme.add_chip());
    let mut i = 0;
    while i < ops.len() {
        match ops[i].0 {
            AlignKind::Equal => {
                let no = *old_line;
                *old_line = old_line.map(|n| n + 1);
                *new_line = new_line.map(|n| n + 1);
                pending_context.push((no.or(*new_line), ops[i].1.clone()));
                i += 1;
            }
            AlignKind::Del => {
                flush_context(pending_context, out, number_width, theme);
                let old_no = *old_line;
                *old_line = old_line.map(|n| n + 1);
                if i + 1 < ops.len() && ops[i + 1].0 == AlignKind::Add {
                    let new_no = *new_line;
                    *new_line = new_line.map(|n| n + 1);
                    let (old_chip, new_chip) = match changed_span(&ops[i].1, &ops[i + 1].1) {
                        Some((a, b)) => (Some(a), Some(b)),
                        None => (None, None),
                    };
                    out.push(numbered_band(
                        (old_no, theme.diff_old_no()),
                        '−',
                        &ops[i].1,
                        old_chip,
                        del_style,
                        number_width,
                        body_width,
                        theme,
                    ));
                    out.push(numbered_band(
                        (new_no, theme.diff_new_no()),
                        '+',
                        &ops[i + 1].1,
                        new_chip,
                        add_style,
                        number_width,
                        body_width,
                        theme,
                    ));
                    i += 2;
                } else {
                    out.push(numbered_band(
                        (old_no, theme.diff_old_no()),
                        '−',
                        &ops[i].1,
                        None,
                        del_style,
                        number_width,
                        body_width,
                        theme,
                    ));
                    i += 1;
                }
            }
            AlignKind::Add => {
                flush_context(pending_context, out, number_width, theme);
                let new_no = *new_line;
                *new_line = new_line.map(|n| n + 1);
                out.push(numbered_band(
                    (new_no, theme.diff_new_no()),
                    '+',
                    &ops[i].1,
                    None,
                    add_style,
                    number_width,
                    body_width,
                    theme,
                ));
                i += 1;
            }
        }
    }
}

/// `@@ -old,old_count +new,new_count @@` — counts default to 1.
fn parse_hunk(row: &str) -> Option<(usize, usize, usize, usize)> {
    let mut fields = row.split_whitespace();
    (fields.next()? == "@@").then_some(())?;
    let (old, old_count) = parse_hunk_side(fields.next()?, '-')?;
    let (new, new_count) = parse_hunk_side(fields.next()?, '+')?;
    Some((old, new, old_count, new_count))
}

fn parse_hunk_side(field: &str, sign: char) -> Option<(usize, usize)> {
    let field = field.strip_prefix(sign)?;
    let mut parts = field.split(',');
    let start = parts.next()?.parse().ok()?;
    let count = parts.next().map(|n| n.parse().ok()).unwrap_or(Some(1))?;
    Some((start, count))
}

fn hunk_last_line(start: usize, count: usize) -> usize {
    start.saturating_add(count.saturating_sub(1).max(0))
}

fn hunk_gap(old_line: Option<usize>, new_line: Option<usize>, old: usize, new: usize) -> usize {
    match (old_line, new_line) {
        (Some(prev_old), Some(prev_new)) => old.saturating_sub(prev_old).max(new.saturating_sub(prev_new)),
        (Some(prev_old), None) => old.saturating_sub(prev_old),
        (None, Some(prev_new)) => new.saturating_sub(prev_new),
        _ => 0,
    }
}

fn hunk_gap_line(skipped: usize, theme: Theme) -> Line<'static> {
    let noun = if skipped == 1 { "line" } else { "lines" };
    Line::from(Span::styled(
        format!("… {skipped} unchanged {noun}"),
        theme.diff_meta(),
    ))
}

fn diff_number_width(rows: &[String]) -> usize {
    let mut max_no = 1usize;
    for row in rows {
        if let Some((old, new, old_count, new_count)) = parse_hunk(row) {
            max_no = max_no
                .max(hunk_last_line(old, old_count))
                .max(hunk_last_line(new, new_count));
        }
    }
    max_no.to_string().len().max(2)
}

fn gutter_spans(
    number: Option<usize>,
    digits: usize,
    style: Style,
    theme: Theme,
) -> Vec<Span<'static>> {
    let text = number.map_or_else(|| " ".repeat(digits), |n| format!("{n:>digits$}"));
    vec![
        Span::styled(text, style),
        Span::styled(" │ ", theme.diff_gutter()),
    ]
}

fn numbered_context(
    number: Option<usize>,
    body: &str,
    digits: usize,
    theme: Theme,
) -> Line<'static> {
    let mut spans = gutter_spans(number, digits, theme.diff_meta(), theme);
    // Same two-cell sign gutter as add/del (`+ ` / `− `), but empty so
    // surrounding text does not look like a change.
    spans.push(Span::styled("  ", theme.context()));
    spans.push(Span::styled(body.to_string(), theme.context()));
    Line::from(spans)
}

fn numbered_band(
    number: (Option<usize>, Style),
    sign: char,
    body: &str,
    chip: Option<Range<usize>>,
    styles: (Style, Style, Style),
    digits: usize,
    width: u16,
    theme: Theme,
) -> Line<'static> {
    let mut spans = gutter_spans(number.0, digits, number.1, theme);
    spans.extend(band(sign, body, chip, styles, width).spans);
    Line::from(spans)
}

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

fn band(
    sign: char,
    body: &str,
    chip: Option<Range<usize>>,
    styles: (Style, Style, Style),
    width: u16,
) -> Line<'static> {
    let (band, sign_style, chip_style) = styles;
    // Reserve a dedicated two-cell sign gutter: `+ ` / `− `.
    let max_body = (width as usize).saturating_sub(2);
    let body = clip_cols(body, max_body);
    let chip = chip.filter(|range| {
        range.start <= body.len() && range.end <= body.len() && range.start <= range.end
    });
    let mut spans = vec![
        Span::styled(sign.to_string(), sign_style),
        Span::styled(" ", band),
    ];
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
    fn peel_edited_path_off_a_write_result() {
        let raw =
            "edited src/app.rs\n--- src/app.rs\n+++ src/app.rs\n@@ -1 +1 @@\n-old\n+new\n";
        assert_eq!(
            super::peel_diff_action(raw),
            (
                Some("edit"),
                "--- src/app.rs\n+++ src/app.rs\n@@ -1 +1 @@\n-old\n+new\n"
            )
        );
        assert_eq!(super::diff_action_glyph("edit"), "✎");
        assert_eq!(
            split_markdown_and_diffs(raw),
            vec![Segment::Diff(
                "--- src/app.rs\n+++ src/app.rs\n@@ -1 +1 @@\n-old\n+new\n".into()
            )]
        );
        assert_eq!(
            super::peel_diff_action("edited notes.txt"),
            (None, "edited notes.txt")
        );
    }

    #[test]
    fn empty_source_lines_inside_a_hunk_are_not_painted() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines("@@ -1 +1 @@\n\n-old\n+new", theme, 20);
        assert_eq!(
            lines.len(),
            2,
            "{:?}",
            lines.iter().map(plain).collect::<Vec<_>>()
        );
        assert!(plain(&lines[0]).contains('−'));
        assert!(plain(&lines[1]).contains('+'));
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
    fn diff_rows_show_old_new_line_numbers_and_context() {
        let theme = Theme::plain(ThemeName::CatppuccinMocha);
        let lines = diff_lines(
            "--- a/x.rs\n+++ b/x.rs\n@@ -10,4 +10,4 @@\n keep before\n-old value\n+new value\n keep after",
            theme,
            40,
        );
        let text: Vec<String> = lines.iter().map(plain).collect();
        assert!(
            text.iter().any(|row| row.contains("10 │   keep before")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|row| row.contains("│ − old value")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|row| row.contains("│ + new value")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|row| row.contains("12 │   keep after")),
            "{text:?}"
        );
        assert!(
            text.iter().all(|row| {
                !row.contains("keep before") || (!row.contains('+') && !row.contains('−'))
            }),
            "context must not carry a change sign: {text:?}"
        );
    }

    #[test]
    fn add_and_del_rows_are_padded_into_full_width_bands() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines("@@ -1 +1 @@\n-old\n+new", theme, 20);
        // del band, add band — hunk header is omitted
        assert_eq!(lines.len(), 2);
        for row in &lines {
            assert_eq!(plain(row).chars().count(), 20, "{:?}", plain(row));
        }
        // Padding carries the band background, not a default style.
        let tail = lines[0].spans.last().unwrap();
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
        assert_eq!(lines.len(), 2);
        for row in &lines {
            let text = plain(row);
            assert!(!text.contains('\t'), "{text:?}");
            assert_eq!(text.chars().count(), 24, "{text:?}");
        }
        // The add band is not a wrap leftover of the del wash.
        // A word-chip on the add row is still add-family, never the del band.
        let add_family = [theme.add().bg, theme.add_sign().bg, theme.add_chip().bg];
        assert!(
            lines[1]
                .spans
                .iter()
                .all(|span| span.style.bg.is_none() || add_family.contains(&span.style.bg)),
            "add row leaked another band: {:?}",
            lines[1]
                .spans
                .iter()
                .map(|span| span.style.bg)
                .collect::<Vec<_>>()
        );
        assert!(
            !lines[1]
                .spans
                .iter()
                .any(|span| span.style.bg == theme.del().bg)
        );
    }

    #[test]
    fn context_rows_are_not_padded_or_washed() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines("@@ -1 +1 @@\n unchanged", theme, 30);
        assert!(plain(&lines[0]).ends_with("unchanged"));
        assert_eq!(lines[0].spans[0].style.bg, None);
    }

    #[test]
    fn hunk_and_file_headers_are_omitted_from_the_body() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines("--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n+added", theme, 40);
        let text: Vec<String> = lines.iter().map(plain).collect();
        assert!(
            text.iter()
                .all(|row| !row.contains("---") && !row.contains("+++") && !row.contains("@@")),
            "{text:?}"
        );
        assert_eq!(lines.len(), 1);
        // The real add row still gets the wash.
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.style == theme.add_sign())
        );
    }

    #[test]
    fn signs_use_minus_and_survive_no_color() {
        let theme = Theme::plain(ThemeName::CatppuccinMocha);
        let lines = diff_lines("@@ -1 +1 @@\n-old\n+new", theme, 12);
        assert!(plain(&lines[0]).contains('−'), "want U+2212 minus");
        assert!(plain(&lines[1]).contains('+'));
        // No RGB when colour is off.
        assert!(lines[0].spans.iter().all(|span| span.style.bg.is_none()));
    }

    #[test]
    fn paired_edit_inverts_only_the_changed_span() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines("@@ -1 +1 @@\n-let x = 1;\n+let x = 2;", theme, 40);
        let del: Vec<&str> = lines[0]
            .spans
            .iter()
            .filter(|span| span.style == theme.del_chip())
            .map(|span| span.content.as_ref())
            .collect();
        let add: Vec<&str> = lines[1]
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
            !lines[0]
                .spans
                .iter()
                .any(|span| span.style == theme.del_chip())
        );
    }

    #[test]
    fn later_hunks_insert_an_unchanged_gap() {
        let theme = Theme::colored(ThemeName::CatppuccinMocha);
        let lines = diff_lines(
            "@@ -3,2 +3,2 @@\n one()\n-old_one();\n+new_one();\n@@ -12,2 +12,2 @@\n ctx_two();\n-old_two();\n+new_two();",
            theme,
            40,
        );
        let text: Vec<String> = lines.iter().map(plain).collect();
        assert!(
            text.iter().any(|row| row.contains("… 7 unchanged lines")),
            "{text:?}"
        );
        assert!(
            text.iter().all(|row| !row.contains("@@")),
            "{text:?}"
        );
        let gap = lines
            .iter()
            .find(|row| plain(row).contains("unchanged"))
            .expect("gap");
        assert_eq!(gap.spans[0].style, theme.diff_meta());
    }

    #[test]
    fn long_context_runs_collapse_to_an_unchanged_gap() {
        let theme = Theme::plain(ThemeName::CatppuccinMocha);
        let ctx = (0..8)
            .map(|i| format!(" keep{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = diff_lines(
            &format!("@@ -1,10 +1,10 @@\n{ctx}\n-old\n+new"),
            theme,
            40,
        );
        let text: Vec<String> = lines.iter().map(plain).collect();
        assert!(
            text.iter().any(|row| row.contains("… 4 unchanged lines")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|row| row.contains("keep0")),
            "keep the first context rows: {text:?}"
        );
        assert!(
            text.iter().any(|row| row.contains("keep7")),
            "keep the last context rows: {text:?}"
        );
        assert!(
            text.iter().all(|row| !row.contains("keep3")),
            "middle context should collapse: {text:?}"
        );
    }

    #[test]
    fn identical_minus_plus_pair_paints_as_context() {
        let theme = Theme::plain(ThemeName::CatppuccinMocha);
        let lines = diff_lines(
            "@@ -1,3 +1,3 @@\n same()\n-old();\n+old();\n keep()\n",
            theme,
            40,
        );
        let text: Vec<String> = lines.iter().map(plain).collect();
        assert!(
            text.iter().any(|row| {
                row.contains("old();") && !row.contains('−') && !row.contains('+')
            }),
            "identical pair must be context: {text:?}"
        );
        assert!(
            text.iter()
                .all(|row| !row.contains("− old();") && !row.contains("+ old();")),
            "{text:?}"
        );
    }

    #[test]
    fn number_gutter_fits_the_last_line_in_a_hunk() {
        assert_eq!(
            super::diff_number_width(&[
                "@@ -98,20 +98,20 @@".into(),
                " context".into(),
            ]),
            3
        );
        assert_eq!(super::parse_hunk("@@ -10,4 +12,6 @@"), Some((10, 12, 4, 6)));
    }
}
