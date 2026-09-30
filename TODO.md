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

- [x] Record the real exit code or signal: the exit watcher captures the status and the reader records it after the last output (`feat/expect-parity`, parity item 1.1).
- [x] Pick one terminal emulator: alacritty `Term` only.
- [x] rust-expect-specific issues (`SessionBuilder` arguments, `pid()` returning `0`, missing process-group kill, redundant `#[serde(default)]`): gone with rust-expect.

## New

- [x] README still said ShadowPTY uses `vt100` and `portable-pty`: rewritten, with the technical reference in `docs/reference.md` (#11).
- [x] An unterminated synchronized-output frame froze the screen: the reader now polls with the sync deadline and calls `Processor::stop_sync` when it passes (#7).
- [x] macOS lost the output of short-lived processes (it discards unread PTY output ~0.5s after the last slave fd closes): the reader keeps a slave fd open and a `waitid(WNOWAIT)` watcher signals child exit (#7, `session.rs` in #10).
- [ ] **Reader thread sometimes doesn't stop after the child is killed (Linux)**: `test_session_bracketed_paste` (`sh -i`) intermittently hung in `TuiSession::terminate` waiting on the reader, only on Linux with the test suite running in parallel (1 in 5 runs in a podman container; never on macOS). The cause is unknown: `terminate` kills the process group, which should wake the exit watcher (`watch_child_exit`) or give the reader EOF. Currently bounded by `READER_JOIN_TIMEOUT` (2s) plus a SIGKILL to the pid, which can leave the reader thread running. To investigate: log from `watch_child_exit` and `next_event` in tests, check whether `waitid` returns and whether the exit pipe's write end leaks into concurrently spawned children, then catch a hang and inspect `/proc/<pid>/task/*/wchan`.

## API parity with `rust-expect`

In progress on `feat/expect-parity`: plan in [`docs/expect-parity/ROADMAP.md`](docs/expect-parity/ROADMAP.md), tasks in [`docs/expect-parity/TODO.md`](docs/expect-parity/TODO.md).

## Live viewer

- [ ] **Feed report events to the live viewer** once `feat/session-report` merges: implement `forward_report_events` in `src/live/mod.rs` with `PtyManager::subscribe_report_session` (the loop is sketched in its `TODO(session-report)` comment) and drop its `#[allow(clippy::unused_async)]`. Subscribing happens right after `tui_start` spawns the session, so the report's `start` event is only seen if the report replays it or `start_session` returns the receiver. Then add an integration test that runs `tui_expect` on a live session and receives a `report` event with `passed`. The page already handles every event type (`tests/fixtures/live_report_events.jsonl`).
