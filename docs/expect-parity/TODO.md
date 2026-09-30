# TODO: `rust-expect` API parity

Task list for [ROADMAP.md](./ROADMAP.md). Each item ships as one commit with tests (real processes, 43×155) and updates to [`docs/reference.md`](../reference.md) (and the README when users would notice).

Legend: `[ ]` to do, `[~]` in progress, `[x]` done.

## Phase 1: Waiting and matching

### 1.1 Wait for exit and exit status `[x]`
- [x] Recorder writes the exit event only once (`AsciicastRecorder::record_exit`).
- [x] `ExitStatus` (`exit_code` / `signal` / `unknown`), captured by `watch_child_exit` from `waitid` (still `WNOWAIT`) into a `watch` channel.
- [x] Reader records the real exit status after draining output, before closing the stream; killed processes record `128 + signal`.
- [x] Remove the hard-coded exit code `0` from `terminate` and `Drop`.
- [x] `TuiSession::exit_status()` and `wait_exit(timeout)` → status + unread output (marks it read).
- [x] `PtyManager::wait_exit_session` / `wait_exit`.
- [x] `tui_wait_exit { session_id?, timeout_ms? = 10000 }`.
- [x] `tui_list_sessions` includes `exit_status`.
- [x] `TuiSession::drop` waits for the exit report before `Pty` reaps the child (Linux lost the status 13/30 runs without it).
- [x] Tests (`tests/exit_test.rs`): exit code, killed by signal, timeout while running, recording ends with the real code, recording after `tui_end`, tools.
- [x] Docs: reference (new tool, list output), README capabilities; tick the recording item in the root `TODO.md`.

### 1.2 First of several patterns `[ ]`
- [ ] `SessionOutput::expect_any(&[Pattern])`: earliest match in unread output wins; ties go to the first pattern.
- [ ] Screen mode: earliest match in screen text.
- [ ] `tui_expect` accepts `patterns` (array) as an alternative to `pattern`; exactly one must be given.
- [ ] Result names the pattern index and text that matched.
- [ ] Tests: stream and screen, tie-breaking, invalid combinations.

### 1.3 Wait for text to disappear `[ ]`
- [ ] `TuiSession::wait_gone(pattern, timeout)` on the screen text, re-checked on each revision.
- [ ] `tui_wait_gone { pattern, is_regex?, timeout_ms?, session_id? }`.
- [ ] Succeeds immediately if the text isn't on screen; if the process exits, checks once more.
- [ ] Tests: spinner that clears, text that never clears (timeout, shows screen).

### 1.4 Glob patterns `[ ]`
- [ ] `Pattern::glob` (`*`, `?`, `[...]`) compiled to a regex.
- [ ] `syntax` parameter on `tui_expect`, `tui_wait_gone`, `tui_run_script`; `is_regex` still accepted.
- [ ] Tests: glob matching, conflicting `syntax` + `is_regex`.

### 1.5 Text around the match `[ ]`
- [x] Decide the result format (ROADMAP open question 2): text in the tool reply; structured data in the session report.
- [ ] Stream mode returns `before` and `after` (rest of the unread output); screen mode returns the matching line.
- [ ] Tests.

## Session report (JSON)

After Phase 1; see [ROADMAP](./ROADMAP.md#session-report-json).

- [ ] Report writer (JSON Lines, flushed per entry) behind a `report_path` on `tui_start`.
- [ ] Entries: session start (command, size), inputs, each expectation (patterns, target, timeout, outcome, elapsed ms, matched text), waits, screenshots (path/format), exit status.
- [ ] Summary entry on `tui_end` / process exit: checks run, passed, failed, exit code.
- [ ] Share timing with session metrics (5.2) where they overlap.
- [ ] Tests: passing and failing checks, crash mid-session still leaves valid lines, summary counts.
- [ ] Docs: reference (format and fields), README (TDD/CI section).

## Phase 2: Process control and input

- [ ] 2.1 `tui_signal { signal, session_id? }`: named signals to the process group; the session stays open.
- [ ] 2.2 Key tokens: `<SHIFT+TAB>`, `<INSERT>`; audit `rust-expect`'s `send_*` list against `input.rs`.
- [ ] 2.3 `tui_input` `line_ending` (`cr`, `lf`, `crlf`).
- [ ] 2.4 `tui_input` `delay_ms` (+ optional jitter) for human-like typing; recording shows real timing.

## Phase 3: Screen queries

- [ ] Resolve ROADMAP open question 1 (tool count vs. parameters).
- [ ] 3.1 `tui_find { pattern, syntax?, session_id? }` → matches with row/col; include cursor position.
- [ ] 3.2 Read part of the screen: rows or a region.
- [ ] 3.3 Changes since the last read (changed rows only).
- [ ] 3.4 Scrollback lines (Term history) in reads.

## Phase 4: Scripted interactions

- [ ] 4.1 `tui_dialog`: declarative steps (expect → send), branches, failure patterns, per-step timeout.
- [ ] 4.2 Expect across several sessions (`session_ids`), returning the matching session.
- [ ] 4.3 Shell/prompt detection for `tui_run_script` when `prompt_pattern` is omitted (bash, zsh, fish).

## Phase 5: Other

- [ ] 5.1 PII redaction on every string returned to the agent (see root `TODO.md`, Correctness).
- [ ] 5.2 Session metrics (with RFC step 5).
- [ ] 5.3 Asciicast playback / baseline comparison.
- [ ] 5.4 Legacy (non-UTF-8) encodings.
- [ ] 5.5 Decide on SSH sessions.

## Housekeeping

- [ ] Keep the parity table in ROADMAP.md current as items land.
- [ ] Release notes per phase.
