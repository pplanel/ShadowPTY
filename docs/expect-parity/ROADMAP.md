# Roadmap: `rust-expect` API parity

> **Status:** In progress (Phase 1)
> **Started:** 2026-09-30
> **Branch:** `feat/expect-parity`
> **Tasks:** [TODO.md](./TODO.md)

## Goal

Everything useful that [`rust-expect`](https://github.com/praxiomlabs/rust-expect) 0.6.1 (the latest release) offers should be available to agents as ShadowPTY tools.

We rebuild the features rather than depend on the crate. `rust-expect` only reads output during an expect call, which froze apps between tool calls and broke recordings (see [RFC-single-emulator-core](../proposals/RFC-single-emulator-core.md)). ShadowPTY's continuous reader and single alacritty `Term` are the foundation; parity features are built on top of them.

## Already covered (v0.4.0)

| `rust-expect` | ShadowPTY |
| :--- | :--- |
| `expect` (literal, regex) | `tui_expect` |
| `expect_screen_contains` | `tui_expect` with `screen_mode` |
| `send_paste` | `tui_paste` |
| `wait_screen_stable` | `tui_wait_stable` |
| `send`, `send_control`, key sends | `tui_input` key tokens |
| `resize_pty` | `tui_resize` |
| `transcript` recorder (asciicast) | `record_path` on `tui_start` |
| `kill` on drop, process groups | `tui_end` |

## Principles

- **Built on the reader.** Waits watch the revision counter; screen features read the shared `Term` through a `Screen` snapshot. No tool reads the PTY or holds a lock across a wait.
- **Few, clear tools.** Extend an existing tool when the meaning is the same (e.g. more patterns for `tui_expect`); add a tool when the action is different (e.g. sending a signal).
- **One item, one commit**, each with tests (real processes, 43×155 screens) and updates to [`docs/reference.md`](../reference.md) and, if users would notice, the README.
- **Stable API.** New parameters are optional with defaults that keep today's behavior.

## Phases

### Phase 1: Waiting and matching

The waits testers need most.

| Item | `rust-expect` | Proposed ShadowPTY API |
| :--- | :--- | :--- |
| Wait for exit, exit status | `expect_eof`, `wait`, `wait_timeout`, `is_running` | New `tui_wait_exit` returns exit code or signal and unread output; `tui_list_sessions` shows `exit_status`; recordings log the real exit code |
| First of several patterns | `expect_any` | `tui_expect` gains `patterns` (array); result says which one matched |
| Wait for text to disappear | `wait_screen_not_contains` | New `tui_wait_gone` (screen), e.g. spinners, "Loading…" |
| Glob patterns | `Pattern::Glob` | `tui_expect` gains `syntax: "literal" \| "regex" \| "glob"` (`is_regex` kept) |
| Text around the match | `before` / `after` | `tui_expect` returns `before` and `after` as well as `matched` |

### Phase 2: Process control and input

| Item | `rust-expect` | Proposed ShadowPTY API |
| :--- | :--- | :--- |
| Signals | `signal`, `send_interrupt`, `send_suspend` | New `tui_signal` (`INT`, `TERM`, `TSTP`, `CONT`, `HUP`, `KILL`, …) without ending the session |
| Missing keys | `send_shift_tab`, `send_function_key`, … | `tui_input` tokens: `<SHIFT+TAB>`, `<INSERT>`, and any other gaps |
| Send a line | `send_line` | `tui_input` gains `line_ending` for text that ends in a newline |
| Human-like typing | `send_human`, `HumanTyper`, `send_with_delay` | `tui_input` gains `delay_ms` (and optional jitter) between keys |

### Phase 3: Screen queries

| Item | `rust-expect` | Proposed ShadowPTY API |
| :--- | :--- | :--- |
| Find text with position | `find`, `find_all`, `find_regex` | New `tui_find` returns `[{row, col, text}]` |
| Part of the screen, cursor | `region_text`, `row_text`, `line` | `tui_read` gains `rows` / `region`; cursor position in `tui_find`/`tui_read` output |
| What changed | `diff`, `changes`, `changed_rows` | `tui_read` gains `changes_only` (rows changed since the last read) |
| Scrollback | `attach_screen_with_scrollback` | `tui_read` gains `scrollback_lines` |

### Phase 4: Scripted interactions

| Item | `rust-expect` | Proposed ShadowPTY API |
| :--- | :--- | :--- |
| Dialogs | `Dialog`, `run_dialog` | New `tui_dialog`: declarative expect → send steps with branches (answer "y" if asked, fail on "error") |
| Expect across sessions | `multi::select` | `tui_expect` gains `session_ids`; returns which session matched |
| Shell and prompt detection | `auto_config` | `tui_run_script` picks the prompt for bash/zsh/fish when `prompt_pattern` is omitted |

### Phase 5: Other

| Item | `rust-expect` | Notes |
| :--- | :--- | :--- |
| PII redaction | `pii-redaction` | One redaction pass on everything returned to the agent |
| Session metrics | `metrics` | Overlaps RFC step 5 (time to first output, time to stable, bytes, frames) |
| Transcript playback | `transcript::Player` | Read/compare an asciicast baseline |
| Non-UTF-8 output | `legacy-encoding` | Decode legacy encodings before the emulator |
| SSH sessions | `ssh` | Decide if in scope |

### Not planned

- `interact` (hands the terminal to a human): no MCP equivalent.
- `mock`, `test_utils`: library-only test helpers.

## Open questions

1. **Tool count vs. parameters.** Phase 3 adds several options to `tui_read`. Is one flexible `tui_read` better for agents than separate `tui_find` / `tui_read_region` tools?
2. **`tui_expect` result format.** Returning `before` / `after` / `index` suggests a JSON result instead of today's text. Switch now (small break) or add a `format` option?
3. **Glob vs. `is_regex`.** Replace `is_regex` with `syntax`, or keep both?
4. **SSH.** In scope for a PTY-focused MCP server?

## Done when

- Every Phase 1–4 item is a documented tool or parameter with tests.
- Phase 5 items are either done or explicitly deferred here with a reason.
- The parity table in this file maps every `rust-expect` feature to a ShadowPTY tool or to "not planned".
