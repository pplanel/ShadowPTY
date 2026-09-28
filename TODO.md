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

- [ ] README on the alacritty branches still says ShadowPTY uses `vt100` and `portable-pty`, including in the architecture diagram.
- [ ] If an app starts a synchronized-output frame and never ends it (e.g. it crashes mid-frame), the screen stays frozen until 2 MiB of output has been buffered. vte's `StdSyncHandler` never expires a frame on its own; alacritty's event loop calls `Processor::stop_sync` once `sync_timeout()` (150ms) passes, and `run_pty_reader` doesn't. Fix: read with a timeout (e.g. `poll`) and call `stop_sync` when the deadline passes.
