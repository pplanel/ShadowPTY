# 06 · Capture form that never blocks `tui_start`

**Feature name:** `capture-form`
**Commits:** `29c0c53` (lock and retry fixes), `51f39d8` (redesign)

## What was implemented

When `tui_start` sets none of `record_path`, `report_path` and `live`, the person decides how the session is captured, through an MCP elicitation form:

- `tui_start` **never waits** for the person. The session starts at once and records, with a report, to the default files `<server cwd>/<command>.cast` and `<command>.report.jsonl` (next free name if taken: `sh-2.cast`, …).
- **Protocol 2026-07-28 (Claude Code):** `tui_end` returns the form ("The assistant is ending `<command>` …", fields **Keep the recording**, **Recording file**, **Keep the report**, **Report file**). The answer moves or deletes the files; closing the form keeps them.
- **Older protocols:** the form goes out in the background as the session starts and also offers **Watch live**. No answer within 30 s → defaults; a `tui_end` while it's open waits for it.
- **Defaults** (declined, closed, unanswered, or no form support): keep both files. Declined → no reminders; closed/no forms → the agent gets a one-time hint about the other options.
- The answer (or the defaults) applies to every later session of that server process, from its start. A `tui_start` that sets any capture field is captured exactly as asked and never involves the person.
- On 2026-07-28, a retry with the form's `requestState` but no answer counts as closed, and a second session ending while a form is out keeps its defaults without a second form.

With the README's MCP config the server runs in `<repo>/mcp-tests/auto-test` (`<dir>` below), so the default files land there.

## Setup

- `!rm -f <dir>/sh*.cast <dir>/sh*.report.jsonl <dir>/capture-form-kept*.cast` before each scenario: default names depend on what already exists.
- The capture decision lives in the server process. **Each scenario opens its own Claude Code session** (a new `shadowpty-dev` process), as in the [README](README.md#opening-a-test-session), with `session_id` and file names `capture-form-<scenario>`, e.g. `capture-form-a-test.cast` and `capture-form-a-test.report.jsonl`.
- The prompts below ask Claude to call the tools exactly as written. Approve each tool call when Claude Code asks. Read every tool result Claude Code shows in the transcript (expand it if collapsed) and compare it with the expected text.

The start prompt used below, `START`:

> Use the shadowpty-dev MCP server. Call tui_start with exactly {"command": "sh", "args": ["-c", "echo capture-check; sleep 120"], "session_id": "s1"} and no other arguments. Show me the tool result verbatim. Don't call any other tool.

The end prompt, `END`: `Now call shadowpty-dev's tui_end with {"session_id": "s1"} and show me the result verbatim.`

## Scenario A: answer the form at `tui_end`, choosing other files

1. Send `START`. Assert the result arrives without any form appearing, and reads:
   `Started command 'sh' in PTY session 's1' (… recording to '<dir>/sh.cast', reporting to '<dir>/sh.report.jsonl')` followed by `When this session ends, tui_end asks the person whether to keep the recording and report and where; …`
2. `!ls <dir>/sh.cast <dir>/sh.report.jsonl`: both exist while the session runs.
3. Send `END`. `tui_expect` (`screen_mode: true`) for `is ending` (the form's message). `tui_take_screenshot` → `<dir>/capture-form-a-form.png`.
4. Assert the form shows the four fields, with **Recording file** = `<dir>/sh.cast` and **Report file** = `<dir>/sh.report.jsonl`, and **no "Watch live"** field.
5. Fill it: **Keep the recording** on, **Recording file** = `<dir>/capture-form-kept.cast`, **Keep the report** off. Use the dialog's own keys (read the hint line at the bottom of the form; typically arrows/Tab to move, Space to toggle, typing to edit, Enter to submit). Submit.
6. Assert the `tui_end` result: `Terminated session 's1' … Recording saved to '<dir>/capture-form-kept.cast'. Report deleted, as the person chose`.
7. `!ls <dir>`: `capture-form-kept.cast` exists; `sh.cast` and `sh.report.jsonl` don't.
8. Later sessions use the answer without asking: send `START` again with `"session_id": "s2"`. Assert `recording to '<dir>/capture-form-kept-2.cast'`, no `reporting to`, no "When this session ends" note. Send `END` for `s2`: no form appears; the result is a plain `Terminated session 's2' …` (the session wasn't waiting for a decision, so there's no "Recording saved" line), and `!ls <dir>/capture-form-kept-2.cast` shows the file.

## Scenario B: close the form

New Claude Code session (`capture-form-b`), clean `<dir>` first.

1. `START`, then `END`. When the form appears, press `<ESC>` to close it.
2. Assert the result: `Recording saved to '<dir>/sh.cast'. Report '<dir>/sh.report.jsonl': …`, and both files exist.
3. `START` with `"session_id": "s2"`: `recording to '<dir>/sh-2.cast', reporting to '<dir>/sh-2.report.jsonl'` plus the hint `These are the default files. … Ask the person whether they want any of these.`
4. `START` with `"session_id": "s3"`: same files pattern (`sh-3…`), **without** the hint. End `s2` and `s3`; no form appears.

## Scenario C: decline the form

New session (`capture-form-c`), clean `<dir>`. As B, but choose the form's decline option instead of closing it (if this Claude Code version offers one; otherwise note "no decline button" in the result file and skip). Assert the files are kept as in B.2, and that the next `START` has **no** hint.

## Scenario D: explicit capture never asks

New session (`capture-form-d`). Prompt: `Use shadowpty-dev: tui_start with exactly {"command": "sh", "args": ["-c", "sleep 120"], "session_id": "s1", "record_path": "<dir>/capture-form-explicit.cast"}, then tui_end {"session_id": "s1"}. Show both results verbatim.`

Assert: `tui_start` says `recording to '<dir>/capture-form-explicit.cast'` with no `reporting to` and no note; `tui_end` shows no form and doesn't mention "Recording saved"; no `sh.cast` was created.

## Scenario E: older protocols, the background form (automated)

Claude Code only speaks 2026-07-28, so the background path is checked by the integration tests with a real rmcp client on both lifecycles:

`!cd <repo> && cargo test --test capture_test`

Expect 7 passing tests, including `test_background_form_does_not_block_start_and_its_answer_moves_the_files`, `test_unanswered_background_form_keeps_the_defaults`, `test_retry_without_an_answer_keeps_the_defaults` and `test_explicit_start_captures_only_what_it_asks_for`. Also `!cd <repo> && cargo test --lib capture::` (unit tests of the plan/settle logic).

Optional manual check with a client on an older protocol (e.g. the MCP Inspector in a browser): call `tui_start` with only `command: "sh"` and `args: ["-c","sleep 120"]`; the result comes back at once while the form (with **Watch live**) is shown; turning on **Watch live** opens the live page in the browser; not answering for 30 s and then calling `tui_end` keeps `sh.cast` and `sh.report.jsonl`.

## Assertions

1. A.1: `tui_start` returns without waiting and records to the default files; no form at start.
2. A.3–A.4: `tui_end` shows the end form with the four fields and the session's current files as defaults, without "Watch live".
3. A.6–A.7: the recording is moved to the chosen file, the report deleted, and the reply says so.
4. A.8: later sessions take the answer from the start and aren't asked again.
5. B: closing keeps both files; the hint appears exactly once afterwards.
6. C (if available): declining keeps both files, with no hint.
7. D: explicit capture never shows a form or creates default files.
8. E: all capture tests pass.

## Close

End each Claude Code session as in the [README](README.md#closing-a-test-session). Then remove the scenario's default files from `<dir>` (`sh*.cast`, `sh*.report.jsonl`, `capture-form-kept*.cast`, `capture-form-explicit.cast`) unless you want to keep them as evidence.
