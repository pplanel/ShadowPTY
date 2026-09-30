# CLAUDE.md

<!-- Same content as GEMINI.md. Keep both files in sync. -->

ShadowPTY is an MCP server (Rust, stdio) that lets agents run and test TUI applications headlessly: spawn in a PTY, send keys, read the screen with color/style tags, wait for output, take PNG/SVG screenshots, record asciicast v3. The README is for product and QA readers; tool parameters, formats and architecture are in `docs/reference.md`.

The design in force is `docs/proposals/RFC-single-emulator-core.md`; screenshots follow `docs/proposals/RFC-screenshots-on-shared-term.md`. Domain vocabulary (Screen, Cell, Tagged Text, Session, …) is in `CONTEXT.md`. Open follow-ups are in `TODO.md`.

## Commands

CI runs exactly these on macOS and Linux; run them before committing:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Benchmarks (criterion): `cargo bench`, or one group with `cargo bench -- <emulator|render|output|pty>`.

## Architecture

- `src/server.rs`: rmcp tool definitions (13 tools); thin wrappers over `PtyManager`.
- `src/pty_manager.rs`: `PtyManager`, the map of sessions keyed by `session_id` (default `"default"`).
- `src/session.rs`: `TuiSession` owns the `alacritty_terminal::tty::Pty`, one alacritty `Term`, a `SessionOutput`, an optional recorder, **one reader thread** (`run_pty_reader`) and a child-exit watcher (`watch_child_exit`, which also publishes the `ExitStatus`). Input, paste, expect, wait-gone, wait-exit, wait-stable, run-script, resize, snapshot and `terminate` live here.
- `src/output.rs`: raw output buffer (bounded, 1 MiB) with a read position and a `watch` revision counter; `Pattern`, stream expect, wait-stable.
- `src/screen.rs`: `Screen` snapshot copied from the `Term`; tagged text (`tui_read`) and plain text (screen-mode expect).
- `src/palette.rs`: base palette, app color overrides (OSC 4/10/11/12), dim/inverse/hidden resolution. Shared by text and screenshots.
- `src/screenshot.rs` (SVG) and `src/rasterizer.rs` (PNG, `fontdue` + embedded JetBrains Mono in `assets/fonts/`).
- `src/input.rs`: `<ENTER>`, `<UP>`, `<CTRL+C>`, … key tokens to bytes. `src/recorder.rs`: asciicast v3.

## Invariants

- **One emulator.** Every view of the screen (read, screen expect, screenshots) comes from the session's single alacritty `Term`. Don't add a second parser (`vt100`, `termsnap-lib`, …). Take a `Screen` snapshot under the lock and render outside it.
- **Only the reader thread reads the PTY**, continuously, so apps never block and recording timestamps are real. Waiting tools watch the revision counter.
- **No lock is held across a wait.** Clone the handles out of the session map, drop the map lock, then wait. Session teardown runs outside the map lock.
- **Stream expect never re-matches seen output**: a match consumes up to its end, and `tui_read` marks everything read.
- The reader flushes an unterminated synchronized-output frame (`?2026`) after vte's sync timeout and must wake waiters when the screen changes without new output (`SessionOutput::notify_screen_changed`).
- The reader keeps a PTY slave fd open until it's done: macOS discards unread output ~0.5 s after the last slave fd closes. Child exit is signalled by `watch_child_exit` (`waitid` with `WNOWAIT`, so `Pty`'s `Drop` still reaps).
- The exit status comes from `waitid` in the watcher and is lost once the child is reaped, and `Pty`'s `Drop` reaps it. So `TuiSession::drop` waits for the exit report before its fields drop, and the reader records the exit event after the last output.
- Stopping a session kills the whole process group and reaps the child. `terminate` waits at most `READER_JOIN_TIMEOUT` for the reader (see the open Linux issue in `TODO.md`).
- Logging goes to stderr only; stdout is the MCP JSON-RPC channel.
- The server must survive a crashing child and report errors as tool errors; no panics in library code.

## Conventions

- Lints are strict (pedantic + nursery + cargo, `unwrap_used`/`expect_used`/`panic` warn, `unsafe_code` forbid). See `clippy.toml`: max 4 arguments including `self`, at most 1 bool parameter. When a function needs more, introduce a params struct (e.g. `Expectation`, `Script`, `ReaderSinks`) instead of `#[allow]`.
- `significant_drop_tightening`: release guards with an explicit `drop(guard)`.
- MSRV 1.88, edition 2024.
- `anyhow::Result` in the session and manager; tools convert errors to `CallToolResult::error`.
- Tests default to a **43 rows × 155 cols** screen unless told otherwise.
- Integration tests use real processes (`sh -c`, `cat`); wait with `expect`/`wait_stable`, not fixed sleeps.
- Tool results are short, human-like text for the model (it relays them to the person). Machine-readable output goes to files written during the session: the asciicast recording and, once built, the JSON session report (`docs/expect-parity/ROADMAP.md`).
- New tools or parameters: update `docs/reference.md` (and the capabilities table in `README.md` if users would notice).
