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
            if !in_code && is_diff_fence_lang(info) {
                if let Some(rel) = lines[i + 1..]
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

pub fn markdown_lines(text: &str, theme: Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut style = theme.assistant();
    let options = Options::ENABLE_STRIKETHROUGH;

    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                style = theme.assistant().add_modifier(Modifier::BOLD);
            }
            Event::End(TagEnd::Heading(..)) => {
                lines.push(Line::from(std::mem::take(&mut current)));
                style = theme.assistant();
            }
            Event::Start(Tag::Strong) => style = style.add_modifier(Modifier::BOLD),
            Event::End(TagEnd::Strong) => style = theme.assistant(),
            Event::Start(Tag::Emphasis) => style = style.add_modifier(Modifier::ITALIC),
            Event::End(TagEnd::Emphasis) => style = theme.assistant(),
            Event::Code(code) => current.push(Span::styled(
                code.into_string(),
                if theme.colored {
                    style.add_modifier(Modifier::REVERSED)
                } else {
                    style
                },
            )),
            Event::Text(value) => current.push(Span::styled(value.into_string(), style)),
            Event::SoftBreak | Event::HardBreak => {
                lines.push(Line::from(std::mem::take(&mut current)));
            }
            Event::End(TagEnd::Paragraph) => {
                if !current.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current)));
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let label = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => format!("```{lang}"),
                    _ => "```".into(),
                };
                current.push(Span::styled(label, theme.note()));
            }
            Event::End(TagEnd::CodeBlock) => {
                lines.push(Line::from(std::mem::take(&mut current)));
                lines.push(Line::from(Span::styled("```", theme.note())));
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    if lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines
}

/// File path of a unified diff, from the `+++ b/path` header when present.
pub fn diff_file(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("+++ "))
        .map(|path| {
            path.split('\t')
                .next()
                .unwrap_or(path)
                .trim_start_matches("b/")
                .to_string()
        })
        .filter(|path| !path.is_empty() && path != "/dev/null")
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
            out.extend(std::iter::repeat(' ').take(n));
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
    Line::from(spans)
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
    use super::{Line, Segment, Theme, diff_lines, is_unified_diff, split_markdown_and_diffs};

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
    }

    fn plain(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn add_and_del_rows_are_padded_into_full_width_bands() {
        let theme = Theme { colored: true };
        let lines = diff_lines("@@ -1 +1 @@\n-old\n+new", theme, 20);
        // hunk header, del band, add band
        assert_eq!(lines.len(), 3);
        for row in &lines[1..] {
            assert_eq!(plain(row).chars().count(), 20, "{:?}", plain(row));
        }
        // Padding carries the band background, not a default style.
        let tail = lines[1].spans.last().unwrap();
        assert_eq!(tail.style.bg, theme.del().bg);
    }

    #[test]
    fn tab_indented_rows_expand_and_stay_within_width() {
        let theme = Theme { colored: true };
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
        let theme = Theme { colored: true };
        let lines = diff_lines("@@ -1 +1 @@\n unchanged", theme, 30);
        assert_eq!(plain(&lines[1]), " unchanged");
        assert_eq!(lines[1].spans[0].style.bg, None);
    }

    #[test]
    fn hunk_and_file_headers_are_meta_not_add_or_del() {
        let theme = Theme { colored: true };
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
        let theme = Theme { colored: false };
        let lines = diff_lines("@@ -1 +1 @@\n-old\n+new", theme, 12);
        assert!(plain(&lines[1]).starts_with('−'), "want U+2212 minus");
        assert!(plain(&lines[2]).starts_with('+'));
        // No RGB when colour is off.
        assert_eq!(lines[1].spans[1].style.bg, None);
    }

    #[test]
    fn paired_edit_inverts_only_the_changed_span() {
        let theme = Theme { colored: true };
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
        let theme = Theme { colored: true };
        let lines = diff_lines("@@ -1 +1 @@\n-alpha\n+omega", theme, 20);
        assert!(
            !lines[1]
                .spans
                .iter()
                .any(|span| span.style == theme.del_chip())
        );
    }
}
