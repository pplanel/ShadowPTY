//! Screen snapshot extraction from `alacritty_terminal`.
//!
//! Provides a detached representation of the terminal screen.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, point_to_viewport};
pub use alacritty_terminal::vte::ansi::CursorShape;

use crate::palette::{Palette, Rgb, TermPalette, Underline};

/// Detached snapshot of a terminal grid at a specific point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Number of visible rows (screen lines).
    pub rows: u16,
    /// Number of visible columns.
    pub cols: u16,
    /// Grid cells in row-major order (`rows * cols`).
    pub cells: Vec<SnapCell>,
    /// Cursor position and shape if visible.
    pub cursor: Option<SnapCursor>,
}

/// Resolved representation of a single terminal cell.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapCell {
    /// Text character plus zero-width characters, or empty for spacers.
    pub text: String,
    /// Whether this cell starts a wide (2-column) character.
    pub wide: bool,
    /// Fully resolved foreground color.
    pub fg: Rgb,
    /// Fully resolved background color.
    pub bg: Rgb,
    /// Bold text attribute.
    pub bold: bool,
    /// Italic text attribute.
    pub italic: bool,
    /// Underline style.
    pub underline: Underline,
    /// Strikethrough text attribute.
    pub strikethrough: bool,
}

/// Position and shape of the terminal cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapCursor {
    /// 0-indexed row in the visible viewport.
    pub row: u16,
    /// 0-indexed column in the visible viewport.
    pub col: u16,
    /// Shape of the cursor (e.g. Block, Underline, Beam).
    pub shape: CursorShape,
}

impl Snapshot {
    /// Captures the visible screen grid and cursor using the default palette.
    #[must_use]
    pub fn from_term(term: &Term<VoidListener>) -> Self {
        let palette = Palette::default();
        Self::from_term_with_palette(term, &palette)
    }

    /// Captures the visible screen grid and cursor using a specific palette.
    #[must_use]
    pub fn from_term_with_palette(term: &Term<VoidListener>, base_palette: &Palette) -> Self {
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

                cells.push(SnapCell {
                    text,
                    wide,
                    fg: resolved.fg,
                    bg: resolved.bg,
                    bold: cell.flags.contains(Flags::BOLD),
                    italic: cell.flags.contains(Flags::ITALIC),
                    underline: Underline::from_flags(cell.flags),
                    strikethrough: cell.flags.contains(Flags::STRIKEOUT),
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
                    Some(SnapCursor {
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

    /// Retrieves the cell at the given 0-indexed `(row, col)`.
    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<&SnapCell> {
        if row < self.rows && col < self.cols {
            let idx = usize::from(row) * usize::from(self.cols) + usize::from(col);
            self.cells.get(idx)
        } else {
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::palette::{DEFAULT_BACKGROUND, rgb};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{NamedColor, Processor, StdSyncHandler};

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

    fn new_term(rows: usize, cols: usize) -> Term<VoidListener> {
        Term::new(Config::default(), &TestSize { cols, rows }, VoidListener)
    }

    #[test]
    fn test_snapshot_dimensions_and_cells_count() {
        let mut term = new_term(3, 5);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, b"Hi");

        let snap = Snapshot::from_term(&term);
        assert_eq!(snap.rows, 3);
        assert_eq!(snap.cols, 5);
        assert_eq!(snap.cells.len(), 15);

        assert_eq!(snap.cell(0, 0).map(|c| c.text.as_str()), Some("H"));
        assert_eq!(snap.cell(0, 1).map(|c| c.text.as_str()), Some("i"));
        assert_eq!(snap.cell(0, 2).map(|c| c.text.as_str()), Some(" "));
        assert_eq!(snap.cell(10, 10), None);
    }

    #[test]
    fn test_snapshot_colors_named_indexed_truecolor() {
        let mut term = new_term(2, 10);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(
            &mut term,
            b"\x1b[31mA\x1b[38;5;196mB\x1b[38;2;12;34;56mC\x1b[0m",
        );

        let snap = Snapshot::from_term(&term);
        let cell_a = snap.cell(0, 0).expect("cell A");
        let cell_b = snap.cell(0, 1).expect("cell B");
        let cell_c = snap.cell(0, 2).expect("cell C");

        assert_eq!(cell_a.text, "A");
        assert_eq!(cell_a.fg, rgb(0xcd, 0x00, 0x00));
        assert_eq!(cell_a.bg, DEFAULT_BACKGROUND);

        assert_eq!(cell_b.text, "B");
        assert_eq!(cell_b.fg, rgb(255, 0, 0));

        assert_eq!(cell_c.text, "C");
        assert_eq!(cell_c.fg, rgb(12, 34, 56));
    }

    #[test]
    fn test_snapshot_osc_overrides() {
        let mut term = new_term(2, 10);
        let mut parser: Processor<StdSyncHandler> = Processor::new();

        // Override OSC 10 (fg), OSC 11 (bg), and OSC 4 (color 1 = red)
        parser.advance(
            &mut term,
            b"\x1b]10;#aabbcc\x1b\\\x1b]11;#443322\x1b\\\x1b]4;1;#112233\x1b\\",
        );
        parser.advance(&mut term, b"\x1b[31mX\x1b[0m Y");

        let snap = Snapshot::from_term(&term);
        let cell_x = snap.cell(0, 0).expect("cell X");
        let cell_y = snap.cell(0, 2).expect("cell Y");

        assert_eq!(cell_x.text, "X");
        assert_eq!(cell_x.fg, rgb(0x11, 0x22, 0x33));
        assert_eq!(cell_x.bg, rgb(0x44, 0x33, 0x22));

        assert_eq!(cell_y.text, "Y");
        assert_eq!(cell_y.fg, rgb(0xaa, 0xbb, 0xcc));
        assert_eq!(cell_y.bg, rgb(0x44, 0x33, 0x22));
    }

    #[test]
    fn test_snapshot_dim_inverse_hidden_styles() {
        let mut term = new_term(2, 10);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        // Red fg (31), Green bg (42), Dim (2), Inverse (7), Hidden (8)
        parser.advance(&mut term, b"\x1b[31;42;2mA\x1b[7mB\x1b[8mC\x1b[0m");

        let snap = Snapshot::from_term(&term);
        let palette = Palette::default();
        let green = palette.base().named(NamedColor::Green);
        let expected_dim_r = u8::try_from((0xcdu32 * 66) / 100).expect("valid u8");

        let cell_a = snap.cell(0, 0).expect("cell A");
        assert_eq!(cell_a.fg, rgb(expected_dim_r, 0, 0));
        assert_eq!(cell_a.bg, green);

        let cell_b = snap.cell(0, 1).expect("cell B");
        // Inverse swapped dimmed red (now bg) and green (now fg)
        assert_eq!(cell_b.fg, green);
        assert_eq!(cell_b.bg, rgb(expected_dim_r, 0, 0));

        let cell_c = snap.cell(0, 2).expect("cell C");
        // Hidden sets fg = bg
        assert_eq!(cell_c.fg, cell_c.bg);
    }

    #[test]
    fn test_snapshot_wide_char_and_spacers() {
        let mut term = new_term(2, 10);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, "你好".as_bytes());

        let snap = Snapshot::from_term(&term);
        let cell_0 = snap.cell(0, 0).expect("cell 0");
        let cell_1 = snap.cell(0, 1).expect("cell 1");
        let cell_2 = snap.cell(0, 2).expect("cell 2");
        let cell_3 = snap.cell(0, 3).expect("cell 3");

        assert_eq!(cell_0.text, "你");
        assert!(cell_0.wide);

        assert_eq!(cell_1.text, "");
        assert!(!cell_1.wide);

        assert_eq!(cell_2.text, "好");
        assert!(cell_2.wide);

        assert_eq!(cell_3.text, "");
        assert!(!cell_3.wide);
    }

    #[test]
    fn test_snapshot_combining_characters() {
        let mut term = new_term(2, 10);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, "cafe\u{301} ok".as_bytes());

        let snap = Snapshot::from_term(&term);
        let cell_e = snap.cell(0, 3).expect("cell e with acute");
        assert_eq!(cell_e.text, "e\u{0301}");
    }

    #[test]
    fn test_snapshot_cursor_position_and_shapes() {
        let mut term = new_term(3, 10);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, b"hello");

        let snap = Snapshot::from_term(&term);
        assert_eq!(
            snap.cursor,
            Some(SnapCursor {
                row: 0,
                col: 5,
                shape: CursorShape::Block,
            })
        );

        // Switch to underline cursor: \x1b[4 q (steady underline)
        parser.advance(&mut term, b"\x1b[4 q");
        let snap2 = Snapshot::from_term(&term);
        assert_eq!(
            snap2.cursor,
            Some(SnapCursor {
                row: 0,
                col: 5,
                shape: CursorShape::Underline,
            })
        );
    }

    #[test]
    fn test_snapshot_cursor_hidden_dectcem() {
        let mut term = new_term(2, 10);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        parser.advance(&mut term, b"hello");

        let snap_visible = Snapshot::from_term(&term);
        assert!(snap_visible.cursor.is_some());

        // Hide cursor via DECTCEM off: \x1b[?25l
        parser.advance(&mut term, b"\x1b[?25l");
        let snap_hidden = Snapshot::from_term(&term);
        assert_eq!(snap_hidden.cursor, None);

        // Restore cursor via DECTCEM on: \x1b[?25h
        parser.advance(&mut term, b"\x1b[?25h");
        let snap_restored = Snapshot::from_term(&term);
        assert!(snap_restored.cursor.is_some());
    }
}
