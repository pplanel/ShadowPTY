//! SVG screenshot renderer for terminal snapshots.
//!
//! Renders a [`Snapshot`] into a deterministic SVG image with custom styles.

use std::collections::HashMap;
use std::fmt::Write;

use crate::palette::{Rgb, Underline, rgb, to_hex};
use crate::snapshot::{CursorShape, SnapCursor, Snapshot};

/// Theme configuration for SVG rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    /// Font stack to use in SVG `<style>`.
    pub font_family: String,
    /// Font size in pixels.
    pub font_size: f64,
    /// Width of a single character cell in pixels.
    pub cell_width: f64,
    /// Height of a single character cell in pixels.
    pub cell_height: f64,
    /// Baseline offset from cell top in pixels.
    pub baseline: f64,
    /// Default cursor color if not resolved from terminal.
    pub cursor_color: Rgb,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            font_family: "\"JetBrains Mono\", Menlo, Consolas, \"DejaVu Sans Mono\", monospace"
                .to_string(),
            font_size: 14.0,
            cell_width: 9.0,
            cell_height: 18.0,
            baseline: 14.0,
            cursor_color: rgb(0xff, 0xff, 0xff),
        }
    }
}

/// Renders a [`Snapshot`] to an SVG string using the default theme.
#[must_use]
pub fn render_svg_default(snapshot: &Snapshot) -> String {
    render_svg(snapshot, &Theme::default())
}

/// Renders a [`Snapshot`] to a deterministic SVG string.
#[must_use]
pub fn render_svg(snapshot: &Snapshot, theme: &Theme) -> String {
    let total_width = f64::from(snapshot.cols) * theme.cell_width;
    let total_height = f64::from(snapshot.rows) * theme.cell_height;

    // Find the most frequent background color to use as canvas fill
    let canvas_bg = find_dominant_bg(snapshot);
    let canvas_bg_hex = to_hex(canvas_bg);

    let mut svg = String::with_capacity(snapshot.cells.len() * 32 + 512);

    let _ = writeln!(
        svg,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {total_width:.1} {total_height:.1}\" width=\"{total_width:.1}\" height=\"{total_height:.1}\">"
    );
    svg.push_str("  <style>\n");
    let _ = writeln!(
        svg,
        "    text {{ font-family: {}; font-size: {:.1}px; white-space: pre; }}",
        theme.font_family, theme.font_size
    );
    svg.push_str("  </style>\n");

    // Canvas background
    let _ = writeln!(
        svg,
        "  <rect width=\"{total_width:.1}\" height=\"{total_height:.1}\" fill=\"{canvas_bg_hex}\"/>"
    );

    // Render non-canvas background rects (grouped by horizontal run)
    render_background_rects(snapshot, theme, canvas_bg, &mut svg);

    // Render text runs, underlines, and strikethroughs
    render_content(snapshot, theme, &mut svg);

    // Render cursor if visible
    if let Some(cursor) = snapshot.cursor {
        render_cursor(cursor, theme, &mut svg);
    }

    svg.push_str("</svg>\n");
    svg
}

/// Finds the most frequent background color across all snapshot cells.
fn find_dominant_bg(snapshot: &Snapshot) -> Rgb {
    if snapshot.cells.is_empty() {
        return crate::palette::DEFAULT_BACKGROUND;
    }

    let mut counts: HashMap<(u8, u8, u8), usize> = HashMap::new();
    for cell in &snapshot.cells {
        *counts.entry((cell.bg.r, cell.bg.g, cell.bg.b)).or_default() += 1;
    }

    // Deterministic selection: highest count, breaking ties by (r, g, b) ordering
    counts
        .into_iter()
        .max_by(|(rgb_a, count_a), (rgb_b, count_b)| {
            count_a.cmp(count_b).then_with(|| rgb_a.cmp(rgb_b))
        })
        .map_or(crate::palette::DEFAULT_BACKGROUND, |((r, g, b), _)| {
            rgb(r, g, b)
        })
}

/// Renders horizontal runs of non-default background rectangles.
fn render_background_rects(snapshot: &Snapshot, theme: &Theme, canvas_bg: Rgb, svg: &mut String) {
    for row in 0..snapshot.rows {
        let mut col = 0;
        while col < snapshot.cols {
            let Some(cell) = snapshot.cell(row, col) else {
                break;
            };

            let bg = cell.bg;
            let run_start = col;
            while col < snapshot.cols {
                if let Some(next_cell) = snapshot.cell(row, col)
                    && next_cell.bg == bg
                {
                    col += 1;
                    continue;
                }
                break;
            }

            if bg != canvas_bg {
                let x = f64::from(run_start) * theme.cell_width;
                let y = f64::from(row) * theme.cell_height;
                let w = f64::from(col - run_start) * theme.cell_width;
                let bg_hex = to_hex(bg);
                let _ = writeln!(
                    svg,
                    "  <rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{:.1}\" fill=\"{bg_hex}\"/>",
                    theme.cell_height
                );
            }
        }
    }
}

/// Holds a horizontal run of cells sharing identical text styling attributes.
#[derive(Debug, PartialEq, Eq)]
struct TextRunStyle {
    fg: Rgb,
    bold: bool,
    italic: bool,
    underline: Underline,
    strikethrough: bool,
}

/// Parameters for rendering an underline line or wave.
#[derive(Clone, Copy)]
struct UnderlineRenderRequest {
    underline: Underline,
    x: f64,
    y: f64,
    run_cols: u16,
    color: Rgb,
}

/// Renders text runs and decoration lines (underlines, strikethroughs).
fn render_content(snapshot: &Snapshot, theme: &Theme, svg: &mut String) {
    for row in 0..snapshot.rows {
        let mut col = 0;
        while col < snapshot.cols {
            let Some(start_cell) = snapshot.cell(row, col) else {
                break;
            };

            let style = TextRunStyle {
                fg: start_cell.fg,
                bold: start_cell.bold,
                italic: start_cell.italic,
                underline: start_cell.underline,
                strikethrough: start_cell.strikethrough,
            };

            let run_start = col;
            let mut run_text = String::new();

            while col < snapshot.cols {
                if let Some(next_cell) = snapshot.cell(row, col) {
                    let next_style = TextRunStyle {
                        fg: next_cell.fg,
                        bold: next_cell.bold,
                        italic: next_cell.italic,
                        underline: next_cell.underline,
                        strikethrough: next_cell.strikethrough,
                    };
                    if next_style == style {
                        run_text.push_str(&next_cell.text);
                        col += 1;
                        continue;
                    }
                }
                break;
            }

            let run_cols = col - run_start;
            let x = f64::from(run_start) * theme.cell_width;
            let y = f64::from(row) * theme.cell_height;
            let run_width = f64::from(run_cols) * theme.cell_width;

            // Only emit text if it is non-empty and contains non-whitespace or decorations
            let has_decorations = style.underline != Underline::None || style.strikethrough;
            let is_all_whitespace = run_text.chars().all(|c| c == ' ');

            if !run_text.is_empty() && (!is_all_whitespace || has_decorations) {
                let escaped = escape_xml_text(&run_text);
                let fg_hex = to_hex(style.fg);
                let baseline_y = y + theme.baseline;

                let mut attrs = format!(
                    "x=\"{x:.1}\" y=\"{baseline_y:.1}\" fill=\"{fg_hex}\" textLength=\"{run_width:.1}\" lengthAdjust=\"spacingAndGlyphs\" xml:space=\"preserve\""
                );
                if style.bold {
                    attrs.push_str(" font-weight=\"bold\"");
                }
                if style.italic {
                    attrs.push_str(" font-style=\"italic\"");
                }

                let _ = writeln!(svg, "  <text {attrs}>{escaped}</text>");
            }

            // Render decorations if present
            if style.underline != Underline::None {
                render_underline(
                    UnderlineRenderRequest {
                        underline: style.underline,
                        x,
                        y,
                        run_cols,
                        color: style.fg,
                    },
                    theme,
                    svg,
                );
            }
            if style.strikethrough {
                let strike_y = y + (theme.cell_height / 2.0);
                let fg_hex = to_hex(style.fg);
                let _ = writeln!(
                    svg,
                    "  <line x1=\"{x:.1}\" y1=\"{strike_y:.1}\" x2=\"{:.1}\" y2=\"{strike_y:.1}\" stroke=\"{fg_hex}\" stroke-width=\"1.2\"/>",
                    x + run_width
                );
            }
        }
    }
}

/// Renders an underline style along a given horizontal span.
fn render_underline(req: UnderlineRenderRequest, theme: &Theme, svg: &mut String) {
    let color_hex = to_hex(req.color);
    let ul_y = req.y + theme.cell_height - 2.0;
    let end_x = f64::from(req.run_cols).mul_add(theme.cell_width, req.x);

    match req.underline {
        Underline::None => {}
        Underline::Single => {
            let _ = writeln!(
                svg,
                "  <line x1=\"{:.1}\" y1=\"{ul_y:.1}\" x2=\"{end_x:.1}\" y2=\"{ul_y:.1}\" stroke=\"{color_hex}\" stroke-width=\"1.2\"/>",
                req.x
            );
        }
        Underline::Double => {
            let y1 = ul_y - 1.5;
            let y2 = ul_y + 0.5;
            let _ = writeln!(
                svg,
                "  <line x1=\"{:.1}\" y1=\"{y1:.1}\" x2=\"{end_x:.1}\" y2=\"{y1:.1}\" stroke=\"{color_hex}\" stroke-width=\"1.0\"/>",
                req.x
            );
            let _ = writeln!(
                svg,
                "  <line x1=\"{:.1}\" y1=\"{y2:.1}\" x2=\"{end_x:.1}\" y2=\"{y2:.1}\" stroke=\"{color_hex}\" stroke-width=\"1.0\"/>",
                req.x
            );
        }
        Underline::Dotted => {
            let _ = writeln!(
                svg,
                "  <line x1=\"{:.1}\" y1=\"{ul_y:.1}\" x2=\"{end_x:.1}\" y2=\"{ul_y:.1}\" stroke=\"{color_hex}\" stroke-width=\"1.2\" stroke-dasharray=\"1.5, 2.5\"/>",
                req.x
            );
        }
        Underline::Dashed => {
            let _ = writeln!(
                svg,
                "  <line x1=\"{:.1}\" y1=\"{ul_y:.1}\" x2=\"{end_x:.1}\" y2=\"{ul_y:.1}\" stroke=\"{color_hex}\" stroke-width=\"1.2\" stroke-dasharray=\"4, 3\"/>",
                req.x
            );
        }
        Underline::Curly => {
            let mut path_data = format!("M {:.1} {ul_y:.1}", req.x);
            let step = theme.cell_width;
            for i in 0..req.run_cols {
                let curr = f64::from(i).mul_add(step, req.x);
                let mid = curr + (step / 2.0);
                let next = (curr + step).min(end_x);
                let q1_x = curr + (step / 4.0);
                let q2_x = step.mul_add(0.75, curr);
                let _ = write!(
                    path_data,
                    " Q {q1_x:.1} {:.1}, {mid:.1} {ul_y:.1} Q {q2_x:.1} {:.1}, {next:.1} {ul_y:.1}",
                    ul_y - 1.5,
                    ul_y + 1.5
                );
            }
            let _ = writeln!(
                svg,
                "  <path d=\"{path_data}\" fill=\"none\" stroke=\"{color_hex}\" stroke-width=\"1.2\"/>"
            );
        }
    }
}

/// Renders the cursor based on its shape and viewport position.
fn render_cursor(cursor: SnapCursor, theme: &Theme, svg: &mut String) {
    let x = f64::from(cursor.col) * theme.cell_width;
    let y = f64::from(cursor.row) * theme.cell_height;
    let cursor_hex = to_hex(theme.cursor_color);

    match cursor.shape {
        CursorShape::Block => {
            let _ = writeln!(
                svg,
                "  <rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"{cursor_hex}\" opacity=\"0.4\"/>",
                theme.cell_width, theme.cell_height
            );
        }
        CursorShape::Underline => {
            let line_y = y + theme.cell_height - 1.5;
            let end_x = x + theme.cell_width;
            let _ = writeln!(
                svg,
                "  <line x1=\"{x:.1}\" y1=\"{line_y:.1}\" x2=\"{end_x:.1}\" y2=\"{line_y:.1}\" stroke=\"{cursor_hex}\" stroke-width=\"2.0\"/>"
            );
        }
        CursorShape::Beam => {
            let beam_x = x + 1.0;
            let end_y = y + theme.cell_height;
            let _ = writeln!(
                svg,
                "  <line x1=\"{beam_x:.1}\" y1=\"{y:.1}\" x2=\"{beam_x:.1}\" y2=\"{end_y:.1}\" stroke=\"{cursor_hex}\" stroke-width=\"2.0\"/>"
            );
        }
        CursorShape::HollowBlock => {
            let w = theme.cell_width - 1.0;
            let h = theme.cell_height - 1.0;
            let _ = writeln!(
                svg,
                "  <rect x=\"{:.1}\" y=\"{:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" fill=\"none\" stroke=\"{cursor_hex}\" stroke-width=\"1.5\"/>",
                x + 0.5,
                y + 0.5
            );
        }
        CursorShape::Hidden => {}
    }
}

/// Escapes XML special characters for safe inclusion in SVG text.
fn escape_xml_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::palette::DEFAULT_FOREGROUND;
    use crate::snapshot::{SnapCell, SnapCursor};

    fn make_test_snapshot(rows: u16, cols: u16, fill_char: char) -> Snapshot {
        let total = usize::from(rows) * usize::from(cols);
        let mut cells = Vec::with_capacity(total);
        for _ in 0..total {
            cells.push(SnapCell {
                text: fill_char.to_string(),
                wide: false,
                fg: DEFAULT_FOREGROUND,
                bg: crate::palette::DEFAULT_BACKGROUND,
                bold: false,
                italic: false,
                underline: Underline::None,
                strikethrough: false,
            });
        }
        Snapshot {
            rows,
            cols,
            cells,
            cursor: None,
        }
    }

    #[test]
    fn test_svg_deterministic_output() {
        let snap = make_test_snapshot(3, 10, 'x');
        let theme = Theme::default();

        let svg1 = render_svg(&snap, &theme);
        let svg2 = render_svg(&snap, &theme);
        assert_eq!(svg1, svg2, "SVG output must be strictly deterministic");
    }

    #[test]
    fn test_svg_contains_xml_and_styles() {
        let snap = make_test_snapshot(2, 5, 'a');
        let theme = Theme::default();
        let svg = render_svg(&snap, &theme);

        assert!(svg.starts_with("<svg xmlns="));
        assert!(svg.ends_with("</svg>\n"));
        assert!(svg.contains("<style>"));
        assert!(svg.contains("viewBox=\"0 0 45.0 36.0\""));
    }

    #[test]
    fn test_svg_escapes_special_characters() {
        let mut snap = make_test_snapshot(1, 4, ' ');
        snap.cells[0].text = "<".to_string();
        snap.cells[1].text = ">".to_string();
        snap.cells[2].text = "&".to_string();
        snap.cells[3].text = "\"".to_string();

        let svg = render_svg(&snap, &Theme::default());
        assert!(svg.contains("&lt;"));
        assert!(svg.contains("&gt;"));
        assert!(svg.contains("&amp;"));
        assert!(svg.contains("&quot;"));
        assert!(!svg.contains("<text>&amp;</text>"));
    }

    #[test]
    fn test_svg_renders_cursor_block_underline_beam() {
        let mut snap = make_test_snapshot(2, 5, ' ');
        snap.cursor = Some(SnapCursor {
            row: 0,
            col: 2,
            shape: CursorShape::Block,
        });

        let svg_block = render_svg(&snap, &Theme::default());
        assert!(svg_block.contains("<rect x=\"18.0\" y=\"0.0\" width=\"9.0\" height=\"18.0\""));

        snap.cursor = Some(SnapCursor {
            row: 1,
            col: 3,
            shape: CursorShape::Underline,
        });
        let svg_ul = render_svg(&snap, &Theme::default());
        assert!(svg_ul.contains("<line x1=\"27.0\" y1=\"34.5\" x2=\"36.0\" y2=\"34.5\""));

        snap.cursor = Some(SnapCursor {
            row: 0,
            col: 1,
            shape: CursorShape::Beam,
        });
        let svg_beam = render_svg(&snap, &Theme::default());
        assert!(svg_beam.contains("<line x1=\"10.0\" y1=\"0.0\" x2=\"10.0\" y2=\"18.0\""));
    }

    #[test]
    fn test_svg_renders_decorations() {
        let mut snap = make_test_snapshot(1, 3, 'a');
        snap.cells[0].underline = Underline::Single;
        snap.cells[1].underline = Underline::Curly;
        snap.cells[2].strikethrough = true;

        let svg = render_svg(&snap, &Theme::default());
        assert!(svg.contains("<line x1=\"0.0\""));
        assert!(svg.contains("<path d=\"M 9.0 16.0"));
        assert!(svg.contains("stroke-width=\"1.2\""));
    }
}
