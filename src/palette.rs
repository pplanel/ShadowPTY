//! Terminal color palette and resolution according to ANSI and `XTerm` standards.
//!
//! Provides the base 16 ANSI colors, the 6×6×6 color cube (indices 16–231),
//! the 24-step grayscale ramp (indices 232–255), default canvas colors,
//! and resolution of dynamic app overrides (OSC 4, OSC 10, OSC 11, OSC 12).

pub use alacritty_terminal::term::cell::{Cell, Flags};
pub use alacritty_terminal::term::color::Colors;
pub use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// Creates an [`Rgb`] color from individual red, green, and blue components.
#[must_use]
pub const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

/// Formats an [`Rgb`] value as a lowercase hex string (e.g. `"#ff00aa"`).
#[must_use]
pub fn to_hex(color: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

/// Scales an [`Rgb`] color by a floating-point multiplier, clamping each channel to 0..=255.
#[must_use]
pub fn scale_rgb(color: Rgb, factor: f32) -> Rgb {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Rgb {
        r: (f32::from(color.r) * factor).round().clamp(0.0, 255.0) as u8,
        g: (f32::from(color.g) * factor).round().clamp(0.0, 255.0) as u8,
        b: (f32::from(color.b) * factor).round().clamp(0.0, 255.0) as u8,
    }
}

/// Scales an [`Rgb`] color by an integer percentage (e.g. 66 for 66%).
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub const fn scale_rgb_const(color: Rgb, percent: u32) -> Rgb {
    Rgb {
        r: ((color.r as u32 * percent) / 100) as u8,
        g: ((color.g as u32 * percent) / 100) as u8,
        b: ((color.b as u32 * percent) / 100) as u8,
    }
}

/// Default foreground color for the terminal canvas (`#d8d8d8`).
pub const DEFAULT_FOREGROUND: Rgb = rgb(0xd8, 0xd8, 0xd8);

/// Default background color for the terminal canvas (`#181818`).
pub const DEFAULT_BACKGROUND: Rgb = rgb(0x18, 0x18, 0x18);

/// Default cursor color (`#ffffff`).
pub const DEFAULT_CURSOR: Rgb = rgb(0xff, 0xff, 0xff);

/// Names of the 16 ANSI colors, indexed matching `NamedColor` discriminants and 256-color indices `0..=15`.
pub const ANSI_COLOR_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bright-black",
    "bright-red",
    "bright-green",
    "bright-yellow",
    "bright-blue",
    "bright-magenta",
    "bright-cyan",
    "bright-white",
];

/// Returns the semantic color name for a given ANSI color index (`0..=15`).
#[must_use]
pub const fn ansi_color_name(index: usize) -> Option<&'static str> {
    if index < ANSI_COLOR_NAMES.len() {
        Some(ANSI_COLOR_NAMES[index])
    } else {
        None
    }
}

/// Constructs the default 256-color table at compile time.
const fn make_base_256_colors() -> [Rgb; 256] {
    let mut table = [Rgb { r: 0, g: 0, b: 0 }; 256];

    // Standard 16 ANSI colors
    table[0] = rgb(0x00, 0x00, 0x00); // Black
    table[1] = rgb(0xcd, 0x00, 0x00); // Red
    table[2] = rgb(0x00, 0xcd, 0x00); // Green
    table[3] = rgb(0xcd, 0xcd, 0x00); // Yellow
    table[4] = rgb(0x00, 0x00, 0xee); // Blue
    table[5] = rgb(0xcd, 0x00, 0xcd); // Magenta
    table[6] = rgb(0x00, 0xcd, 0xcd); // Cyan
    table[7] = rgb(0xe5, 0xe5, 0xe5); // White
    table[8] = rgb(0x7f, 0x7f, 0x7f); // Bright Black (Gray)
    table[9] = rgb(0xff, 0x00, 0x00); // Bright Red
    table[10] = rgb(0x00, 0xff, 0x00); // Bright Green
    table[11] = rgb(0xff, 0xff, 0x00); // Bright Yellow
    table[12] = rgb(0x5c, 0x5c, 0xff); // Bright Blue
    table[13] = rgb(0xff, 0x00, 0xff); // Bright Magenta
    table[14] = rgb(0x00, 0xff, 0xff); // Bright Cyan
    table[15] = rgb(0xff, 0xff, 0xff); // Bright White

    // 16..232: 6x6x6 color cube
    let mut index = 16;
    let mut r = 0;
    while r < 6 {
        let mut g = 0;
        while g < 6 {
            let mut b = 0;
            while b < 6 {
                let r_val = if r == 0 { 0 } else { r * 40 + 55 };
                let g_val = if g == 0 { 0 } else { g * 40 + 55 };
                let b_val = if b == 0 { 0 } else { b * 40 + 55 };
                table[index] = rgb(r_val, g_val, b_val);
                index += 1;
                b += 1;
            }
            g += 1;
        }
        r += 1;
    }

    // 232..256: 24-step grayscale ramp
    let mut i: u8 = 0;
    while i < 24 {
        let val = i * 10 + 8;
        table[index] = rgb(val, val, val);
        index += 1;
        i += 1;
    }

    table
}

/// The compile-time base 256-color table.
pub static BASE_256_COLORS: [Rgb; 256] = make_base_256_colors();

/// The base color palette containing 256 indexed colors and default canvas colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasePalette {
    pub indexed: [Rgb; 256],
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
}

impl Default for BasePalette {
    fn default() -> Self {
        Self {
            indexed: BASE_256_COLORS,
            foreground: DEFAULT_FOREGROUND,
            background: DEFAULT_BACKGROUND,
            cursor: DEFAULT_CURSOR,
        }
    }
}

impl BasePalette {
    /// Resolves a [`NamedColor`] to its concrete [`Rgb`] value.
    #[must_use]
    pub const fn named(&self, named: NamedColor) -> Rgb {
        match named {
            NamedColor::Black => self.indexed[0],
            NamedColor::Red => self.indexed[1],
            NamedColor::Green => self.indexed[2],
            NamedColor::Yellow => self.indexed[3],
            NamedColor::Blue => self.indexed[4],
            NamedColor::Magenta => self.indexed[5],
            NamedColor::Cyan => self.indexed[6],
            NamedColor::White => self.indexed[7],
            NamedColor::BrightBlack => self.indexed[8],
            NamedColor::BrightRed => self.indexed[9],
            NamedColor::BrightGreen => self.indexed[10],
            NamedColor::BrightYellow => self.indexed[11],
            NamedColor::BrightBlue => self.indexed[12],
            NamedColor::BrightMagenta => self.indexed[13],
            NamedColor::BrightCyan => self.indexed[14],
            NamedColor::BrightWhite => self.indexed[15],
            NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
            NamedColor::Background => self.background,
            NamedColor::Cursor => self.cursor,
            NamedColor::DimBlack => scale_rgb_const(self.indexed[0], 66),
            NamedColor::DimRed => scale_rgb_const(self.indexed[1], 66),
            NamedColor::DimGreen => scale_rgb_const(self.indexed[2], 66),
            NamedColor::DimYellow => scale_rgb_const(self.indexed[3], 66),
            NamedColor::DimBlue => scale_rgb_const(self.indexed[4], 66),
            NamedColor::DimMagenta => scale_rgb_const(self.indexed[5], 66),
            NamedColor::DimCyan => scale_rgb_const(self.indexed[6], 66),
            NamedColor::DimWhite => scale_rgb_const(self.indexed[7], 66),
            NamedColor::DimForeground => scale_rgb_const(self.foreground, 66),
        }
    }

    /// Resolves an indexed color (`0..=255`) to its concrete [`Rgb`] value.
    #[must_use]
    pub const fn indexed(&self, index: u8) -> Rgb {
        self.indexed[index as usize]
    }

    /// Resolves any [`Color`] (Named, Indexed, or Spec) to [`Rgb`].
    #[must_use]
    pub const fn to_rgb(&self, color: Color) -> Rgb {
        match color {
            Color::Named(named) => self.named(named),
            Color::Indexed(idx) => self.indexed(idx),
            Color::Spec(rgb) => rgb,
        }
    }
}

const fn base_color_for_dim(dim: NamedColor) -> Option<NamedColor> {
    match dim {
        NamedColor::DimBlack => Some(NamedColor::Black),
        NamedColor::DimRed => Some(NamedColor::Red),
        NamedColor::DimGreen => Some(NamedColor::Green),
        NamedColor::DimYellow => Some(NamedColor::Yellow),
        NamedColor::DimBlue => Some(NamedColor::Blue),
        NamedColor::DimMagenta => Some(NamedColor::Magenta),
        NamedColor::DimCyan => Some(NamedColor::Cyan),
        NamedColor::DimWhite => Some(NamedColor::White),
        NamedColor::DimForeground => Some(NamedColor::Foreground),
        _ => None,
    }
}

/// Terminal text underline style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Underline {
    /// No underline.
    #[default]
    None,
    /// Standard single underline (`SGR 4`).
    Single,
    /// Double underline (`SGR 4:2`).
    Double,
    /// Curly underline / wave (`SGR 4:3`).
    Curly,
    /// Dotted underline (`SGR 4:4`).
    Dotted,
    /// Dashed underline (`SGR 4:5`).
    Dashed,
}

impl Underline {
    /// Extracts the [`Underline`] style from cell [`Flags`].
    #[must_use]
    pub const fn from_flags(flags: Flags) -> Self {
        if flags.contains(Flags::UNDERCURL) {
            Self::Curly
        } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
            Self::Double
        } else if flags.contains(Flags::DOTTED_UNDERLINE) {
            Self::Dotted
        } else if flags.contains(Flags::DASHED_UNDERLINE) {
            Self::Dashed
        } else if flags.contains(Flags::UNDERLINE) {
            Self::Single
        } else {
            Self::None
        }
    }
}

/// Request parameters for resolving a cell's effective colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellColorRequest {
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
}

impl CellColorRequest {
    /// Creates a new cell color request.
    #[must_use]
    pub const fn new(fg: Color, bg: Color, flags: Flags) -> Self {
        Self { fg, bg, flags }
    }

    /// Creates a request directly from a terminal [`Cell`].
    #[must_use]
    pub const fn from_cell(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags,
        }
    }
}

/// Resolved foreground and background RGB colors for a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedCellColors {
    pub fg: Rgb,
    pub bg: Rgb,
}

/// Terminal color resolver combining a [`BasePalette`] with dynamic app overrides.
///
/// Handles entries set by applications via OSC 4 (indexed colors), OSC 10 (text foreground),
/// OSC 11 (text background), and OSC 12 (text cursor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Palette {
    base: BasePalette,
}

impl Palette {
    /// Creates a new resolver with the specified base palette.
    #[must_use]
    pub const fn new(base: BasePalette) -> Self {
        Self { base }
    }

    /// Returns a reference to the underlying base palette.
    #[must_use]
    pub const fn base(&self) -> &BasePalette {
        &self.base
    }

    /// Resolves a [`NamedColor`] taking into account any overrides in `overrides`.
    #[must_use]
    pub fn resolve_named(&self, named: NamedColor, overrides: Option<&Colors>) -> Rgb {
        if let Some(colors) = overrides {
            if let Some(rgb) = colors[named] {
                return rgb;
            }
            if let Some(base_rgb) = base_color_for_dim(named).and_then(|b| colors[b]) {
                return scale_rgb_const(base_rgb, 66);
            }
        }
        self.base.named(named)
    }

    /// Resolves an indexed color (`0..=255`) taking into account any overrides in `overrides`.
    #[must_use]
    pub fn resolve_indexed(&self, index: u8, overrides: Option<&Colors>) -> Rgb {
        if let Some(rgb) = overrides.and_then(|c| c[index as usize]) {
            return rgb;
        }
        self.base.indexed(index)
    }

    /// Resolves any [`Color`] (Named, Indexed, or Spec) taking into account any overrides in `overrides`.
    #[must_use]
    pub fn resolve_color(&self, color: Color, overrides: Option<&Colors>) -> Rgb {
        match color {
            Color::Named(named) => self.resolve_named(named, overrides),
            Color::Indexed(idx) => self.resolve_indexed(idx, overrides),
            Color::Spec(rgb) => rgb,
        }
    }

    /// Resolves the effective background color.
    #[must_use]
    pub fn resolve_background(&self, overrides: Option<&Colors>) -> Rgb {
        self.resolve_named(NamedColor::Background, overrides)
    }

    /// Resolves the effective foreground color.
    #[must_use]
    pub fn resolve_foreground(&self, overrides: Option<&Colors>) -> Rgb {
        self.resolve_named(NamedColor::Foreground, overrides)
    }

    /// Resolves the effective cursor color.
    #[must_use]
    pub fn resolve_cursor(&self, overrides: Option<&Colors>) -> Rgb {
        self.resolve_named(NamedColor::Cursor, overrides)
    }

    /// Resolves a cell's final colors according to:
    ///
    /// 1. Base color / dynamic OSC overrides
    /// 2. `DIM` (matching dim named color or 0.66 scale factor)
    /// 3. `INVERSE` (swap fg and bg after dim)
    /// 4. `HIDDEN` (fg = bg)
    #[must_use]
    pub fn resolve_cell(
        &self,
        req: CellColorRequest,
        overrides: Option<&Colors>,
    ) -> ResolvedCellColors {
        let mut bg = self.resolve_color(req.bg, overrides);
        let mut fg = if req.flags.contains(Flags::DIM) {
            match req.fg {
                Color::Named(named) => self.resolve_named(named.to_dim(), overrides),
                Color::Indexed(_) | Color::Spec(_) => {
                    scale_rgb_const(self.resolve_color(req.fg, overrides), 66)
                }
            }
        } else {
            self.resolve_color(req.fg, overrides)
        };

        if req.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }

        if req.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }

        ResolvedCellColors { fg, bg }
    }
}

/// A color resolver bound to a specific terminal's dynamic colors.
#[derive(Clone, Copy)]
pub struct TermPalette<'a> {
    palette: &'a Palette,
    colors: &'a Colors,
}

impl std::fmt::Debug for TermPalette<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TermPalette")
            .field("palette", &self.palette)
            .finish_non_exhaustive()
    }
}

impl<'a> TermPalette<'a> {
    /// Binds a [`Palette`] to a terminal's active dynamic [`Colors`].
    #[must_use]
    pub const fn new(palette: &'a Palette, colors: &'a Colors) -> Self {
        Self { palette, colors }
    }

    /// Resolves any [`Color`] (Named, Indexed, or Spec) against the bound terminal colors.
    #[must_use]
    pub fn resolve(&self, color: Color) -> Rgb {
        self.palette.resolve_color(color, Some(self.colors))
    }

    /// Resolves a [`NamedColor`] against the bound terminal colors.
    #[must_use]
    pub fn resolve_named(&self, named: NamedColor) -> Rgb {
        self.palette.resolve_named(named, Some(self.colors))
    }

    /// Resolves an indexed color (`0..=255`) against the bound terminal colors.
    #[must_use]
    pub fn resolve_indexed(&self, index: u8) -> Rgb {
        self.palette.resolve_indexed(index, Some(self.colors))
    }

    /// Resolves the terminal's active background color.
    #[must_use]
    pub fn background(&self) -> Rgb {
        self.palette.resolve_background(Some(self.colors))
    }

    /// Resolves the terminal's active foreground color.
    #[must_use]
    pub fn foreground(&self) -> Rgb {
        self.palette.resolve_foreground(Some(self.colors))
    }

    /// Resolves the terminal's active cursor color.
    #[must_use]
    pub fn cursor(&self) -> Rgb {
        self.palette.resolve_cursor(Some(self.colors))
    }

    /// Resolves the final foreground and background colors for a cell request.
    #[must_use]
    pub fn resolve_cell(&self, req: CellColorRequest) -> ResolvedCellColors {
        self.palette.resolve_cell(req, Some(self.colors))
    }

    /// Resolves the final foreground and background colors directly from a [`Cell`].
    #[must_use]
    pub fn resolve_term_cell(&self, cell: &Cell) -> ResolvedCellColors {
        self.resolve_cell(CellColorRequest::from_cell(cell))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    struct TestTermSize {
        columns: usize,
        screen_lines: usize,
    }

    impl Dimensions for TestTermSize {
        fn total_lines(&self) -> usize {
            self.screen_lines
        }
        fn screen_lines(&self) -> usize {
            self.screen_lines
        }
        fn columns(&self) -> usize {
            self.columns
        }
    }

    fn new_test_term() -> Term<VoidListener> {
        let size = TestTermSize {
            columns: 80,
            screen_lines: 24,
        };
        Term::new(Config::default(), &size, VoidListener)
    }

    #[test]
    fn test_ansi_named_colors() {
        let palette = BasePalette::default();
        assert_eq!(palette.named(NamedColor::Black), rgb(0x00, 0x00, 0x00));
        assert_eq!(palette.named(NamedColor::Red), rgb(0xcd, 0x00, 0x00));
        assert_eq!(palette.named(NamedColor::Green), rgb(0x00, 0xcd, 0x00));
        assert_eq!(palette.named(NamedColor::Yellow), rgb(0xcd, 0xcd, 0x00));
        assert_eq!(palette.named(NamedColor::Blue), rgb(0x00, 0x00, 0xee));
        assert_eq!(palette.named(NamedColor::Magenta), rgb(0xcd, 0x00, 0xcd));
        assert_eq!(palette.named(NamedColor::Cyan), rgb(0x00, 0xcd, 0xcd));
        assert_eq!(palette.named(NamedColor::White), rgb(0xe5, 0xe5, 0xe5));
        assert_eq!(
            palette.named(NamedColor::BrightBlack),
            rgb(0x7f, 0x7f, 0x7f)
        );
        assert_eq!(
            palette.named(NamedColor::BrightWhite),
            rgb(0xff, 0xff, 0xff)
        );
    }

    #[test]
    fn test_default_canvas_colors() {
        let palette = BasePalette::default();
        assert_eq!(palette.named(NamedColor::Foreground), DEFAULT_FOREGROUND);
        assert_eq!(palette.named(NamedColor::Background), DEFAULT_BACKGROUND);
        assert_eq!(palette.named(NamedColor::Cursor), DEFAULT_CURSOR);
    }

    #[test]
    fn test_dim_named_colors() {
        let palette = BasePalette::default();
        let red = palette.named(NamedColor::Red);
        let dim_red = palette.named(NamedColor::DimRed);
        let expected_r = u8::try_from((u32::from(red.r) * 66) / 100).unwrap();
        assert_eq!(dim_red.r, expected_r);
        assert_eq!(dim_red.g, 0);
        assert_eq!(dim_red.b, 0);
    }

    #[test]
    fn test_color_cube_bounds() {
        let palette = BasePalette::default();
        // Index 16 is (0, 0, 0)
        assert_eq!(palette.indexed(16), rgb(0, 0, 0));
        // Index 16 + 1 is (0, 0, 95)
        assert_eq!(palette.indexed(17), rgb(0, 0, 95));
        // Index 16 + 36 * 5 is (255, 0, 0)
        assert_eq!(palette.indexed(196), rgb(255, 0, 0));
        // Index 231 is (255, 255, 255)
        assert_eq!(palette.indexed(231), rgb(255, 255, 255));
    }

    #[test]
    fn test_grayscale_ramp() {
        let palette = BasePalette::default();
        // Index 232 is 8
        assert_eq!(palette.indexed(232), rgb(8, 8, 8));
        // Index 255 is 238
        assert_eq!(palette.indexed(255), rgb(238, 238, 238));
    }

    #[test]
    fn test_to_rgb_resolution() {
        let palette = BasePalette::default();
        assert_eq!(
            palette.to_rgb(Color::Named(NamedColor::Red)),
            rgb(0xcd, 0x00, 0x00)
        );
        assert_eq!(palette.to_rgb(Color::Indexed(196)), rgb(255, 0, 0));
        let truecolor = rgb(12, 34, 56);
        assert_eq!(palette.to_rgb(Color::Spec(truecolor)), truecolor);
    }

    #[test]
    fn test_to_hex() {
        assert_eq!(to_hex(rgb(0, 0, 0)), "#000000");
        assert_eq!(to_hex(rgb(255, 0, 128)), "#ff0080");
        assert_eq!(to_hex(rgb(18, 52, 86)), "#123456");
    }

    #[test]
    fn test_scale_rgb() {
        let c = rgb(100, 200, 50);
        let scaled = scale_rgb(c, 0.5);
        assert_eq!(scaled, rgb(50, 100, 25));
    }

    #[test]
    fn test_osc4_indexed_color_override() {
        let mut term = new_test_term();
        let mut parser: Processor<StdSyncHandler> = Processor::new();

        // Initially index 1 (Red) is the base palette red
        let palette = Palette::default();
        let term_palette = TermPalette::new(&palette, term.colors());
        assert_eq!(
            term_palette.resolve(Color::Indexed(1)),
            rgb(0xcd, 0x00, 0x00)
        );
        assert_eq!(
            term_palette.resolve(Color::Named(NamedColor::Red)),
            rgb(0xcd, 0x00, 0x00)
        );

        // App overrides color 1 via OSC 4 to custom #112233
        parser.advance(&mut term, b"\x1b]4;1;#112233\x1b\\");

        let term_palette = TermPalette::new(&palette, term.colors());
        assert_eq!(
            term_palette.resolve(Color::Indexed(1)),
            rgb(0x11, 0x22, 0x33)
        );
        assert_eq!(
            term_palette.resolve(Color::Named(NamedColor::Red)),
            rgb(0x11, 0x22, 0x33)
        );

        // Other colors (e.g. index 2 Green) remain unchanged
        assert_eq!(
            term_palette.resolve(Color::Indexed(2)),
            rgb(0x00, 0xcd, 0x00)
        );
    }

    #[test]
    fn test_osc10_foreground_and_osc11_background_override() {
        let mut term = new_test_term();
        let mut parser: Processor<StdSyncHandler> = Processor::new();

        let palette = Palette::default();
        let term_palette = TermPalette::new(&palette, term.colors());
        assert_eq!(term_palette.foreground(), DEFAULT_FOREGROUND);
        assert_eq!(term_palette.background(), DEFAULT_BACKGROUND);

        // App overrides foreground to #aabbcc (OSC 10) and background to #443322 (OSC 11)
        parser.advance(&mut term, b"\x1b]10;#aabbcc\x1b\\\x1b]11;#443322\x1b\\");

        let term_palette = TermPalette::new(&palette, term.colors());
        assert_eq!(term_palette.foreground(), rgb(0xaa, 0xbb, 0xcc));
        assert_eq!(term_palette.background(), rgb(0x44, 0x33, 0x22));
        assert_eq!(
            term_palette.resolve(Color::Named(NamedColor::Foreground)),
            rgb(0xaa, 0xbb, 0xcc)
        );
        assert_eq!(
            term_palette.resolve(Color::Named(NamedColor::Background)),
            rgb(0x44, 0x33, 0x22)
        );
    }

    #[test]
    fn test_osc12_cursor_override() {
        let mut term = new_test_term();
        let mut parser: Processor<StdSyncHandler> = Processor::new();

        let palette = Palette::default();
        let term_palette = TermPalette::new(&palette, term.colors());
        assert_eq!(term_palette.cursor(), DEFAULT_CURSOR);

        // App overrides cursor to #ffaa00 (OSC 12)
        parser.advance(&mut term, b"\x1b]12;#ffaa00\x1b\\");

        let term_palette = TermPalette::new(&palette, term.colors());
        assert_eq!(term_palette.cursor(), rgb(0xff, 0xaa, 0x00));
        assert_eq!(
            term_palette.resolve(Color::Named(NamedColor::Cursor)),
            rgb(0xff, 0xaa, 0x00)
        );
    }

    #[test]
    fn test_dim_scaling_with_overridden_base_color() {
        let mut term = new_test_term();
        let mut parser: Processor<StdSyncHandler> = Processor::new();

        let palette = Palette::default();

        // Override color 1 (Red) to #ff0000
        parser.advance(&mut term, b"\x1b]4;1;#ff0000\x1b\\");

        let term_palette = TermPalette::new(&palette, term.colors());
        // DimRed should scale the overridden Red (#ff0000) by 66%
        let dim_red = term_palette.resolve(Color::Named(NamedColor::DimRed));
        let expected_r = u8::try_from((255u32 * 66) / 100).unwrap();
        assert_eq!(dim_red, rgb(expected_r, 0, 0));
    }

    #[test]
    fn test_underline_from_flags() {
        assert_eq!(Underline::from_flags(Flags::empty()), Underline::None);
        assert_eq!(Underline::from_flags(Flags::UNDERLINE), Underline::Single);
        assert_eq!(
            Underline::from_flags(Flags::DOUBLE_UNDERLINE),
            Underline::Double
        );
        assert_eq!(Underline::from_flags(Flags::UNDERCURL), Underline::Curly);
        assert_eq!(
            Underline::from_flags(Flags::DOTTED_UNDERLINE),
            Underline::Dotted
        );
        assert_eq!(
            Underline::from_flags(Flags::DASHED_UNDERLINE),
            Underline::Dashed
        );
    }

    #[test]
    fn test_cell_dim_resolution() {
        let palette = Palette::default();
        let red = Color::Named(NamedColor::Red);
        let bg = Color::Named(NamedColor::Background);

        // Without DIM
        let normal = palette.resolve_cell(CellColorRequest::new(red, bg, Flags::empty()), None);
        assert_eq!(normal.fg, rgb(0xcd, 0x00, 0x00));
        assert_eq!(normal.bg, DEFAULT_BACKGROUND);

        // With DIM: Named red becomes DimRed
        let dimmed = palette.resolve_cell(CellColorRequest::new(red, bg, Flags::DIM), None);
        let expected_r = u8::try_from((0xcdu32 * 66) / 100).unwrap();
        assert_eq!(dimmed.fg, rgb(expected_r, 0, 0));
        assert_eq!(dimmed.bg, DEFAULT_BACKGROUND);

        // With DIM on truecolor Spec
        let spec = Color::Spec(rgb(100, 200, 50));
        let dimmed_spec = palette.resolve_cell(CellColorRequest::new(spec, bg, Flags::DIM), None);
        assert_eq!(dimmed_spec.fg, rgb(66, 132, 33));
    }

    #[test]
    fn test_cell_inverse_resolution() {
        let palette = Palette::default();
        let fg = Color::Named(NamedColor::Red);
        let bg = Color::Named(NamedColor::Blue);

        let res = palette.resolve_cell(CellColorRequest::new(fg, bg, Flags::INVERSE), None);
        assert_eq!(res.fg, palette.base().named(NamedColor::Blue));
        assert_eq!(res.bg, palette.base().named(NamedColor::Red));
    }

    #[test]
    fn test_cell_dim_then_inverse_order() {
        let palette = Palette::default();
        let fg = Color::Named(NamedColor::Red);
        let bg = Color::Named(NamedColor::Blue);

        // Pipeline order: DIM dims fg, then INVERSE swaps fg and bg
        let res = palette.resolve_cell(
            CellColorRequest::new(fg, bg, Flags::DIM | Flags::INVERSE),
            None,
        );
        let expected_dim_r = u8::try_from((0xcdu32 * 66) / 100).unwrap();
        assert_eq!(res.fg, palette.base().named(NamedColor::Blue));
        assert_eq!(res.bg, rgb(expected_dim_r, 0, 0));
    }

    #[test]
    fn test_cell_hidden_resolution() {
        let palette = Palette::default();
        let fg = Color::Named(NamedColor::Red);
        let bg = Color::Named(NamedColor::Blue);

        // HIDDEN sets fg = bg
        let res = palette.resolve_cell(CellColorRequest::new(fg, bg, Flags::HIDDEN), None);
        assert_eq!(res.fg, palette.base().named(NamedColor::Blue));
        assert_eq!(res.bg, palette.base().named(NamedColor::Blue));

        // INVERSE + HIDDEN: swap first (fg=Blue, bg=Red), then HIDDEN (fg = bg = Red)
        let inv_hidden = palette.resolve_cell(
            CellColorRequest::new(fg, bg, Flags::INVERSE | Flags::HIDDEN),
            None,
        );
        assert_eq!(inv_hidden.fg, palette.base().named(NamedColor::Red));
        assert_eq!(inv_hidden.bg, palette.base().named(NamedColor::Red));
    }

    #[test]
    fn test_term_cell_resolution_from_terminal_grid() {
        use alacritty_terminal::index::{Column, Line};

        let mut term = new_test_term();
        let mut parser: Processor<StdSyncHandler> = Processor::new();

        // Write "A" with Red fg, Green bg, Dim and Inverse
        parser.advance(&mut term, b"\x1b[31;42;2;7mA\x1b[0m");

        let palette = Palette::default();
        let term_palette = TermPalette::new(&palette, term.colors());

        let cell = &term.grid()[Line(0)][Column(0)];
        assert_eq!(cell.c, 'A');
        assert!(cell.flags.contains(Flags::DIM));
        assert!(cell.flags.contains(Flags::INVERSE));

        let colors = term_palette.resolve_term_cell(cell);
        // fg was Green (inverted), bg was Red (dimmed and inverted)
        assert_eq!(colors.fg, palette.base().named(NamedColor::Green));
        let expected_dim_r = u8::try_from((0xcdu32 * 66) / 100).unwrap();
        assert_eq!(colors.bg, rgb(expected_dim_r, 0, 0));
    }
}
