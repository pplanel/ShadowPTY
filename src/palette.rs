//! Terminal color palette and resolution according to ANSI and `XTerm` standards.
//!
//! Provides the base 16 ANSI colors, the 6×6×6 color cube (indices 16–231),
//! the 24-step grayscale ramp (indices 232–255), and default canvas colors.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
