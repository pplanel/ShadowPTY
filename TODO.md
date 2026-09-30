# TODO

Follow-ups from the code review of `feat/expect-feat` (rust-expect integration), 2026-09-28.

The plan is in `docs/proposals/RFC-single-emulator-core.md`. The work is stacked on `feat/alacritty-terminal` → `feat/alacritty-multi-session` → `feat/alacritty-expect`, which replaces the rust-expect session layer. Items marked done are fixed on that stack.

## Before merge

- [x] **Stale `tui_expect` matches after `tui_read`**: stream expect now starts at a read position that `tui_read` moves to the end (`feat/alacritty-expect`).
- [x] **Long waits block other calls**: waits no longer hold any lock, starting an existing id tears the old session down outside the map lock, and waits are capped at 120s (`feat/alacritty-multi-session`, `feat/alacritty-expect`).

## Design

- [x] **Bring back a background reader per session**: kept from `feat/alacritty-terminal`; expect and wait-stable are built on it.

## Correctness

- [x] **Restarting a session doesn't clean up the old process**: the old session is killed and reaped (`feat/alacritty-multi-session`).
- [ ] **`redact_pii` isn't ported yet**: the flag only existed on `feat/expect-feat` (via rust-expect). When it comes back, redact in one place on everything returned to the agent: screen reads, expect matches, script output and error messages.
- [x] **`tui_run_script` matching its own echo**: the echo of each command is skipped and `timeout_ms` was added (`feat/alacritty-expect`). The default prompt is still `"$"`, which doesn't fit zsh (`%`).

## Smaller issues

- [ ] Record the real exit code or signal: `TuiSession`'s `Drop` still writes exit code `0` to the recording.
- [x] Pick one terminal emulator: alacritty `Term` only.
- [x] rust-expect-specific issues (`SessionBuilder` arguments, `pid()` returning `0`, missing process-group kill, redundant `#[serde(default)]`): gone with rust-expect.

## New

- [x] README still said ShadowPTY uses `vt100` and `portable-pty`: rewritten, with the technical reference in `docs/reference.md` (#11).
- [x] An unterminated synchronized-output frame froze the screen: the reader now polls with the sync deadline and calls `Processor::stop_sync` when it passes (#7).
- [x] macOS lost the output of short-lived processes (it discards unread PTY output ~0.5s after the last slave fd closes): the reader keeps a slave fd open and a `waitid(WNOWAIT)` watcher signals child exit (#7, `session.rs` in #10).
- [ ] **Reader thread sometimes doesn't stop after the child is killed (Linux)**: `test_session_bracketed_paste` (`sh -i`) intermittently hung in `TuiSession::terminate` waiting on the reader, only on Linux with the test suite running in parallel (1 in 5 runs in a podman container; never on macOS). The cause is unknown: `terminate` kills the process group, which should wake the exit watcher (`watch_child_exit`) or give the reader EOF. Currently bounded by `READER_JOIN_TIMEOUT` (2s) plus a SIGKILL to the pid, which can leave the reader thread running. To investigate: log from `watch_child_exit` and `next_event` in tests, check whether `waitid` returns and whether the exit pipe's write end leaks into concurrently spawned children, then catch a hang and inspect `/proc/<pid>/task/*/wchan`.

## API parity with `rust-expect`

Goal: everything useful that `rust-expect` 0.6 offered on `feat/expect-feat` should be available as ShadowPTY tools, built on the continuous reader and the shared `Term` (not by bringing the crate back). Already covered: literal/regex expect, screen-mode expect, bracketed paste, wait-for-stable, shell scripts, resize, sessions, asciicast recording, process-group kill.

### Expect
- [ ] **Several patterns at once** (`expect_any`): wait for the first of N patterns and return which one matched (e.g. `["Password:", "Permission denied", "$ "]`).
- [ ] **Wait for exit** (`expect_eof`, `wait`, `wait_timeout`, `is_running`): wait until the process exits and return its exit code or signal. Also fixes recordings always logging exit code `0`.
- [ ] **Glob patterns** (`Pattern::Glob`) next to literal and regex.
- [ ] **Return the text after the match**, not just `matched` / `before`.

### Screen
- [ ] **Wait for text to disappear** (`wait_screen_not_contains`): e.g. a spinner or "Loading…".
- [ ] **Find text with its position** (`find`, `find_all`, `find_regex`): return row/column, so tests can assert where something is drawn.
- [ ] **Read part of the screen** (`region_text`, `row_text`, `line`): a region or single row instead of the whole screen; also the cursor position.
- [ ] **What changed** (`diff`, `changes`, `visual_diff`, `changed_rows`): screen diff since the last read or snapshot.
- [ ] **Scrollback** (`attach_screen_with_scrollback`, `on_screen_line_scrolled_out`): read lines that scrolled off the top.

### Input and process control
- [ ] **Signals** (`signal`, `kill`, `send_interrupt`, `send_suspend`): send SIGINT/SIGTERM/SIGTSTP/… to the app without closing the session.
- [ ] **Missing key tokens**: Shift+Tab (`send_shift_tab`), Insert, and any other keys `send_*` covers that `input.rs` doesn't.
- [ ] **`send_line`** with a configurable line ending (`\r`, `\n`, `\r\n`).
- [ ] **Human-like typing** (`send_human`, `HumanTyper`, `send_with_delay`): per-key delays (and optional typos) for realistic recordings and apps that react to typing speed.

### Scripted interactions
- [ ] **Dialogs** (`Dialog`, `run_dialog`): a declarative list of expect → send steps (with branches, e.g. answer "y" if asked, fail on "error") run in one call. Generalizes `tui_run_script` beyond shells.
- [ ] **Expect across sessions** (`multi::select`): wait for the first of several sessions to match.
- [ ] **Better shell defaults** (`auto_config`: shell, prompt and line-ending detection): pick the prompt pattern for bash/zsh/fish instead of the fixed `"$"`.

### Other
- [ ] **PII redaction** (`pii-redaction` feature): see the `redact_pii` item under Correctness.
- [ ] **Session metrics** (`metrics`, `SessionMetrics`): overlaps with RFC step 5 (time to first output, time to stable, bytes, frames).
- [ ] **Transcript playback** (`transcript::Player`): read or replay an asciicast file, e.g. to compare a run against a recorded baseline.
- [ ] **Non-UTF-8 output** (`legacy-encoding` feature).
- [ ] **SSH sessions** (`ssh` feature, `russh`): drive a remote host. Decide whether it's in scope.
- Not planned: `interact` (hands the terminal to a human) and `mock`/`test_utils` (library-only helpers with no MCP equivalent).
