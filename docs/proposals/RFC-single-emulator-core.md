# RFC: Single-Emulator Session Core — Continuous Reader, alacritty `Term`, Measurable Sessions

> **RFC Status:** Proposed
> **Date:** 2026-09-28
> **Supersedes (in part):** the `rust-expect` session layer on `feat/expect-feat`
> **Related:** [RFC-fluid-agent-terminal-api.md](./RFC-fluid-agent-terminal-api.md) — `tui_step` and the other FATA tools build on this core unchanged.

---

## 1. Goal

ShadowPTY should be able to run **real tests** against terminal applications:

1. **Measure**: time to first output, time to a stable screen, frame count and duration, bytes written.
2. **Record**: asciicast files whose timestamps match what the app actually did.
3. **Stay faithful**: the colors and layout an agent reads (as text or as a screenshot) match what a real terminal would show.

## 2. Problem

Three branches each solve part of this, and they don't fit together.

| Branch | PTY owner | Emulator(s) | Reads output |
|---|---|---|---|
| `feat/expect-feat` | `rust-expect` (`Session<AsyncPty>`) | `vt100` + rust-expect's screen | Only during tool calls |
| `feat/alacritty-terminal` | `alacritty_terminal::tty` | alacritty `Term` 0.26 | Background thread |
| `feat/tui-take-screenshot` | `portable-pty` | `vt100` + `termsnap-lib` (alacritty 0.24.2) | Background thread |

Consequences if they were merged as-is:

- **Measurements and recordings would be wrong.** `rust-expect` only reads output inside `expect` / `wait_screen_stable` / `wait_timeout`. Between tool calls the app blocks once the PTY buffer fills, and asciicast timestamps show when ShadowPTY read the bytes, not when the app wrote them.
- **Up to three emulators would parse the same bytes**, each able to render a slightly different screen. Two versions of `alacritty_terminal` would be in the build.
- **Known bugs in the expect layer** (see `TODO.md`): expect matches output the agent already saw via `tui_read`; the session lock is held for the whole wait; `redact_pii` doesn't cover every output.

Measured differences between `vt100` and alacritty `Term` (same bytes, same formatter output):

| Case | `vt100` | alacritty |
|---|---|---|
| App draws a frame using synchronized output (`?2026`) and hasn't finished yet | Half-drawn frame | Last complete frame |
| Terminal shrinks | Text cut off | Text re-wrapped |
| Combining accent | Kept | **Dropped** (formatter bug, fixable) |
| Colour `38;5;1` | `<fg:red>` | `<fg:idx:1>` (formatter bug, fixable) |

## 3. Proposal

### 3.1 One emulator: alacritty `Term` 0.26

Every view of the screen comes from the same grid:

- `tui_read` text (the `formatter.rs` from `feat/alacritty-terminal`, with the two bugs above fixed);
- screen-mode expect and wait-for-stable;
- screenshots: an SVG renderer that walks the grid directly, replacing `termsnap-lib` and its second copy of alacritty.

Remove `vt100`, rust-expect's screen and `termsnap-lib`.

### 3.2 One continuous reader per session

Each session has a reader (a thread, as on `feat/alacritty-terminal`) that, for every chunk:

1. feeds the alacritty parser and `Term`;
2. writes the asciicast output event with the time the chunk was read;
3. appends the chunk to a bounded output buffer used for stream-mode expect;
4. updates metrics (§3.4);
5. bumps a revision counter and notifies waiters (`tokio::sync::watch` or `Notify`).

Tool calls never read the PTY themselves. They only look at state the reader keeps current, so no tool call holds a lock for the length of a wait.

### 3.3 Expect built on the reader

| Tool | Implementation |
|---|---|
| `tui_expect` (stream) | Search the output buffer from the session's **read position**; on a match, move the position past it. `tui_read` also moves it to the end, which fixes the stale-match bug. |
| `tui_expect` (screen) | Check the rendered screen text on each revision until it matches or times out. |
| `tui_wait_stable` | Wait until the revision stops changing for `quiet_period`, or a synchronized frame ends. |
| `tui_paste` | Write `ESC[200~ … ESC[201~`; reject text containing the end marker (same rule as `rust-expect`). |
| `tui_run_script` | send line → expect prompt, with a `timeout_ms` parameter and the echoed command stripped from the output. |

Tool names and parameters in `server.rs` stay the same. Waits have an upper limit on `timeout_ms`.

### 3.4 Metrics

Recorded per session and returned by a new `tui_metrics` tool (and optionally in `tui_end`'s result):

- time from spawn to first output byte;
- time from each input to the next stable screen;
- frame count and per-frame duration, using the start/end markers of synchronized output where the app emits them, otherwise stable-screen periods;
- total bytes in and out;
- exit status (the real code or signal, also written to the asciicast `x` event).

### 3.5 Terminal fidelity

- **Environment**: set `TERM=xterm-256color` and `COLORTERM=truecolor` for the child, overridable per session. Today no branch sets them, so the app inherits whatever the MCP host passes.
- **Terminal queries**: replace `VoidListener` with a listener that writes alacritty's replies (cursor position, device attributes, color queries such as OSC 11 background) back to the PTY, so apps detect the terminal as they would in a real one.
- **Colors in screenshots**: map indexed colors to a fixed palette, and make the palette and font configurable.

### 3.6 Sessions and teardown

- Keep multi-session support (`session_id`) from `feat/expect-feat`.
- Keep the process-group kill from `feat/alacritty-terminal` / v0.3.0, and always reap the child, including when `tui_start` replaces an existing session.
- Apply `redact_pii` in one place, to every string returned to the agent (reads, matches, script output, error messages).

## 4. What happens to `rust-expect`

Its read-only-during-calls model conflicts with §3.2, and the features ShadowPTY uses from it are small to rebuild on the reader. Drop it. If its PII redaction is worth keeping, depend on it for `pii::redact` only, or replace that with a small regex set.

## 5. Plan

1. **Base**: merge `feat/alacritty-terminal` with the two formatter fixes (combining characters, names for colors 0–15).
2. **Sessions**: port multi-session support and the session-management tools from `feat/expect-feat`.
3. **Expect**: add the read position, revision counter and output buffer to the reader; implement §3.3 and restore the expect branch's integration tests.
4. **Screenshots**: SVG renderer over the shared `Term`; port the `tui_take_screenshot` tool from `feat/tui-take-screenshot`. Detailed in [RFC-screenshots-on-shared-term.md](./RFC-screenshots-on-shared-term.md).
5. **Measure and fidelity**: metrics (§3.4), `TERM`/`COLORTERM`, terminal query replies.

Each step is a separate PR with its own tests.

## 6. Acceptance criteria

- [ ] Exactly one terminal emulator and one version of `alacritty_terminal` in `Cargo.lock`.
- [ ] An app that writes continuously for 5 s with no tool calls runs without blocking; its recording spans ~5 s.
- [ ] `tui_expect` never matches output already returned by an earlier `tui_read` or `tui_expect`.
- [ ] `tui_input` and `tui_end` return promptly while another call is waiting on the same session.
- [ ] A frame drawn inside synchronized output is never returned half-drawn by `tui_read` or the screenshot.
- [ ] `tui_read` text and the screenshot agree on every cell's character and color.
- [ ] An app querying the cursor position (`ESC[6n`) gets a reply.
- [ ] No child or grandchild process survives `tui_end`.
