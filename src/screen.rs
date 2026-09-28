//! Deep Screen module for terminal grid capture and rendering.
//!
//! Captures the visible state of an `alacritty_terminal::term::Term` grid into an
//! immutable, lock-free [`Screen`] in ~200 µs. Provides text rendering adapters
//! ([`Screen::to_tagged_text`], [`Screen::to_plain_text`]) and feeds visual screenshot
//! exporters (SVG, PNG).

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
pub use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, point_to_viewport};
pub use alacritty_terminal::vte::ansi::CursorShape;
use alacritty_terminal::vte::ansi::{Color, NamedColor};

use crate::palette::{Palette, Rgb, TermPalette, Underline, ansi_color_name};

/// Position and shape of the terminal cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenCursor {
    /// 0-indexed row in the visible viewport.
    pub row: u16,
    /// 0-indexed column in the visible viewport.
    pub col: u16,
    /// Shape of the cursor (e.g. Block, Underline, Beam).
    pub shape: CursorShape,
}

/// A single cell on the screen grid with both resolved RGB colors and semantic terminal attributes.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenCell {
    /// Text character plus zero-width combining characters, or empty for spacers.
    pub text: String,
    /// Whether this cell starts a wide (2-column) character.
    pub wide: bool,
    /// Fully resolved foreground RGB color for visual rendering.
    pub fg: Rgb,
    /// Fully resolved background RGB color for visual rendering.
    pub bg: Rgb,
    /// Semantic terminal foreground color (Named, Indexed, Spec).
    pub semantic_fg: Color,
    /// Semantic terminal background color (Named, Indexed, Spec).
    pub semantic_bg: Color,
    /// Bold text attribute.
    pub bold: bool,
    /// Dim text attribute.
    pub dim: bool,
    /// Italic text attribute.
    pub italic: bool,
    /// Underline style.
    pub underline: Underline,
    /// Strikethrough text attribute.
    pub strikethrough: bool,
    /// Hidden text attribute.
    pub hidden: bool,
    /// Inverse (reversed) text attribute.
    pub inverse: bool,
}

impl Default for ScreenCell {
    fn default() -> Self {
        Self {
            text: " ".to_string(),
            wide: false,
            fg: crate::palette::DEFAULT_FOREGROUND,
            bg: crate::palette::DEFAULT_BACKGROUND,
            semantic_fg: Color::Named(NamedColor::Foreground),
            semantic_bg: Color::Named(NamedColor::Background),
            bold: false,
            dim: false,
            italic: false,
            underline: Underline::None,
            strikethrough: false,
            hidden: false,
            inverse: false,
        }
    }
}

impl ScreenCell {
    /// Returns true if this cell has standard unstyled colors and no text attributes.
    #[must_use]
    pub fn is_default_style(&self) -> bool {
        self.semantic_fg == Color::Named(NamedColor::Foreground)
            && self.semantic_bg == Color::Named(NamedColor::Background)
            && !self.bold
            && !self.dim
            && !self.italic
            && self.underline == Underline::None
            && !self.strikethrough
            && !self.hidden
            && !self.inverse
    }

    /// Checks if this cell shares identical text formatting attributes with another.
    #[must_use]
    pub fn style_eq(&self, other: &Self) -> bool {
        self.semantic_fg == other.semantic_fg
            && self.semantic_bg == other.semantic_bg
            && self.bold == other.bold
            && self.dim == other.dim
            && self.italic == other.italic
            && self.underline == other.underline
            && self.strikethrough == other.strikethrough
            && self.hidden == other.hidden
            && self.inverse == other.inverse
    }
}

/// An immutable, lock-free snapshot of the visible terminal grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    /// Number of visible rows.
    pub rows: u16,
    /// Number of visible columns.
    pub cols: u16,
    /// Grid cells in row-major order (`rows * cols`).
    pub cells: Vec<ScreenCell>,
    /// Cursor position and shape if visible.
    pub cursor: Option<ScreenCursor>,
}

impl Screen {
    /// Captures the visible screen grid and cursor using the default palette.
    #[must_use]
    pub fn capture(term: &Term<VoidListener>) -> Self {
        let palette = Palette::default();
        Self::capture_with_palette(term, &palette)
    }

    /// Alias for backwards compatibility with `Snapshot::from_term`.
    #[must_use]
    pub fn from_term(term: &Term<VoidListener>) -> Self {
        Self::capture(term)
    }

    /// Captures the visible screen grid and cursor using a specific palette.
    #[must_use]
    pub fn capture_with_palette(term: &Term<VoidListener>, base_palette: &Palette) -> Self {
        let content = term.renderable_content();
        let grid = term.grid();
        let rows = u16::try_from(grid.screen_lines()).unwrap_or(0);
        let cols = u16::try_from(grid.columns()).unwrap_or(0);
        let display_offset = content.display_offset;

        let total_cells = usize::from(rows) * usize::from(cols);
        let mut cells = Vec::with_capacity(total_cells);
        let palette = TermPalette::new(base_palette, term.colors());

        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let display_offset_i32 = display_offset as i32;

        for row_idx in 0..rows {
            let line = Line(i32::from(row_idx) - display_offset_i32);
            let row = &grid[line];

            for col_idx in 0..cols {
                let cell = &row[Column(usize::from(col_idx))];

                let is_spacer = cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
                let wide = cell.flags.contains(Flags::WIDE_CHAR);

                let text = if is_spacer {
                    String::new()
                } else {
                    let mut s = String::with_capacity(4);
                    s.push(cell.c);
                    if let Some(zerowidth) = cell.zerowidth() {
                        s.extend(zerowidth);
                    }
                    s
                };

                let resolved = palette.resolve_term_cell(cell);

                cells.push(ScreenCell {
                    text,
                    wide,
                    fg: resolved.fg,
                    bg: resolved.bg,
                    semantic_fg: cell.fg,
                    semantic_bg: cell.bg,
                    bold: cell.flags.contains(Flags::BOLD),
                    dim: cell.flags.contains(Flags::DIM),
                    italic: cell.flags.contains(Flags::ITALIC),
                    underline: Underline::from_flags(cell.flags),
                    strikethrough: cell.flags.contains(Flags::STRIKEOUT),
                    hidden: cell.flags.contains(Flags::HIDDEN),
                    inverse: cell.flags.contains(Flags::INVERSE),
                });
            }
        }

        let cursor = if content.cursor.shape == CursorShape::Hidden {
            None
        } else {
            point_to_viewport(display_offset, content.cursor.point).and_then(|pt| {
                let r = u16::try_from(pt.line).ok()?;
                let c = u16::try_from(pt.column.0).ok()?;
                if r < rows && c < cols {
                    Some(ScreenCursor {
                        row: r,
                        col: c,
                        shape: content.cursor.shape,
                    })
                } else {
                    None
                }
            })
        };

        Self {
            rows,
            cols,
            cells,
            cursor,
        }
    }

    /// Alias for backwards compatibility with `Snapshot::from_term_with_palette`.
    #[must_use]
    pub fn from_term_with_palette(term: &Term<VoidListener>, base_palette: &Palette) -> Self {
        Self::capture_with_palette(term, base_palette)
    }

    /// Retrieves the cell at the given 0-indexed `(row, col)`.
    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<&ScreenCell> {
        if row < self.rows && col < self.cols {
            let idx = usize::from(row) * usize::from(self.cols) + usize::from(col);
            self.cells.get(idx)
        } else {
            None
        }
    }

    /// Renders the screen as formatted text with semantic XML-like styling tags.
    ///
    /// Groups contiguous cells sharing attributes to minimize tokens, and trims trailing
    /// whitespace and trailing empty rows.
    #[must_use]
    pub fn to_tagged_text(&self) -> String {
        let mut row_strings = Vec::with_capacity(usize::from(self.rows));

        for row_idx in 0..self.rows {
            let mut row_output = String::new();
            self.format_row(row_idx, &mut row_output);
            row_strings.push(row_output);
        }

        while row_strings.last().is_some_and(String::is_empty) {
            row_strings.pop();
        }

        row_strings.join("\n")
    }

    /// Renders the screen as clean, unstyled plain text for pattern matching.
    ///
    /// Trims trailing whitespace on each row and trailing empty rows.
    #[must_use]
    pub fn to_plain_text(&self) -> String {
        let mut row_strings = Vec::with_capacity(usize::from(self.rows));

        for row_idx in 0..self.rows {
            let mut text = String::with_capacity(usize::from(self.cols));
            for col_idx in 0..self.cols {
                if let Some(cell) = self.cell(row_idx, col_idx)
                    && !cell.text.is_empty()
                {
                    text.push_str(&cell.text);
                }
            }
            row_strings.push(text.trim_end().to_string());
        }

        while row_strings.last().is_some_and(String::is_empty) {
            row_strings.pop();
        }

        row_strings.join("\n")
    }

    fn find_last_active_col(&self, row_idx: u16) -> u16 {
        for col in (0..self.cols).rev() {
            if let Some(cell) = self.cell(row_idx, col)
                && (cell.text != " " || !cell.is_default_style())
            {
                return col + 1;
            }
        }
        0
    }

    fn format_row(&self, row_idx: u16, output: &mut String) {
        let last_active = self.find_last_active_col(row_idx);
        let mut current_cell: Option<&ScreenCell> = None;
        let mut span_text = String::new();

        for col_idx in 0..last_active {
            let Some(cell) = self.cell(row_idx, col_idx) else {
                continue;
            };

            // Skip wide spacer cells
            if cell.text.is_empty() {
                continue;
            }

            if let Some(cur) = current_cell {
                if !cur.style_eq(cell) {
                    flush_span(cur, &span_text, output);
                    span_text.clear();
                    current_cell = Some(cell);
                }
            } else {
                current_cell = Some(cell);
            }

            span_text.push_str(&cell.text);
        }

        if let Some(cur) = current_cell {
            flush_span(cur, &span_text, output);
        }
    }
}

fn flush_span(cell: &ScreenCell, text: &str, output: &mut String) {
    if text.is_empty() {
        return;
    }

    if cell.is_default_style() {
        output.push_str(text);
        return;
    }

    let mut open_tags = Vec::new();
    let mut close_tags = Vec::new();

    append_color_tags(cell.semantic_fg, "fg", &mut open_tags, &mut close_tags);
    append_color_tags(cell.semantic_bg, "bg", &mut open_tags, &mut close_tags);

    if cell.bold {
        open_tags.push("<bold>".to_string());
        close_tags.push("</bold>".to_string());
    }
    if cell.dim {
        open_tags.push("<dim>".to_string());
        close_tags.push("</dim>".to_string());
    }
    if cell.italic {
        open_tags.push("<italic>".to_string());
        close_tags.push("</italic>".to_string());
    }
    match cell.underline {
        Underline::None => {}
        Underline::Single => {
            open_tags.push("<underline>".to_string());
            close_tags.push("</underline>".to_string());
        }
        Underline::Double => {
            open_tags.push("<underline:double>".to_string());
            close_tags.push("</underline:double>".to_string());
        }
        Underline::Curly => {
            open_tags.push("<underline:curly>".to_string());
            close_tags.push("</underline:curly>".to_string());
        }
        Underline::Dotted => {
            open_tags.push("<underline:dotted>".to_string());
            close_tags.push("</underline:dotted>".to_string());
        }
        Underline::Dashed => {
            open_tags.push("<underline:dashed>".to_string());
            close_tags.push("</underline:dashed>".to_string());
        }
    }
    if cell.strikethrough {
        open_tags.push("<strikethrough>".to_string());
        close_tags.push("</strikethrough>".to_string());
    }
    if cell.hidden {
        open_tags.push("<hidden>".to_string());
        close_tags.push("</hidden>".to_string());
    }
    if cell.inverse {
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
            let name = ansi_color_name(named as usize).unwrap_or("unknown");
            open_tags.push(format!("<{prefix}:{name}>"));
            close_tags.push(format!("</{prefix}>"));
        }
        Color::Indexed(idx) => {
            if let Some(name) = ansi_color_name(usize::from(idx)) {
                open_tags.push(format!("<{prefix}:{name}>"));
            } else {
                open_tags.push(format!("<{prefix}:idx:{idx}>"));
            }
            close_tags.push(format!("</{prefix}>"));
        }
        Color::Spec(rgb) => {
            let r = rgb.r;
            let g = rgb.g;
            let b = rgb.b;
            open_tags.push(format!("<{prefix}:#{r:02x}{g:02x}{b:02x}>"));
            close_tags.push(format!("</{prefix}>"));
        }
    }
}

// ---------------------------------------------------------------------------
// Compatibility aliases and functions
// ---------------------------------------------------------------------------

/// Type alias for [`Screen`].
pub type Snapshot = Screen;

/// Type alias for [`ScreenCell`].
pub type SnapCell = ScreenCell;

/// Type alias for [`ScreenCursor`].
pub type SnapCursor = ScreenCursor;

/// Captures and formats the current screen into semantic text with styling tags.
#[must_use]
pub fn format_screen(term: &Term<VoidListener>) -> String {
    Screen::capture(term).to_tagged_text()
}

/// Captures and renders the visible screen as plain text, without style tags.
#[must_use]
pub fn screen_text(term: &Term<VoidListener>) -> String {
    Screen::capture(term).to_plain_text()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::palette::rgb;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    struct TestSize {
        cols: usize,
        rows: usize,
    }

    impl Dimensions for TestSize {
        fn total_lines(&self) -> usize {
            self.rows
        }
        fn screen_lines(&self) -> usize {
            self.rows
        }
        fn columns(&self) -> usize {
            self.cols
        }
    }

    fn create_test_term(cols: usize, rows: usize) -> Term<VoidListener> {
        let size = TestSize { cols, rows };
        Term::new(Config::default(), &size, VoidListener)
    }

    fn feed_input(term: &mut Term<VoidListener>, input: &[u8]) {
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(term, input);
    }

    #[test]
    fn test_plain_output() {
        let mut term = create_test_term(80, 24);
        feed_input(&mut term, b"Hello World");
        assert_eq!(format_screen(&term), "Hello World");
        assert_eq!(screen_text(&term), "Hello World");
    }

    #[test]
    fn test_colors_and_styles() {
        let mut term = create_test_term(80, 24);
        // Red foreground (31), Bold (1), Blue background (44)
        feed_input(&mut term, b"\x1b[31;1;44mError\x1b[0m Normal");
        let formatted = format_screen(&term);
        assert!(formatted.contains("<fg:red>"));
        assert!(formatted.contains("<bg:blue>"));
        assert!(formatted.contains("<bold>"));
        assert!(formatted.contains("Error"));
        assert!(formatted.contains(" Normal"));
        assert_eq!(screen_text(&term), "Error Normal");
    }

    #[test]
    fn test_indexed_ansi_colors_use_names() {
        let mut term = create_test_term(80, 24);
        // 38;5;1 is red via indexed color
        feed_input(&mut term, b"\x1b[38;5;1mRedText\x1b[0m");
        let formatted = format_screen(&term);
        assert!(formatted.contains("<fg:red>RedText</fg>"));
    }

    #[test]
    fn test_rgb_color() {
        let mut term = create_test_term(80, 24);
        // 38;2;255;128;0m (TrueColor Orange)
        feed_input(&mut term, b"\x1b[38;2;255;128;0mOrange\x1b[0m");
        let formatted = format_screen(&term);
        assert!(formatted.contains("<fg:#ff8000>Orange</fg>"));
    }

    #[test]
    fn test_combining_characters() {
        let mut term = create_test_term(80, 24);
        // 'e' + acute accent combining mark (U+0301)
        feed_input(&mut term, "e\u{0301}".as_bytes());
        let formatted = format_screen(&term);
        assert_eq!(formatted, "e\u{0301}");
        assert_eq!(screen_text(&term), "e\u{0301}");
    }

    #[test]
    fn test_strikethrough_and_hidden() {
        let mut term = create_test_term(80, 24);
        feed_input(&mut term, b"\x1b[9mCrossed\x1b[29m \x1b[8mSecret\x1b[28m");
        let formatted = format_screen(&term);
        assert!(formatted.contains("<strikethrough>Crossed</strikethrough>"));
        assert!(formatted.contains("<hidden>Secret</hidden>"));
    }

    #[test]
    fn test_underline_styles() {
        let mut term = create_test_term(80, 24);
        feed_input(
            &mut term,
            b"\x1b[4:1mSingle\x1b[4:0m \x1b[4:2mDouble\x1b[4:0m \x1b[4:3mCurly\x1b[4:0m",
        );
        let formatted = format_screen(&term);
        assert!(formatted.contains("<underline>Single</underline>"));
        assert!(formatted.contains("<underline:double>Double</underline:double>"));
        assert!(formatted.contains("<underline:curly>Curly</underline:curly>"));
    }

    #[test]
    fn test_multiline_empty_trailing() {
        let mut term = create_test_term(80, 24);
        feed_input(&mut term, b"Line 1\r\n\r\nLine 3\r\n\r\n\r\n");
        let formatted = format_screen(&term);
        assert_eq!(formatted, "Line 1\n\nLine 3");
    }

    #[test]
    fn test_screen_text_has_no_tags_and_reflects_overwrites() {
        let mut term = create_test_term(80, 24);
        feed_input(&mut term, b"\x1b[31;1mhello\x1b[0m world");
        assert_eq!(screen_text(&term), "hello world");

        // Move to column 1 and overwrite "hello" with "HELLO"
        feed_input(&mut term, b"\rHELLO");
        assert_eq!(screen_text(&term), "HELLO world");
    }

    #[test]
    fn test_snapshot_dimensions_and_cells_count() {
        let term = create_test_term(80, 24);
        let screen = Screen::capture(&term);
        assert_eq!(screen.rows, 24);
        assert_eq!(screen.cols, 80);
        assert_eq!(screen.cells.len(), 80 * 24);
    }

    #[test]
    fn test_snapshot_colors_named_indexed_truecolor() {
        let mut term = create_test_term(80, 24);
        feed_input(&mut term, b"\x1b[31mA\x1b[38;5;2mB\x1b[38;2;10;20;30mC");
        let screen = Screen::capture(&term);

        let cell_a = screen.cell(0, 0).expect("cell 0,0");
        assert_eq!(cell_a.text, "A");
        assert_eq!(cell_a.fg, rgb(0xcd, 0x00, 0x00)); // Standard red

        let cell_b = screen.cell(0, 1).expect("cell 0,1");
        assert_eq!(cell_b.text, "B");
        assert_eq!(cell_b.fg, rgb(0x00, 0xcd, 0x00)); // Standard green

        let cell_c = screen.cell(0, 2).expect("cell 0,2");
        assert_eq!(cell_c.text, "C");
        assert_eq!(cell_c.fg, rgb(10, 20, 30)); // Truecolor
    }

    #[test]
    fn test_snapshot_dim_inverse_hidden_styles() {
        let mut term = create_test_term(80, 24);
        feed_input(
            &mut term,
            b"\x1b[2;31mD\x1b[0m\x1b[7;31;42mI\x1b[0m\x1b[8;31mH\x1b[0m",
        );
        let screen = Screen::capture(&term);

        // Dim red
        let cell_d = screen.cell(0, 0).expect("cell D");
        assert_eq!(cell_d.text, "D");
        assert_eq!(cell_d.fg, rgb(0x87, 0x00, 0x00));

        // Inverse (fg=31, bg=42 -> swapped)
        let cell_i = screen.cell(0, 1).expect("cell I");
        assert_eq!(cell_i.text, "I");
        assert_eq!(cell_i.fg, rgb(0x00, 0xcd, 0x00)); // was bg
        assert_eq!(cell_i.bg, rgb(0xcd, 0x00, 0x00)); // was fg

        // Hidden (fg becomes bg)
        let cell_h = screen.cell(0, 2).expect("cell H");
        assert_eq!(cell_h.text, "H");
        assert_eq!(cell_h.fg, cell_h.bg);
    }

    #[test]
    fn test_snapshot_osc_overrides() {
        let mut term = create_test_term(80, 24);
        feed_input(&mut term, b"\x1b]11;#112233\x07\x1b]10;#445566\x07Z");
        let screen = Screen::capture(&term);

        let cell = screen.cell(0, 0).expect("cell Z");
        assert_eq!(cell.text, "Z");
        assert_eq!(cell.fg, rgb(0x44, 0x55, 0x66));
        assert_eq!(cell.bg, rgb(0x11, 0x22, 0x33));
    }

    #[test]
    fn test_snapshot_wide_char_and_spacers() {
        let mut term = create_test_term(80, 24);
        feed_input(&mut term, "🦀".as_bytes()); // Crab is wide (2 cols)
        let screen = Screen::capture(&term);

        let c0 = screen.cell(0, 0).expect("cell 0");
        assert_eq!(c0.text, "🦀");
        assert!(c0.wide);

        let c1 = screen.cell(0, 1).expect("cell 1");
        assert_eq!(c1.text, ""); // Spacer cell
    }

    #[test]
    fn test_snapshot_cursor_position_and_shapes() {
        let mut term = create_test_term(80, 24);
        feed_input(&mut term, b"\x1b[5;10H\x1b[3 q");
        let screen = Screen::capture(&term);

        let cursor = screen.cursor.expect("cursor present");
        assert_eq!(cursor.row, 4); // 0-indexed row (5 - 1)
        assert_eq!(cursor.col, 9); // 0-indexed col (10 - 1)
        assert_eq!(cursor.shape, CursorShape::Underline);
    }

    #[test]
    fn test_snapshot_cursor_hidden_dectcem() {
        let mut term = create_test_term(80, 24);
        feed_input(&mut term, b"\x1b[?25l"); // Hide cursor
        let screen = Screen::capture(&term);
        assert!(screen.cursor.is_none());
    }
}
