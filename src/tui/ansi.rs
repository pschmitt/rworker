//! ANSI SGR sequences (the colors of job logs, events, inspect and ps) to
//! ratatui spans. Other escape sequences are dropped.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

fn apply(style: Style, params: &str) -> Style {
    let nums: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params.split(';').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let mut s = style;
    let mut i = 0;
    while i < nums.len() {
        let n = nums[i];
        match n {
            0 => s = Style::default(),
            1 => s = s.add_modifier(Modifier::BOLD),
            2 => s = s.add_modifier(Modifier::DIM),
            3 => s = s.add_modifier(Modifier::ITALIC),
            4 => s = s.add_modifier(Modifier::UNDERLINED),
            7 => s = s.add_modifier(Modifier::REVERSED),
            22 => s = s.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => s = s.remove_modifier(Modifier::ITALIC),
            24 => s = s.remove_modifier(Modifier::UNDERLINED),
            27 => s = s.remove_modifier(Modifier::REVERSED),
            30..=37 => s = s.fg(Color::Indexed((n - 30) as u8)),
            39 => s = s.fg(Color::Reset),
            40..=47 => s = s.bg(Color::Indexed((n - 40) as u8)),
            49 => s = s.bg(Color::Reset),
            90..=97 => s = s.fg(Color::Indexed((n - 90 + 8) as u8)),
            100..=107 => s = s.bg(Color::Indexed((n - 100 + 8) as u8)),
            38 | 48 => {
                let color = match nums.get(i + 1) {
                    Some(5) => {
                        let c = nums.get(i + 2).map(|&v| Color::Indexed(v as u8));
                        i += 2;
                        c
                    }
                    Some(2) if i + 4 < nums.len() => {
                        let c = Color::Rgb(nums[i + 2] as u8, nums[i + 3] as u8, nums[i + 4] as u8);
                        i += 4;
                        Some(c)
                    }
                    _ => None,
                };
                if let Some(c) = color {
                    s = if n == 38 { s.fg(c) } else { s.bg(c) };
                }
            }
            _ => {}
        }
        i += 1;
    }
    s
}

/// One line of terminal output as styled spans.
pub fn line(text: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut style = Style::default();
    let mut buf = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            if c == '\t' {
                buf.push_str("    ");
            } else if !c.is_control() {
                buf.push(c);
            }
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                let mut params = String::new();
                let mut fin = None;
                for p in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&p) {
                        fin = Some(p);
                        break;
                    }
                    params.push(p);
                }
                if fin == Some('m') {
                    if !buf.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut buf), style));
                    }
                    style = apply(style, &params);
                }
            }
            Some(']') => {
                // OSC: up to BEL or ST.
                chars.next();
                while let Some(p) = chars.next() {
                    if p == '\x07' || (p == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    Line::from(spans)
}

/// The text of a line without escape sequences (for filters and widths).
pub fn strip(text: &str) -> String {
    line(text)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_and_plain_text() {
        let l = line("\x1b[1;32mw/job\x1b[0m hello\x1b[K");
        assert_eq!(l.spans.len(), 2);
        assert_eq!(l.spans[0].content, "w/job");
        assert!(l.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(l.spans[0].style.fg, Some(Color::Indexed(2)));
        assert_eq!(l.spans[1].content, " hello");
        assert_eq!(l.spans[1].style, Style::default());
        assert_eq!(strip("\x1b[38;5;208mx\x1b[0m\ty"), "x    y");
    }
}
