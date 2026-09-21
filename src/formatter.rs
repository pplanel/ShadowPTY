//! Formats the `vt100::Screen` grid into semantic text preserving color and styling.
//!
//! Rather than stripping colors or dumping raw ANSI escape sequences (which can confuse LLMs),
//! this formatter outputs semantic XML-style tags such as:
//! `<fg:red><bold>Error</bold></fg> <bg:blue>info</bg>`
//!
//! Contiguous cells sharing the same attributes are grouped together to minimize token overhead.

use std::fmt::Write as _;
use vt100::{Cell, Color, Screen};

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl CellStyle {
    #[must_use]
    pub fn from_cell(cell: &Cell) -> Self {
        Self {
            fg: cell.fgcolor(),
            bg: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }

    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self.fg, Color::Default)
            && matches!(self.bg, Color::Default)
            && !self.bold
            && !self.dim
            && !self.italic
            && !self.underline
            && !self.inverse
    }
}

/// Formats the current screen into semantic text with styling tags.
#[must_use]
pub fn format_screen(screen: &Screen) -> String {
    let (rows, cols) = screen.size();
    let mut row_strings = Vec::with_capacity(rows as usize);

    for row_idx in 0..rows {
        let mut row_output = String::new();
        format_row(screen, row_idx, cols, &mut row_output);
        row_strings.push(row_output);
    }

    // Trim trailing empty rows so output doesn't fill the context with blank lines
    while row_strings.last().is_some_and(String::is_empty) {
        row_strings.pop();
    }

    row_strings.join("\n")
}

fn format_row(screen: &Screen, row_idx: u16, cols: u16, output: &mut String) {
    let mut current_style = CellStyle::default();
    let mut span_text = String::new();

    // Collect cells up to the last non-empty or styled cell to avoid trailing whitespace
    let last_content_col = find_last_active_col(screen, row_idx, cols);

    for col_idx in 0..last_content_col {
        let Some(cell) = screen.cell(row_idx, col_idx) else {
            continue;
        };

        if cell.is_wide_continuation() {
            continue;
        }

        let cell_style = CellStyle::from_cell(cell);
        let content = if cell.has_contents() {
            cell.contents()
        } else {
            " "
        };

        if cell_style != current_style {
            flush_span(&current_style, &span_text, output);
            span_text.clear();
            current_style = cell_style;
        }

        span_text.push_str(content);
    }

    flush_span(&current_style, &span_text, output);
}

fn find_last_active_col(screen: &Screen, row_idx: u16, cols: u16) -> u16 {
    for col in (0..cols).rev() {
        if let Some(cell) = screen.cell(row_idx, col) {
            let style = CellStyle::from_cell(cell);
            if cell.has_contents() || !style.is_default() {
                return col + 1;
            }
        }
    }
    0
}

fn flush_span(style: &CellStyle, text: &str, output: &mut String) {
    if text.is_empty() {
        return;
    }

    if style.is_default() {
        output.push_str(text);
        return;
    }

    let mut open_tags = Vec::new();
    let mut close_tags = Vec::new();

    append_color_tags(style.fg, "fg", &mut open_tags, &mut close_tags);
    append_color_tags(style.bg, "bg", &mut open_tags, &mut close_tags);

    if style.bold {
        open_tags.push("<bold>".to_string());
        close_tags.push("</bold>".to_string());
    }
    if style.dim {
        open_tags.push("<dim>".to_string());
        close_tags.push("</dim>".to_string());
    }
    if style.italic {
        open_tags.push("<italic>".to_string());
        close_tags.push("</italic>".to_string());
    }
    if style.underline {
        open_tags.push("<underline>".to_string());
        close_tags.push("</underline>".to_string());
    }
    if style.inverse {
        open_tags.push("<inverse>".to_string());
        close_tags.push("</inverse>".to_string());
    }

    for tag in &open_tags {
        output.push_str(tag);
    }
    output.push_str(text);
    for tag in close_tags.iter().rev() {
        output.push_str(tag);
    }
}

fn append_color_tags(
    color: Color,
    prefix: &str,
    open_tags: &mut Vec<String>,
    close_tags: &mut Vec<String>,
) {
    match color {
        Color::Default => {}
        Color::Idx(idx) => {
            let name = named_color(idx);
            let mut tag = String::new();
            if let Some(n) = name {
                let _ = write!(tag, "<{prefix}:{n}>");
            } else {
                let _ = write!(tag, "<{prefix}:idx:{idx}>");
            }
            open_tags.push(tag);
            close_tags.push(format!("</{prefix}>"));
        }
        Color::Rgb(r, g, b) => {
            open_tags.push(format!("<{prefix}:#{r:02x}{g:02x}{b:02x}>"));
            close_tags.push(format!("</{prefix}>"));
        }
    }
}

const fn named_color(idx: u8) -> Option<&'static str> {
    match idx {
        0 => Some("black"),
        1 => Some("red"),
        2 => Some("green"),
        3 => Some("yellow"),
        4 => Some("blue"),
        5 => Some("magenta"),
        6 => Some("cyan"),
        7 => Some("white"),
        8 => Some("bright-black"),
        9 => Some("bright-red"),
        10 => Some("bright-green"),
        11 => Some("bright-yellow"),
        12 => Some("bright-blue"),
        13 => Some("bright-magenta"),
        14 => Some("bright-cyan"),
        15 => Some("bright-white"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_output() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"Hello World");
        let formatted = format_screen(parser.screen());
        assert_eq!(formatted, "Hello World");
    }

    #[test]
    fn test_colors_and_styles() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"\x1b[31;1mError\x1b[0m normal");
        let formatted = format_screen(parser.screen());
        assert_eq!(formatted, "<fg:red><bold>Error</bold></fg> normal");
    }

    #[test]
    fn test_rgb_color() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"\x1b[38;2;255;128;0mOrange\x1b[0m");
        let formatted = format_screen(parser.screen());
        assert_eq!(formatted, "<fg:#ff8000>Orange</fg>");
    }

    #[test]
    fn test_multiline_empty_trailing() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"Line 1\r\nLine 2");
        let formatted = format_screen(parser.screen());
        assert_eq!(formatted, "Line 1\nLine 2");
    }
}
