use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

use crate::theme::Theme;

pub fn is_unified_diff(text: &str) -> bool {
    let has_hunk = text.lines().any(|line| line.starts_with("@@"));
    let has_change = text
        .lines()
        .any(|line| line.starts_with('+') || line.starts_with('-'));
    has_hunk || (has_change && text.lines().count() > 1)
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
