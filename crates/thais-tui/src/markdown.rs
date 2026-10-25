//! Small, dependency-free Markdown renderer for the terminal transcript.
//!
//! The renderer intentionally uses only a few foreground colors and never sets
//! a background color, so it works with the user's terminal theme.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const ACCENT: Color = Color::Cyan;
const CODE: Color = Color::Yellow;
const MUTED: Color = Color::DarkGray;

/// Render Markdown text into terminal lines.
#[must_use]
pub fn markdown_lines(text: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code = false;
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            lines.push(Line::from(Span::styled(
                trimmed.to_owned(),
                Style::default().fg(MUTED),
            )));
            continue;
        }
        if in_code {
            lines.push(Line::from(Span::styled(
                line.to_owned(),
                Style::default().fg(CODE),
            )));
            continue;
        }
        if trimmed.is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        if let Some(heading) = heading_text(trimmed) {
            lines.push(Line::from(Span::styled(
                heading.to_owned(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if is_horizontal_rule(trimmed) {
            lines.push(Line::from(Span::styled(
                "─".repeat(24),
                Style::default().fg(MUTED),
            )));
            continue;
        }
        if let Some(quote) = trimmed.strip_prefix('>') {
            lines.push(Line::from(Span::styled(
                format!("│ {}", quote.trim_start()),
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            )));
            continue;
        }
        if let Some((checked, task)) = task_text(trimmed) {
            let marker = if checked { "☑" } else { "☐" };
            let style = if checked {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} "), style),
                Span::raw(task.to_owned()),
            ]));
            continue;
        }
        if let Some(item) = bullet_text(trimmed) {
            lines.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(ACCENT)),
                Span::raw(item.to_owned()),
            ]));
            continue;
        }
        if let Some((number, item)) = numbered_text(trimmed) {
            lines.push(Line::from(vec![
                Span::styled(format!("{number}. "), Style::default().fg(ACCENT)),
                Span::raw(item.to_owned()),
            ]));
            continue;
        }
        lines.push(Line::from(inline_spans(line)));
    }
    lines
}

fn heading_text(line: &str) -> Option<&str> {
    let level = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = line[level..].trim_start();
    (!rest.is_empty()).then_some(rest)
}

fn is_horizontal_rule(line: &str) -> bool {
    let line = line.trim();
    line.len() >= 3 && line.chars().all(|character| character == '-')
}

fn task_text(line: &str) -> Option<(bool, &str)> {
    for (marker, checked) in [("- [ ] ", false), ("- [x] ", true), ("- [X] ", true)] {
        if let Some(task) = line.strip_prefix(marker) {
            return Some((checked, task));
        }
    }
    None
}

fn bullet_text(line: &str) -> Option<&str> {
    ["- ", "* ", "+ "]
        .into_iter()
        .find_map(|marker| line.strip_prefix(marker))
}

fn numbered_text(line: &str) -> Option<(&str, &str)> {
    let (number, rest) = line.split_once(". ")?;
    if number.chars().all(|character| character.is_ascii_digit()) && !number.is_empty() {
        Some((number, rest))
    } else {
        None
    }
}

fn inline_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut index = 0;
    while index < text.len() {
        let rest = &text[index..];
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
        {
            spans.push(Span::styled(
                after[..end].to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            index += 2 + end + 2;
            continue;
        }
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            spans.push(Span::styled(
                after[..end].to_owned(),
                Style::default().fg(ACCENT),
            ));
            index += 1 + end + 1;
            continue;
        }
        if let Some(after) = rest.strip_prefix('*')
            && let Some(end) = after.find('*')
        {
            spans.push(Span::styled(
                after[..end].to_owned(),
                Style::default().add_modifier(Modifier::ITALIC),
            ));
            index += 1 + end + 1;
            continue;
        }
        if let Some(after) = rest.strip_prefix('[')
            && let Some((label, target)) = after.split_once("](")
            && let Some((url, _)) = target.split_once(')')
        {
            spans.push(Span::styled(label.to_owned(), Style::default().fg(ACCENT)));
            spans.push(Span::styled(
                format!(" ({url})"),
                Style::default().fg(MUTED),
            ));
            index += 1 + label.len() + 2 + url.len() + 1;
            continue;
        }

        let next = ["**", "`", "*", "["]
            .iter()
            .filter_map(|marker| rest.find(marker))
            .min()
            .unwrap_or(rest.len());
        if next == 0 {
            let character = rest
                .chars()
                .next()
                .expect("index is on a character boundary");
            spans.push(Span::raw(character.to_string()));
            index += character.len_utf8();
        } else {
            spans.push(Span::raw(rest[..next].to_owned()));
            index += next;
        }
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.to_string()
    }

    #[test]
    fn renders_headings_lists_tasks_and_code() {
        let lines =
            markdown_lines("# Title\n- item\n- [ ] open\n- [x] done\n```rust\nlet x = 1;\n```\n");
        assert!(text(&lines[0]).contains("Title"));
        assert!(text(&lines[1]).contains("• item"));
        assert!(text(&lines[2]).contains("☐ open"));
        assert!(text(&lines[3]).contains("☑ done"));
        assert!(lines.iter().any(|line| text(line).contains("let x")));
    }

    #[test]
    fn renders_inline_markdown() {
        let lines = markdown_lines("A **bold** word, `code`, *italic*, and [site](https://x).");
        let text = text(&lines[0]);
        assert!(text.contains("bold"));
        assert!(text.contains("code"));
        assert!(text.contains("italic"));
        assert!(text.contains("site"));
        assert!(text.contains("https://x"));
    }

    #[test]
    fn unclosed_markers_do_not_panic() {
        let lines = markdown_lines("**open `code [link](");
        assert_eq!(lines.len(), 1);
        assert!(!text(&lines[0]).is_empty());
    }
}
