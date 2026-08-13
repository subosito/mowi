use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

use crate::theme::Theme;

pub fn is_unified_diff(text: &str) -> bool {
    text.lines().any(|line| line.starts_with("@@"))
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

pub fn diff_lines(text: &str, theme: Theme) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| {
            if line.starts_with("+++") || line.starts_with("---") {
                Line::from(Span::styled(line.to_string(), theme.context()))
            } else if line.starts_with('+') {
                Line::from(Span::styled(line.to_string(), theme.add()))
            } else if line.starts_with('-') {
                Line::from(Span::styled(line.to_string(), theme.del()))
            } else {
                Line::from(Span::styled(line.to_string(), theme.context()))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::is_unified_diff;

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
    fn diff_title_comes_from_the_plus_header() {
        let diff = "--- a/src/app.rs\n+++ b/src/app.rs\n@@ -1 +1 @@\n-old\n+new";
        assert_eq!(super::diff_file(diff).as_deref(), Some("src/app.rs"));
        assert_eq!(super::diff_file("@@ -1 +1 @@\n+new"), None);
    }
}
