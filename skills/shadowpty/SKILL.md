---
name: shadowpty
description: Drive and test terminal programs (TUIs, CLIs, REPLs, shells) with the ShadowPTY MCP tools (tui_start, tui_input, tui_expect, tui_read, tui_take_screenshot, ...). Use when asked to run, test, demo, debug or screenshot an interactive terminal app (htop, vim, a ratatui/bubbletea/textual app, a CLI wizard, a REPL), to check what it shows or how it reacts to keys, resizes or signals, or to record a terminal session.
---

# Testing terminal apps with ShadowPTY

ShadowPTY runs a program in a real pseudo-terminal and lets you type into it and read its screen. Every tool takes an optional `session_id` (default `"default"`); use distinct ids to run several programs at once.

## The loop

1. **Start**: `tui_start` with `command` and `args`. Pick a size that fits the app (`rows`, `cols`; default 24×80). Starting an id that already exists replaces that session.
2. **Act**: `tui_input` sends keys and text with tokens such as `<ENTER>`, `<ESC>`, `<UP>`, `<TAB>`, `<CTRL+C>`, `<ALT+X>`, `<F1>`. `tui_paste` sends multi-line text as one bracketed paste, so editors and shells don't treat each line as Enter.
3. **Wait, never sleep.** Pick the wait that matches what you expect:
   - text should **appear**: `tui_expect`
   - text should **disappear** (spinner, `Loading…`, a modal): `tui_wait_gone`
   - the app should **settle** before you look: `tui_wait_stable`
   - the process should **end**: `tui_wait_exit` (exit code or signal, plus unread output)
4. **Look**: `tui_read` returns the screen as text with style tags (`<fg:red>`, `<bold>`, `<inverse>`, …). Use `tui_take_screenshot` when layout, color or alignment matters, or to show the person (PNG inline, or SVG; `output_path` saves a file).
5. **End**: `tui_end` for every session you started, even after the process exited. It kills the whole process group and finishes recordings and reports.

## Choosing how `tui_expect` matches

- **Stream mode (default)** searches only output you haven't seen. A match consumes output up to its end, and `tui_read` marks everything on screen as seen. So expect *after* the input that causes the output, and don't `tui_read` between an input and the `tui_expect` for its result.
- **`screen_mode: true`** matches what is drawn right now. Use it for full-screen apps that redraw with cursor moves (the text may never appear contiguously in the stream), and for checking a state that might already be showing. The reply gives the row.
- **`patterns: [...]`** waits for whichever comes first and says which one won. Use it for branches: `["Password:", "Permission denied", "$ "]`.
- **`syntax`**: `"literal"` (default), `"regex"`, or `"glob"` (`*` and `?` stay within a line). See [references/patterns.md](references/patterns.md).
- Set `timeout_ms` for slow steps (builds, network); waits are capped at 120 s. Expect fails at once if the process exits first, so a failed expect usually comes with the output that explains why.
- `include_context: true` shows output around a stream match. Use it when you need the lines near the match without consuming them.

`tui_wait_gone` returns at once if the text isn't showing yet. If it might not have appeared, `tui_expect` it with `screen_mode` first, then wait for it to go.

## Shells and commands

- To run shell commands and get each one's output, start a shell (`sh`, `bash`) and use `tui_run_script` with `commands`. It waits for the prompt after each and stops at the first that doesn't return. Set `prompt_pattern` to match the real prompt; for zsh or fancy prompts, set a plain one first (`PS1='READY> '`).
- Use `tui_input` plus `tui_expect` instead when a command is interactive (asks questions, opens a TUI).

## Signals, keys and resizes

- `<CTRL+C>` in `tui_input` only becomes SIGINT while the app leaves keyboard signals on; raw-mode TUIs read it as a key. To be sure a signal is delivered, use `tui_signal` (`INT`, `TERM`, `HUP`, `STOP`/`CONT`, `KILL`, …). By default it goes to the foreground process group, as Ctrl+C would; `target: "process"` hits only the program `tui_start` launched.
- After a signal, check the effect with `tui_expect` or `tui_wait_exit`.
- `tui_resize` tests re-layout; follow it with `tui_wait_stable`, then read or screenshot.

## Recordings, reports and the live view are for the person

- `record_path` writes an asciicast v3 recording (`asciinema play file.cast`), `report_path` writes a JSON Lines report of every input and check, and `live: true` returns a local link where the person can watch.
- Pass these paths and links on to the person; don't open the live link yourself.
- If you pass none of the three, ShadowPTY may ask the person once how they want sessions captured, and applies the answer to later sessions.

## Pitfalls

- **Sleeping or polling `tui_read` in a loop.** Use a wait tool instead; they return as soon as the condition holds.
- **Reading too early.** A full-screen app may still be drawing. `tui_wait_stable` (or a screen-mode expect for a known landmark) before `tui_read` or a screenshot.
- **Expecting stale text.** Stream expect never re-matches what you've already seen; use `screen_mode` to check what's on screen now.
- **Matching the echo of your own input.** Typing `echo done` puts `done` in the stream twice. Expect something only the result contains, or use `tui_run_script`, which skips the echo.
- **Forgetting `tui_end`.** Leaving sessions running leaks processes. `tui_list_sessions` shows what is still open.
- **Assuming the exit code.** After the app should quit, `tui_wait_exit` reports how it actually ended.

## Example: check that `htop` quits on `q`

```
tui_start            { "command": "htop", "rows": 40, "cols": 120 }
tui_expect           { "pattern": "F10Quit", "screen_mode": true }
tui_take_screenshot  {}
tui_input            { "keys": "q" }
tui_wait_exit        {}
tui_end              {}
```

More: key tokens, pattern syntax and screen tags in [references/patterns.md](references/patterns.md).
