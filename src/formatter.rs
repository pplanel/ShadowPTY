//! Formats the `alacritty_terminal::term::Term` grid into semantic text preserving color and styling.
//!
//! Rather than stripping colors or dumping raw ANSI escape sequences (which can confuse LLMs),
//! this formatter outputs semantic XML-style tags such as:
//! `<fg:red><bold>Error</bold></fg> <bg:blue>info</bg>`
//!
//! Contiguous cells sharing the same attributes are grouped together to minimize token overhead.


use alacritty_terminal::term::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use alacritty_terminal::index::{Line, Column};
use alacritty_terminal::grid::Dimensions;

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }
}

#[allow(clippy::missing_const_for_fn)]
impl CellStyle {
    #[must_use]
    pub fn from_cell(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            bold: cell.flags.contains(Flags::BOLD),
            dim: cell.flags.contains(Flags::DIM),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.contains(Flags::UNDERLINE),
            inverse: cell.flags.contains(Flags::INVERSE),
        }
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        matches!(self.fg, Color::Named(NamedColor::Foreground))
            && matches!(self.bg, Color::Named(NamedColor::Background))
            && !self.bold
            && !self.dim
            && !self.italic
            && !self.underline
            && !self.inverse
    }
}

/// Formats the current screen into semantic text with styling tags.
#[must_use]
pub fn format_screen(term: &Term<VoidListener>) -> String {
    let grid = term.grid();
    let rows = grid.screen_lines();
    let cols = grid.columns();
    
    let mut row_strings = Vec::with_capacity(rows);

    for row_idx in 0..rows {
        let mut row_output = String::new();
        format_row(term, row_idx, cols, &mut row_output);
        row_strings.push(row_output);
    }

    // Trim trailing empty rows so output doesn't fill the context with blank lines
    while row_strings.last().is_some_and(String::is_empty) {
        row_strings.pop();
    }

    row_strings.join("\n")
}

fn format_row(term: &Term<VoidListener>, row_idx: usize, cols: usize, output: &mut String) {
    let mut current_style = CellStyle::default();
    let mut span_text = String::new();

    let grid = term.grid();
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let row = &grid[Line(row_idx as i32)];

    // Collect cells up to the last non-empty or styled cell to avoid trailing whitespace
    let last_content_col = find_last_active_col(term, row_idx, cols);

    for col_idx in 0..last_content_col {
        let cell = &row[Column(col_idx)];

        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }

        let cell_style = CellStyle::from_cell(cell);
        let content = cell.c; // char

        if cell_style != current_style {
            flush_span(&current_style, &span_text, output);
            span_text.clear();
            current_style = cell_style;
        }

        span_text.push(content);
    }

    flush_span(&current_style, &span_text, output);
}

fn find_last_active_col(term: &Term<VoidListener>, row_idx: usize, cols: usize) -> usize {
    let grid = term.grid();
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let row = &grid[Line(row_idx as i32)];
    
    for col in (0..cols).rev() {
        let cell = &row[Column(col)];
        let style = CellStyle::from_cell(cell);
        // Space with default style is empty.
        if cell.c != ' ' || !style.is_default() {
            return col + 1;
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
        Color::Named(NamedColor::Foreground | NamedColor::Background) => {}
        Color::Named(named) => {
            let name = match named {
                NamedColor::Black => "black",
                NamedColor::Red => "red",
                NamedColor::Green => "green",
                NamedColor::Yellow => "yellow",
                NamedColor::Blue => "blue",
                NamedColor::Magenta => "magenta",
                NamedColor::Cyan => "cyan",
                NamedColor::White => "white",
                NamedColor::BrightBlack => "bright-black",
                NamedColor::BrightRed => "bright-red",
                NamedColor::BrightGreen => "bright-green",
                NamedColor::BrightYellow => "bright-yellow",
                NamedColor::BrightBlue => "bright-blue",
                NamedColor::BrightMagenta => "bright-magenta",
                NamedColor::BrightCyan => "bright-cyan",
                NamedColor::BrightWhite => "bright-white",
                _ => "unknown",
            };
            open_tags.push(format!("<{prefix}:{name}>"));
            close_tags.push(format!("</{prefix}>"));
        }
        Color::Indexed(idx) => {
            open_tags.push(format!("<{prefix}:idx:{idx}>"));
            close_tags.push(format!("</{prefix}>"));
        }
        Color::Spec(rgb) => {
            open_tags.push(format!("<{prefix}:#{:02x}{:02x}{:02x}>", rgb.r, rgb.g, rgb.b));
            close_tags.push(format!("</{prefix}>"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    struct TermSize {
        columns: usize,
        screen_lines: usize,
    }
    
    impl Dimensions for TermSize {
        fn total_lines(&self) -> usize { self.screen_lines }
        fn screen_lines(&self) -> usize { self.screen_lines }
        fn columns(&self) -> usize { self.columns }
    }

    fn new_term(rows: usize, cols: usize) -> Term<VoidListener> {
        let size = TermSize { columns: cols, screen_lines: rows };
        Term::new(Config::default(), &size, VoidListener)
    }

    #[test]
    fn test_plain_output() {
        let mut term = new_term(5, 20);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, b"Hello World");
        let formatted = format_screen(&term);
        assert_eq!(formatted, "Hello World");
    }

    #[test]
    fn test_colors_and_styles() {
        let mut term = new_term(5, 20);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, b"\x1b[31;1mError\x1b[0m normal");
        let formatted = format_screen(&term);
        assert_eq!(formatted, "<fg:red><bold>Error</bold></fg> normal");
    }

    #[test]
    fn test_rgb_color() {
        let mut term = new_term(5, 20);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, b"\x1b[38;2;255;128;0mOrange\x1b[0m");
        let formatted = format_screen(&term);
        assert_eq!(formatted, "<fg:#ff8000>Orange</fg>");
    }

    #[test]
    fn test_multiline_empty_trailing() {
        let mut term = new_term(3, 10);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, b"Line 1\r\nLine 2");
        let formatted = format_screen(&term);
        assert_eq!(formatted, "Line 1\nLine 2");
    }
}
