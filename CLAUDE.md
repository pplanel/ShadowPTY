# CLAUDE.md

ShadowPTY is an MCP server (Rust, stdio) that lets agents run and test TUI applications headlessly: spawn in a PTY, send keys, read the screen with color/style markup, wait for output, record asciicast v3.

`AGENT.md` is the original brief and is outdated (it still describes `vt100`, `portable-pty` and four tools). The design in force is `docs/proposals/RFC-single-emulator-core.md`; screenshots follow `docs/proposals/RFC-screenshots-on-shared-term.md`. Open follow-ups are in `TODO.md`.

## Commands

CI runs exactly these; run them before committing:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Benchmarks (criterion): `cargo bench`, or one group with `cargo bench -- <emulator|render|output|pty>`.

## Architecture

- `src/pty_manager.rs`: `PtyManager` holds sessions keyed by `session_id` (default `"default"`). Each `TuiSession` owns an `alacritty_terminal::tty::Pty`, one alacritty `Term`, a `SessionOutput`, an optional recorder, and **one reader thread** (`run_pty_reader`) that feeds every chunk to the parser, the output buffer and the recorder.
- `src/output.rs`: raw output buffer (bounded, 1 MiB) with a read position and a `watch` revision counter. `tui_expect`, `tui_wait_stable` and `tui_run_script` wait on the revision; they never read the PTY.
- `src/formatter.rs`: `format_screen` (markup for `tui_read`) and `screen_text` (plain text for screen-mode expect).
- `src/server.rs`: rmcp tool definitions; thin wrappers over `PtyManager`.
- `src/input.rs`: `<ENTER>`, `<UP>`, … key tokens to bytes. `src/recorder.rs`: asciicast v3.

## Invariants

- **One emulator.** Every view of the screen (read, expect, screenshots) comes from the session's single alacritty `Term`. Don't add a second parser (`vt100`, `termsnap-lib`, …).
- **Only the reader thread reads the PTY**, continuously, so apps never block and recording timestamps are real.
- **No lock is held across a wait.** Clone the handles out of the session map, drop the map lock, then wait. Session teardown runs outside the map lock.
- **Stream expect never re-matches seen output**: a match consumes up to its end, and `tui_read` marks everything read.
- The reader flushes an unterminated synchronized-output frame (`?2026`) after vte's sync timeout and must wake waiters when the screen changes without new output (`SessionOutput::notify_screen_changed`).
- Stopping a session kills the whole process group and reaps the child.
- Logging goes to stderr only; stdout is the MCP JSON-RPC channel.
- The server must survive a crashing child and report errors as tool errors; no panics in library code.

## Conventions

- Lints are strict (pedantic + nursery + cargo, `unwrap_used`/`expect_used`/`panic` warn, `unsafe_code` forbid). See `clippy.toml`: max 4 arguments including `self`, at most 1 bool parameter. When a function needs more, introduce a params struct (e.g. `Expectation`, `Script`, `ReaderSinks`) instead of `#[allow]`.
- `significant_drop_tightening`: release guards with an explicit `drop(guard)`.
- MSRV 1.88, edition 2024.
- `anyhow::Result` in the manager; tools convert errors to `CallToolResult::error`.
- Tests default to a **43 rows × 155 cols** screen unless told otherwise.
- Integration tests use real processes (`sh -c`, `cat`); wait with `expect`/`wait_stable`, not fixed sleeps.
