# ShadowPTY feature test plans

One plan per feature or improvement made on `feat/mcp-best-practices`. Each plan says how to check that the feature is implemented and correct: what to set up, which steps to run, and what to assert. An agent (or a person) runs them with ShadowPTY itself, driving **Claude Code inside a ShadowPTY session**, which in turn talks to the ShadowPTY build under test.

| Plan | Feature | Commit |
| :--- | :--- | :--- |
| [01-tool-annotations-and-instructions.md](01-tool-annotations-and-instructions.md) | Tool titles, behavior hints, server instructions, server name and version | `092303d` |
| [02-mcpb-bundle.md](02-mcpb-bundle.md) | MCPB bundle for Claude Desktop | `092303d` |
| [03-skill-and-plugin.md](03-skill-and-plugin.md) | ShadowPTY skill and Claude Code plugin marketplace | `e0a2360` |
| [04-client-capability-logging.md](04-client-capability-logging.md) | Logging what the connected client supports | `59454e7` |
| [05-input-schemas-without-null.md](05-input-schemas-without-null.md) | Optional parameters without a `null` type | `899a897` |
| [06-capture-form.md](06-capture-form.md) | Capture form that never blocks `tui_start` | `29c0c53`, `51f39d8` |
| [07-live-viewer-mcp-app.md](07-live-viewer-mcp-app.md) | Live viewer as an MCP App (RFC + spike) | `e68994f`, branch `feat/live-viewer-mcp-app` |

## Conventions

- `<repo>` is the absolute path of the repository root. Use absolute paths in every tool call: the ShadowPTY server that runs the test may have a different working directory.
- Every artifact goes in `<repo>/mcp-tests/auto-test/`, named after the plan's feature:
  - `<feature>-test.cast`: the asciicast recording of the test session (replay with `asciinema play`). The extension is `.cast`, the asciicast v3 format ShadowPTY writes.
  - `<feature>-test.report.jsonl`: the ShadowPTY session report (every input and check, pass/fail, summary).
  - Anything else a plan saves (screenshots, logs, captured files) uses the same `<feature>-` prefix.
- Never use the live viewer: always pass `live: false`.
- Terminal size: 43 rows × 155 columns, so Claude Code's dialogs fit.
- Wait with `tui_expect` / `tui_wait_stable`, never with fixed sleeps.

## One-time setup

1. Build the server under test:

   ```sh
   cd <repo> && cargo build --release
   ```

2. Write `<repo>/mcp-tests/auto-test/shadowpty-dev.mcp.json`. It starts the build under test as an MCP server named `shadowpty-dev`, in the test folder (so its default capture files land there), and appends its stderr log to `server-stderr.log`:

   ```json
   {
     "mcpServers": {
       "shadowpty-dev": {
         "command": "/bin/sh",
         "args": ["-c", "cd '<repo>/mcp-tests/auto-test' && exec '<repo>/target/release/shadowpty' 2>>server-stderr.log"],
         "env": { "RUST_LOG": "info" }
       }
     }
   }
   ```

3. Start every plan with an empty `server-stderr.log` (`: > <repo>/mcp-tests/auto-test/server-stderr.log`) so its assertions only see that run.

## Opening a test session

Unless a plan says otherwise, open Claude Code inside ShadowPTY like this, replacing `<feature>`:

```json
{
  "command": "claude",
  "args": ["--strict-mcp-config", "--mcp-config", "<repo>/mcp-tests/auto-test/shadowpty-dev.mcp.json"],
  "rows": 43,
  "cols": 155,
  "session_id": "<feature>",
  "record_path": "<repo>/mcp-tests/auto-test/<feature>-test.cast",
  "report_path": "<repo>/mcp-tests/auto-test/<feature>-test.report.jsonl",
  "live": false
}
```

Then:

1. `tui_wait_stable` (`quiet_period_ms: 1500`, `max_wait_ms: 30000`).
2. If the screen asks whether to trust the folder, answer yes (`<ENTER>` on the highlighted "Yes" option) and wait again.
3. `tui_expect` (`screen_mode: true`) for `Claude Code`: the prompt is ready.

`--strict-mcp-config` loads only `shadowpty-dev`, so every MCP check talks to the build under test.

### Useful Claude Code moves

- Type a slash command: `tui_input` with the command text, then `<ENTER>`. `/mcp` opens **Manage MCP servers**; `shadowpty-dev` is listed with `14 tools`. Select it with arrows and `<ENTER>`; **View tools** lists the tools.
- Run a shell command in the Claude Code session: type `!` followed by the command, then `<ENTER>`. The output shows in the transcript (and in the recording).
- Ask Claude to use the server: type the request as a prompt. Claude Code may ask for permission before a tool call; approve it (the highlighted "Yes" option, `<ENTER>`).
- `<ESC>` closes dialogs; `<CTRL+C>` twice exits Claude Code.

## Closing a test session

1. `tui_take_screenshot` with `output_path: <repo>/mcp-tests/auto-test/<feature>-final.png` for the record.
2. `tui_end` with the plan's `session_id`. The reply says `Report '<…>/<feature>-test.report.jsonl': N checks, N passed, 0 failed`.
3. Check the report's last line is a `summary` with `"failed": 0`, and that `<feature>-test.cast` exists and replays.

## Passing

A plan passes when every assertion in its **Assertions** section holds. Record the outcome at the end of each run, in `<repo>/mcp-tests/auto-test/<feature>-result.md`: date, commit (`git rev-parse --short HEAD`), Claude Code version (`claude --version`), pass/fail per assertion, and notes for anything that didn't match.
