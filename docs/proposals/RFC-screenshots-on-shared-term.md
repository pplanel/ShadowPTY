# RFC: Screenshots Rendered From the Session's Own `Term`

> **RFC Status:** Proposed
> **Date:** 2026-09-28
> **Implements:** step 4 of [RFC-single-emulator-core.md](./RFC-single-emulator-core.md) (§3.1, §3.5 "Colors in screenshots")
> **Replaces:** the `termsnap-lib` approach on `feat/tui-take-screenshot` (not merged)
> **Builds on:** `feat/alacritty-expect`

---

## 1. Goal

A screenshot should show exactly the screen the agent reads with `tui_read` and matches with `tui_expect`: same characters, same colors, same layout. The agent should also be able to **look at it**, not just save it to disk.

## 2. Problem

`feat/tui-take-screenshot` adds `tui_take_screenshot` using `termsnap-lib` 0.4:

- **Second emulator.** `termsnap_lib::Term` wraps its own `alacritty_terminal` **0.24**, fed byte by byte next to the session's emulator. The build would have two alacritty versions, and the two grids can drift (e.g. on resize or synchronized output).
- **Wrong or missing styling.** Checked in `termsnap-lib` 0.4.0's source:

  | Feature | termsnap | Effect |
  |---|---|---|
  | `INVERSE` (SGR 7) | ignored | Selections, status bars and highlighted menu items render with fg and bg the wrong way round |
  | `DIM` (SGR 2) | ignored | Dimmed hints render at full brightness |
  | `HIDDEN` (SGR 8) | ignored | Hidden text is visible |
  | Combining characters | dropped (only `cell.c`) | `é` written as `e` + U+0301 shows as `e` |
  | Cursor | not drawn | No way to check where the cursor is |
  | Palette changes by the app (OSC 4 / 10 / 11) | ignored (always a fixed palette) | Themed apps render in the wrong colors |
  | Underline styles | all treated as a single underline | Undercurl (e.g. spell-check / diagnostics) looks like a plain underline |

- **The agent can't see the result.** The tool writes an SVG to `output_path` and returns only a confirmation string. MCP clients generally show `image/png` content to the model, not SVG text.
- **Single-session only.** It predates `session_id`.

## 3. Proposal

### 3.1 Snapshot, then render

`tui_take_screenshot` locks the session's `Term` (the same lock `tui_read` uses), copies the visible grid into a plain `Snapshot`, releases the lock, and renders from the copy. The reader thread is blocked only for the copy (one 43×155 grid), never for rendering or file I/O.

```rust
pub struct Snapshot {
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<SnapCell>,          // rows * cols, row-major
    pub cursor: Option<SnapCursor>,    // None when the app hid it (DECTCEM off)
}

pub struct SnapCell {
    pub text: String,                  // cell.c + zero-width chars; "" for WIDE_CHAR_SPACER
    pub wide: bool,
    pub fg: Rgb,                       // resolved (§3.2)
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: Underline,          // None | Single | Double | Curly | Dotted | Dashed
    pub strikethrough: bool,
}

pub struct SnapCursor { pub row: u16, pub col: u16, pub shape: CursorShape }
```

Taking the snapshot from the shared `Term` means screenshots automatically get: the last *complete* synchronized-output frame (never a half-drawn one), re-wrap on resize, and the sync-timeout flush in the reader thread (an unterminated `?2026` frame is shown after 150 ms, fixed ahead of this RFC).

### 3.2 One color resolver

New module `src/palette.rs`, used by the screenshot and by `tui_read`'s formatter:

1. **Base palette**: 16 ANSI colors, the 6×6×6 cube (16–231) and the gray ramp (232–255), plus default foreground, background and cursor colors.
2. **App overrides**: entries the app set with OSC 4 / 10 / 11 are read from `term.colors()` and take precedence over the base palette.
3. **Cell resolution**, in this order:
   - `Color::Named` / `Color::Indexed` / `Color::Spec` → RGB via (1) and (2);
   - `DIM` → the matching `Dim*` named color for named colors; other colors scaled by 0.66 (alacritty's own factor);
   - `INVERSE` → swap fg and bg (after dim);
   - `HIDDEN` → fg = bg.

   Bold does **not** brighten colors (alacritty's default `draw_bold_text_with_bright_colors = false`).

The formatter keeps emitting color *names* (`<fg:red>`, `<fg:idx:123>`, `<fg:#rrggbb>`) because they are easier for an agent to reason about than RGB. The resolver's job is to guarantee that the name the formatter emits and the RGB the screenshot draws come from the same cell and the same rules. The formatter also gains the styles it currently misses: strikethrough, underline style and hidden text.

### 3.3 SVG renderer

New module `src/screenshot.rs`, `fn render_svg(&Snapshot, &Theme) -> String`:

- One `<rect>` per horizontal run of equal background; the most common background is drawn once as the canvas fill.
- One `<text>` per run of equal text style, positioned by cell, with `textLength` = run width × cell advance so the font can't shift columns. Wide characters take two cells; spacers are skipped. Zero-width characters stay in the same text run as their base character.
- Underline styles as `<line>` / `<path>` (curly as a wave); strikethrough as a line.
- Cursor: block (inverted cell), underline or beam, according to its `CursorShape`; not drawn when hidden.
- Font stack from the theme (default `"JetBrains Mono", Menlo, Consolas, "DejaVu Sans Mono", monospace`).
- Characters escaped for XML (`&`, `<`, `>`); control and private-use characters that fonts don't cover are rendered as-is, never dropped.
- **Deterministic**: the same snapshot gives byte-identical SVG (no timestamps or random IDs), so SVGs can be used as golden files in tests.

### 3.4 PNG rasterizer

So the agent can see the screenshot, render PNG from the same `Snapshot`, without going through SVG:

- Rasterize cell by cell with `fontdue` (pure Rust), write with `png`.
- Embed one open-licensed monospace font (regular + bold, italic synthesized by skew) so output is identical on every machine. A missing glyph is drawn as a box, never silently dropped.
- Draw box-drawing (U+2500–U+257F) and block elements (U+2580–U+259F) procedurally, like alacritty's built-in font, so TUI borders join exactly across cells.
- Scale factor parameter (default 1×; 2× for crisp images).

This adds `fontdue`, `png` and ~0.5–1 MB of font data to the binary (see §7, question 2).

### 3.5 Tool

```text
tui_take_screenshot {
  session_id?:     string            // default "default"
  format?:         "png" | "svg"     // default "png"
  output_path?:    string            // absolute; if set, write the file there
  include_cursor?: bool              // default true
  scale?:          1 | 2             // PNG only, default 1
}
```

- Without `output_path`: return PNG as MCP image content (`image/png`, base64), or SVG as text.
- With `output_path`: write the file and return the path, size in cells and pixels, and byte count. Relative paths are rejected; the parent directory must exist.
- Errors (unknown session, bad path, write failure) come back as `CallToolResult::error` like the other tools.

Ported from `feat/tui-take-screenshot`: the tool name, the `output_path` parameter and its test. Not ported: `termsnap-lib`, the second `Term`, the `rustix` 0.38 dependency, and the extra per-byte parse in the reader thread.

## 4. Alternatives considered

| Option | Why not |
|---|---|
| Keep `termsnap-lib`, feed it from our `Term` | Its public API only builds a screen from its own `Term`; it would still pull in alacritty 0.24, and the styling gaps in §2 remain. |
| Fork `termsnap-lib` onto alacritty 0.26 | Same result as §3.3, plus a fork to maintain. The SVG part is ~300 lines. |
| PNG via SVG + `resvg` | Pulls in `usvg`/`tiny-skia`/`fontdb`, and output depends on system fonts unless a font is embedded anyway. Rasterizing a cell grid directly is simpler and deterministic. |
| SVG only | The agent couldn't see the screenshot, which is the main reason to have one. |

## 5. Plan

Each step is a separate commit with its own tests, on top of `feat/alacritty-expect`.

0. **Sync-frame timeout** (done, not yet committed): the reader polls with the sync deadline, calls `Processor::stop_sync` when it passes, and wakes waiters.
1. **Palette and snapshot**: `palette.rs`, `Snapshot`, cell resolution (§3.1–3.2); the formatter adopts the missing styles. Unit tests per rule (named, indexed, truecolor, OSC 4 override, dim, inverse, hidden, wide, combining).
2. **SVG + tool**: `screenshot.rs` SVG renderer and `tui_take_screenshot` with `format: "svg"` and `output_path`. Golden-file tests.
3. **PNG**: rasterizer, embedded font, procedural box drawing; `format: "png"` becomes the default and is returned inline.
4. **Benchmarks**: add `render/snapshot`, `render/screenshot_svg` and `render/screenshot_png` to `benches/shadowpty.rs`.

## 6. Acceptance criteria

- [ ] `termsnap-lib` is not a dependency and `Cargo.lock` has one `alacritty_terminal`.
- [ ] For a test corpus (SGR colors 16/256/truecolor, OSC 4 override, dim, inverse, hidden, wide chars, combining chars), every cell's character and resolved fg/bg in the screenshot equal the resolver's output for the same cell that `tui_read` formats.
- [ ] A frame drawn inside synchronized output never appears half-drawn in a screenshot; an unterminated frame appears within 150 ms + one poll.
- [ ] The cursor is drawn at the position and shape the app set, and not drawn after `ESC[?25l`.
- [ ] Two screenshots of an unchanged screen are byte-identical (SVG and PNG).
- [ ] Box-drawing borders have no gaps between cells in PNG output.
- [ ] Taking a screenshot doesn't stall the reader: an app writing continuously keeps its output rate while screenshots are taken in a loop.
- [ ] Snapshot plus SVG for a 43×155 screen takes under 2 ms; PNG at 1× under 20 ms (criterion, release build).
- [ ] Works per `session_id`; an unknown session returns a tool error.

## 7. Open questions

1. **Default format**: PNG inline (proposed) or SVG? PNG is what the agent can actually look at; SVG is smaller and diffable.
2. **Binary size**: ShadowPTY ships through npm. Is ~1 MB of embedded font acceptable, or should PNG be behind a cargo feature that's on by default?
3. **Default theme**: xterm's palette, alacritty's default, or a named theme (e.g. a light and a dark one) chosen per session in `tui_start`?
4. **Glyph coverage**: CJK and emoji aren't in a typical monospace font. Box placeholder (proposed), or allow a fallback to system fonts at the cost of determinism?
