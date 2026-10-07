//! Pure-Rust PNG rasterizer for terminal snapshots.
//!
//! Rasterizes a [`Snapshot`] into an RGB PNG image using embedded fonts and
//! procedural box-drawing glyphs.

use std::sync::LazyLock;

use base64::prelude::*;
use fontdue::{Font, FontSettings};

use crate::palette::{Rgb, Underline, rgb};
use crate::screen::{CursorShape, Screen, ScreenCell, ScreenCursor};

static FONT_REGULAR_BYTES: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");
static FONT_BOLD_BYTES: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf");

#[allow(clippy::expect_used)]
static FONT_REGULAR: LazyLock<Font> = LazyLock::new(|| {
    Font::from_bytes(FONT_REGULAR_BYTES, FontSettings::default())
        .expect("valid embedded regular font")
});

#[allow(clippy::expect_used)]
static FONT_BOLD: LazyLock<Font> = LazyLock::new(|| {
    Font::from_bytes(FONT_BOLD_BYTES, FontSettings::default()).expect("valid embedded bold font")
});

/// PNG rendering options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PngOptions {
    /// Pixel scaling factor (1 or 2).
    pub scale: u8,
    /// Default cursor color.
    pub cursor_color: Rgb,
}

impl Default for PngOptions {
    fn default() -> Self {
        Self {
            scale: 1,
            cursor_color: rgb(0xff, 0xff, 0xff),
        }
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

struct Canvas<'a> {
    pixels: &'a mut [u8],
    width: usize,
    cell_w: usize,
    cell_h: usize,
    scale: u8,
}

impl<'a> Canvas<'a> {
    const fn new(pixels: &'a mut [u8], width: usize, scale: u8) -> Self {
        let s = scale as usize;
        Self {
            pixels,
            width,
            cell_w: 9 * s,
            cell_h: 18 * s,
            scale,
        }
    }

    #[inline]
    fn fill_rect(&mut self, rect: Rect, color: Rgb) {
        for row in rect.y..rect.y + rect.h {
            let start = (row * self.width + rect.x) * 3;
            for col in 0..rect.w {
                let idx = start + col * 3;
                if idx + 2 < self.pixels.len() {
                    self.pixels[idx] = color.r;
                    self.pixels[idx + 1] = color.g;
                    self.pixels[idx + 2] = color.b;
                }
            }
        }
    }

    fn stroke_rect(&mut self, rect: Rect, color: Rgb) {
        if rect.w == 0 || rect.h == 0 {
            return;
        }
        self.fill_rect(
            Rect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: 1,
            },
            color,
        );
        self.fill_rect(
            Rect {
                x: rect.x,
                y: rect.y + rect.h - 1,
                w: rect.w,
                h: 1,
            },
            color,
        );
        self.fill_rect(
            Rect {
                x: rect.x,
                y: rect.y,
                w: 1,
                h: rect.h,
            },
            color,
        );
        self.fill_rect(
            Rect {
                x: rect.x + rect.w - 1,
                y: rect.y,
                w: 1,
                h: rect.h,
            },
            color,
        );
    }

    #[inline]
    #[allow(
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::many_single_char_names
    )]
    fn blend_rect(&mut self, rect: Rect, fg: Rgb, bg: Rgb, alpha: u8) {
        let inv_alpha = 255 - alpha;
        let red = ((u32::from(fg.r) * u32::from(alpha) + u32::from(bg.r) * u32::from(inv_alpha))
            / 255) as u8;
        let green = ((u32::from(fg.g) * u32::from(alpha) + u32::from(bg.g) * u32::from(inv_alpha))
            / 255) as u8;
        let blue = ((u32::from(fg.b) * u32::from(alpha) + u32::from(bg.b) * u32::from(inv_alpha))
            / 255) as u8;
        self.fill_rect(rect, rgb(red, green, blue));
    }

    #[inline]
    #[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
    fn blend_pixel(&mut self, x: usize, y: usize, color: Rgb, alpha: u8) {
        let idx = (y * self.width + x) * 3;
        if idx + 2 < self.pixels.len() {
            let a = u32::from(alpha);
            let inv_a = 255 - a;

            let cur_r = u32::from(self.pixels[idx]);
            let cur_g = u32::from(self.pixels[idx + 1]);
            let cur_b = u32::from(self.pixels[idx + 2]);

            self.pixels[idx] = ((u32::from(color.r) * a + cur_r * inv_a) / 255) as u8;
            self.pixels[idx + 1] = ((u32::from(color.g) * a + cur_g * inv_a) / 255) as u8;
            self.pixels[idx + 2] = ((u32::from(color.b) * a + cur_b * inv_a) / 255) as u8;
        }
    }
}

/// Renders a [`Screen`] into PNG bytes using default options (scale 1x).
pub fn render_png_default(screen: &Screen) -> Result<Vec<u8>, anyhow::Error> {
    render_png(screen, PngOptions::default())
}

/// Renders a [`Screen`] into PNG bytes.
pub fn render_png(screen: &Screen, options: PngOptions) -> Result<Vec<u8>, anyhow::Error> {
    let scale = options.scale.clamp(1, 4);
    let scale_usize = usize::from(scale);
    let cell_width = 9 * scale_usize;
    let cell_height = 18 * scale_usize;

    let img_width = usize::from(screen.cols) * cell_width;
    let img_height = usize::from(screen.rows) * cell_height;

    if img_width == 0 || img_height == 0 {
        return Ok(Vec::new());
    }

    let mut pixels = vec![0u8; img_width * img_height * 3];
    let mut canvas = Canvas::new(&mut pixels, img_width, scale);

    draw_backgrounds(&mut canvas, screen);
    draw_cells(&mut canvas, screen);

    if let Some(cursor) = screen.cursor {
        draw_cursor(&mut canvas, cursor, options.cursor_color);
    }

    encode_png(canvas.pixels, img_width, img_height)
}

/// Fills cell background rectangles.
fn draw_backgrounds(canvas: &mut Canvas<'_>, screen: &Screen) {
    for row in 0..screen.rows {
        for col in 0..screen.cols {
            if let Some(cell) = screen.cell(row, col) {
                let x0 = usize::from(col) * canvas.cell_w;
                let y0 = usize::from(row) * canvas.cell_h;
                canvas.fill_rect(
                    Rect {
                        x: x0,
                        y: y0,
                        w: canvas.cell_w,
                        h: canvas.cell_h,
                    },
                    cell.bg,
                );
            }
        }
    }
}

/// Draws glyphs, box characters, and decorations for all cells.
fn draw_cells(canvas: &mut Canvas<'_>, screen: &Screen) {
    for row in 0..screen.rows {
        for col in 0..screen.cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };

            let x0 = usize::from(col) * canvas.cell_w;
            let y0 = usize::from(row) * canvas.cell_h;

            if !cell.text.is_empty() {
                let first_char = cell.text.chars().next().unwrap_or(' ');
                if is_box_drawing(first_char) {
                    draw_box_drawing(canvas, col, row, cell);
                } else if is_block_element(first_char) {
                    draw_block_element(canvas, col, row, cell);
                } else {
                    draw_text(canvas, col, row, cell);
                }
            }

            if cell.underline != Underline::None {
                draw_underline(canvas, col, row, cell);
            }

            if cell.strikethrough {
                let strike_y = y0 + (canvas.cell_h / 2);
                let strike_h = usize::from(canvas.scale).max(1);
                canvas.fill_rect(
                    Rect {
                        x: x0,
                        y: strike_y,
                        w: canvas.cell_w,
                        h: strike_h,
                    },
                    cell.fg,
                );
            }
        }
    }
}

/// Draws regular or bold text with optional synthetic italic skew.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn draw_text(canvas: &mut Canvas<'_>, col: u16, row: u16, cell: &ScreenCell) {
    let font = if cell.bold {
        &*FONT_BOLD
    } else {
        &*FONT_REGULAR
    };

    let font_size = 14.0 * f32::from(canvas.scale);
    let baseline = 14 * usize::from(canvas.scale);

    let mut current_x = usize::from(col) * canvas.cell_w;
    let y0 = usize::from(row) * canvas.cell_h;

    for ch in cell.text.chars() {
        if ch == ' ' {
            current_x += canvas.cell_w;
            continue;
        }

        let (metrics, bitmap) = font.rasterize(ch, font_size);
        if metrics.width == 0 || metrics.height == 0 {
            let w = canvas.cell_w.saturating_sub(2);
            let h = canvas.cell_h.saturating_sub(4);
            canvas.stroke_rect(
                Rect {
                    x: current_x + 1,
                    y: y0 + 2,
                    w,
                    h,
                },
                cell.fg,
            );
            break;
        }

        let base_glyph_x = current_x as i32 + metrics.xmin;
        let base_glyph_y = (y0 + baseline) as i32 - (metrics.height as i32 + metrics.ymin);

        for gy in 0..metrics.height {
            let row_y = base_glyph_y + gy as i32;
            if row_y < 0 {
                continue;
            }
            let target_y = row_y as usize;

            let skew = if cell.italic {
                ((metrics.height as i32 - gy as i32) * 2) / 7
            } else {
                0
            };

            for gx in 0..metrics.width {
                let col_x = base_glyph_x + gx as i32 + skew;
                if col_x < 0 {
                    continue;
                }
                let target_x = col_x as usize;

                let alpha = bitmap[gy * metrics.width + gx];
                if alpha > 0 {
                    canvas.blend_pixel(target_x, target_y, cell.fg, alpha);
                }
            }
        }

        if !is_combining_char(ch) {
            current_x += if cell.wide {
                canvas.cell_w * 2
            } else {
                canvas.cell_w
            };
        }
    }
}

/// Checks if a character is a Unicode combining mark.
const fn is_combining_char(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

/// Checks if a character is a box-drawing character (U+2500–U+257F).
const fn is_box_drawing(c: char) -> bool {
    matches!(c, '\u{2500}'..='\u{257F}')
}

/// Checks if a character is a block element (U+2580–U+259F).
const fn is_block_element(c: char) -> bool {
    matches!(c, '\u{2580}'..='\u{259F}')
}

const DIR_UP: u8 = 1;
const DIR_DOWN: u8 = 2;
const DIR_LEFT: u8 = 4;
const DIR_RIGHT: u8 = 8;

const fn box_connections(ch: char) -> (u8, bool) {
    match ch {
        '\u{2500}' => (DIR_LEFT | DIR_RIGHT, false),
        '\u{2502}' => (DIR_UP | DIR_DOWN, false),
        '\u{250C}' => (DIR_DOWN | DIR_RIGHT, false),
        '\u{2510}' => (DIR_DOWN | DIR_LEFT, false),
        '\u{2514}' => (DIR_UP | DIR_RIGHT, false),
        '\u{2518}' => (DIR_UP | DIR_LEFT, false),
        '\u{251C}' => (DIR_UP | DIR_DOWN | DIR_RIGHT, false),
        '\u{2524}' => (DIR_UP | DIR_DOWN | DIR_LEFT, false),
        '\u{252C}' => (DIR_DOWN | DIR_LEFT | DIR_RIGHT, false),
        '\u{2534}' => (DIR_UP | DIR_LEFT | DIR_RIGHT, false),
        '\u{2501}' => (DIR_LEFT | DIR_RIGHT, true),
        '\u{2503}' => (DIR_UP | DIR_DOWN, true),
        _ => (DIR_UP | DIR_DOWN | DIR_LEFT | DIR_RIGHT, false),
    }
}

/// Procedural box-drawing implementation ensuring zero gaps at cell boundaries.
fn draw_box_drawing(canvas: &mut Canvas<'_>, col: u16, row: u16, cell: &ScreenCell) {
    let ch = cell.text.chars().next().unwrap_or(' ');
    let (dirs, heavy) = box_connections(ch);
    let scale = usize::from(canvas.scale);
    let thickness = if heavy {
        (scale * 2).max(2)
    } else {
        scale.max(1)
    };
    let half_thick = thickness / 2;

    let x0 = usize::from(col) * canvas.cell_w;
    let y0 = usize::from(row) * canvas.cell_h;
    let mid_x = x0 + (canvas.cell_w / 2);
    let mid_y = y0 + (canvas.cell_h / 2);

    if (dirs & DIR_UP) != 0 {
        canvas.fill_rect(
            Rect {
                x: mid_x - half_thick,
                y: y0,
                w: thickness,
                h: mid_y - y0 + half_thick,
            },
            cell.fg,
        );
    }
    if (dirs & DIR_DOWN) != 0 {
        canvas.fill_rect(
            Rect {
                x: mid_x - half_thick,
                y: mid_y,
                w: thickness,
                h: canvas.cell_h - (mid_y - y0),
            },
            cell.fg,
        );
    }
    if (dirs & DIR_LEFT) != 0 {
        canvas.fill_rect(
            Rect {
                x: x0,
                y: mid_y - half_thick,
                w: mid_x - x0 + half_thick,
                h: thickness,
            },
            cell.fg,
        );
    }
    if (dirs & DIR_RIGHT) != 0 {
        canvas.fill_rect(
            Rect {
                x: mid_x,
                y: mid_y - half_thick,
                w: canvas.cell_w - (mid_x - x0),
                h: thickness,
            },
            cell.fg,
        );
    }
}

/// Procedural block elements (U+2580–U+259F).
fn draw_block_element(canvas: &mut Canvas<'_>, col: u16, row: u16, cell: &ScreenCell) {
    let ch = cell.text.chars().next().unwrap_or(' ');
    let w = canvas.cell_w;
    let h = canvas.cell_h;
    let x = usize::from(col) * w;
    let y = usize::from(row) * h;
    let hw = w / 2;
    let hh = h / 2;

    match ch {
        '\u{2580}' => canvas.fill_rect(Rect { x, y, w, h: hh }, cell.fg),
        '\u{2584}' => canvas.fill_rect(
            Rect {
                x,
                y: y + hh,
                w,
                h: h - hh,
            },
            cell.fg,
        ),
        '\u{258C}' => canvas.fill_rect(Rect { x, y, w: hw, h }, cell.fg),
        '\u{2590}' => canvas.fill_rect(
            Rect {
                x: x + hw,
                y,
                w: w - hw,
                h,
            },
            cell.fg,
        ),
        '\u{2591}' => canvas.blend_rect(Rect { x, y, w, h }, cell.fg, cell.bg, 64),
        '\u{2592}' => canvas.blend_rect(Rect { x, y, w, h }, cell.fg, cell.bg, 128),
        '\u{2593}' => canvas.blend_rect(Rect { x, y, w, h }, cell.fg, cell.bg, 192),
        _ => canvas.fill_rect(Rect { x, y, w, h }, cell.fg),
    }
}

/// Draws an underline style.
fn draw_underline(canvas: &mut Canvas<'_>, col: u16, row: u16, cell: &ScreenCell) {
    let scale = usize::from(canvas.scale);
    let x0 = usize::from(col) * canvas.cell_w;
    let y0 = usize::from(row) * canvas.cell_h;
    let ul_y = y0 + canvas.cell_h - 2 * scale;
    let h = scale.max(1);
    let cell_w = canvas.cell_w;

    match cell.underline {
        Underline::None => {}
        Underline::Single => canvas.fill_rect(
            Rect {
                x: x0,
                y: ul_y,
                w: cell_w,
                h,
            },
            cell.fg,
        ),
        Underline::Double => {
            let y1 = ul_y.saturating_sub(2 * scale);
            canvas.fill_rect(
                Rect {
                    x: x0,
                    y: y1,
                    w: cell_w,
                    h,
                },
                cell.fg,
            );
            canvas.fill_rect(
                Rect {
                    x: x0,
                    y: ul_y,
                    w: cell_w,
                    h,
                },
                cell.fg,
            );
        }
        Underline::Dotted => {
            let dot_len = scale.max(1);
            let mut x = x0;
            while x < x0 + cell_w {
                let w = dot_len.min((x0 + cell_w).saturating_sub(x));
                canvas.fill_rect(Rect { x, y: ul_y, w, h }, cell.fg);
                x += dot_len * 3;
            }
        }
        Underline::Dashed => {
            let dash_len = (scale * 3).max(3);
            let space_len = (scale * 2).max(2);
            let mut x = x0;
            while x < x0 + cell_w {
                let w = dash_len.min((x0 + cell_w).saturating_sub(x));
                canvas.fill_rect(Rect { x, y: ul_y, w, h }, cell.fg);
                x += dash_len + space_len;
            }
        }
        Underline::Curly => {
            for x in 0..cell_w {
                let wave_offset = if x < cell_w / 2 { 0 } else { scale.max(1) };
                let target_y = ul_y.saturating_sub(wave_offset);
                canvas.fill_rect(
                    Rect {
                        x: x0 + x,
                        y: target_y,
                        w: 1,
                        h,
                    },
                    cell.fg,
                );
            }
        }
    }
}

/// Renders the cursor in the pixel buffer.
fn draw_cursor(canvas: &mut Canvas<'_>, cursor: ScreenCursor, cursor_color: Rgb) {
    let scale = usize::from(canvas.scale);
    let x0 = usize::from(cursor.col) * canvas.cell_w;
    let y0 = usize::from(cursor.row) * canvas.cell_h;

    match cursor.shape {
        CursorShape::Block => {
            for dy in 0..canvas.cell_h {
                for dx in 0..canvas.cell_w {
                    canvas.blend_pixel(x0 + dx, y0 + dy, cursor_color, 128);
                }
            }
        }
        CursorShape::Underline => {
            let line_y = y0 + canvas.cell_h - (scale * 2).max(2);
            let h = (scale * 2).max(2);
            canvas.fill_rect(
                Rect {
                    x: x0,
                    y: line_y,
                    w: canvas.cell_w,
                    h,
                },
                cursor_color,
            );
        }
        CursorShape::Beam => {
            let w = (scale * 2).max(2);
            canvas.fill_rect(
                Rect {
                    x: x0,
                    y: y0,
                    w,
                    h: canvas.cell_h,
                },
                cursor_color,
            );
        }
        CursorShape::HollowBlock => {
            canvas.stroke_rect(
                Rect {
                    x: x0,
                    y: y0,
                    w: canvas.cell_w,
                    h: canvas.cell_h,
                },
                cursor_color,
            );
        }
        CursorShape::Hidden => {}
    }
}

/// Encodes an RGB image buffer to PNG format bytes.
fn encode_png(pixels: &[u8], width: usize, height: usize) -> Result<Vec<u8>, anyhow::Error> {
    let w = u32::try_from(width).map_err(|e| anyhow::anyhow!("width overflow: {e}"))?;
    let h = u32::try_from(height).map_err(|e| anyhow::anyhow!("height overflow: {e}"))?;

    let mut png_bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut png_bytes, w, h);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(pixels)?;
    drop(writer);
    Ok(png_bytes)
}

/// Encodes raw PNG bytes into a base64 string for MCP protocol image content.
#[must_use]
pub fn png_to_base64(png_bytes: &[u8]) -> String {
    BASE64_STANDARD.encode(png_bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::palette::DEFAULT_FOREGROUND;

    fn make_test_screen(rows: u16, cols: u16, text: &str) -> Screen {
        let total = usize::from(rows) * usize::from(cols);
        let mut cells = Vec::with_capacity(total);
        for _ in 0..total {
            cells.push(ScreenCell {
                text: text.to_string(),
                fg: DEFAULT_FOREGROUND,
                bg: crate::palette::DEFAULT_BACKGROUND,
                ..Default::default()
            });
        }
        Screen {
            rows,
            cols,
            cells,
            cursor: None,
        }
    }

    #[test]
    fn test_render_png_header_and_dimensions() {
        let screen = make_test_screen(10, 20, "A");
        let png_bytes = render_png(&screen, PngOptions::default()).expect("valid png");

        // Check PNG signature: 0x89, 'P', 'N', 'G', '\r', '\n', 0x1A, '\n'
        assert_eq!(
            &png_bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );

        // Decode PNG header to verify dimensions
        let cursor = std::io::Cursor::new(&png_bytes);
        let decoder = png::Decoder::new(cursor);
        let reader = decoder.read_info().expect("read png info");
        let info = reader.info();

        // cols=20 * 9 = 180, rows=10 * 18 = 180
        assert_eq!(info.width, 180);
        assert_eq!(info.height, 180);
    }

    #[test]
    fn test_render_png_scale_2x() {
        let screen = make_test_screen(5, 10, "X");
        let png_bytes = render_png(
            &screen,
            PngOptions {
                scale: 2,
                ..Default::default()
            },
        )
        .expect("valid png");

        let cursor = std::io::Cursor::new(&png_bytes);
        let decoder = png::Decoder::new(cursor);
        let reader = decoder.read_info().expect("read png info");
        let info = reader.info();

        // cols=10 * 18 = 180, rows=5 * 36 = 180
        assert_eq!(info.width, 180);
        assert_eq!(info.height, 180);
    }

    #[test]
    fn test_render_png_box_drawing_and_blocks() {
        let mut screen = make_test_screen(2, 4, " ");
        screen.cells[0].text = "─".to_string();
        screen.cells[1].text = "│".to_string();
        screen.cells[2].text = "█".to_string();
        screen.cells[3].text = "▄".to_string();

        let png_bytes = render_png_default(&screen).expect("valid png");
        assert_ne!(png_bytes, [] as [u8; 0]);
    }

    #[test]
    fn test_png_to_base64() {
        let screen = make_test_screen(2, 2, "Z");
        let png_bytes = render_png_default(&screen).expect("png");
        let b64 = png_to_base64(&png_bytes);
        assert!(b64.starts_with("iVBORw0KGgo")); // Standard base64 PNG header
    }
}
