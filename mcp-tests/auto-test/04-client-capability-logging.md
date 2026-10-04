# 04 · Logging what the connected client supports

**Feature name:** `client-logging`
**Commit:** `59454e7`

## What was implemented

On the first request that identifies the client, the server writes **one** line to stderr (never stdout, which is the MCP channel):

```
Client connected: <name> <version>, protocol <version>, elicitation: <form and url | form | url only | no>, tasks (io.modelcontextprotocol/tasks): <yes|no>, extensions: <names | none>, experimental: <names | none>, sampling: <yes|no>, roots: <yes|no>
```

- Before protocol 2026-07-28 it's logged from `initialize`.
- On 2026-07-28 (no `initialize`) it's logged from `server/discover` or the first `tui_start`, whichever comes first.
- It's logged once per server process.

## Session

Empty the log first (`: > <repo>/mcp-tests/auto-test/server-stderr.log`), then open Claude Code as in the [README](README.md#opening-a-test-session) with `<feature>` = `client-logging`. Starting Claude Code starts `shadowpty-dev`, whose stderr goes to `server-stderr.log`.

## Steps

### A. Claude Code (protocol 2026-07-28)

1. Make the client talk to the server: type `/mcp`, `<ENTER>`, select `shadowpty-dev`, `<ENTER>`; assert `Protocol: 2026-07-28`. `<ESC>` back to the prompt.
2. To be sure the line exists even if the client skipped `server/discover`, prompt: `Use shadowpty-dev's tui_start to run "true" with live set to false, then call tui_end.` Approve the tool calls.
3. `!grep "Client connected" <repo>/mcp-tests/auto-test/server-stderr.log`
4. `!grep -c "Client connected" <repo>/mcp-tests/auto-test/server-stderr.log`

### B. A legacy client (`initialize`)

Run as one `!` line:

```sh
!{ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"elicitation":{}},"clientInfo":{"name":"legacy-check","version":"9.9"}}}' '{"jsonrpc":"2.0","method":"notifications/initialized"}'; sleep 1; } | RUST_LOG=info <repo>/target/release/shadowpty 2><repo>/mcp-tests/auto-test/client-logging-legacy.log >/dev/null; grep "Client connected" <repo>/mcp-tests/auto-test/client-logging-legacy.log
```

### C. stdout stays clean

Run the step B pipeline again with `2>/dev/null` and stdout to `client-logging-stdout.jsonl`; every line of that file must parse as JSON-RPC (`!python3 -c "import json; [json.loads(l) for l in open('<repo>/mcp-tests/auto-test/client-logging-stdout.jsonl')]; print('ok')"`).

### D. Automated coverage

`!cd <repo> && cargo test --lib client_log` passes.

## Assertions

1. A.3 prints one line starting `Client connected: claude-code <claude --version>, protocol 2026-07-28, elicitation: form and url, tasks (io.modelcontextprotocol/tasks): no` (with Claude Code 2.1.288; record what a newer version declares, especially `tasks`).
2. A.4 prints `1`: logged once even after several requests.
3. B prints `Client connected: legacy-check 9.9, protocol 2025-06-18, elicitation: form, …`.
4. C prints `ok`.
5. D passes.

## Close

As in the [README](README.md#closing-a-test-session). Keep `server-stderr.log` next to the recording as `client-logging-server-stderr.log` (copy it before the next plan empties it).
