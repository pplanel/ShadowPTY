# 05 · Optional parameters without a `null` type

**Feature name:** `schema-null`
**Commit:** `899a897`

## What was implemented

schemars describes `Option<T>` parameters as `"type": ["integer", "null"]` (or `anyOf: [X, {"type": "null"}]` for enums). That's legal JSON Schema, but clients mapping schemas onto a single-`type` dialect (e.g. Gemini's OpenAPI subset) may reject such tools, and the MCP Inspector warns about each one. A schemars transform on every parameters struct now drops the `null`:

- `rows`, `cols`, `session_id`, `record_path`, … → `"type": "integer"` / `"string"` / …, not in `required`.
- `syntax`, `target` → a plain `$ref` to their enum.
- A client that still sends `null` is accepted (serde reads it as "not given").

## Session

Open Claude Code as in the [README](README.md#opening-a-test-session) with `<feature>` = `schema-null`.

## Steps

### A. No tool schema declares `null`

Run as one `!` line:

```sh
!{ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' '{"jsonrpc":"2.0","method":"notifications/initialized"}' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'; sleep 1; } | <repo>/target/release/shadowpty 2>/dev/null > <repo>/mcp-tests/auto-test/schema-null-tools.jsonl; python3 -c "
import json
tools = [json.loads(l) for l in open('<repo>/mcp-tests/auto-test/schema-null-tools.jsonl')][1]['result']['tools']
bad = []
def walk(v, at):
    if isinstance(v, dict):
        t = v.get('type')
        if (isinstance(t, list) and 'null' in t) or any(isinstance(b, dict) and b.get('type') == 'null' for b in v.get('anyOf', [])):
            bad.append(at)
        for k, x in v.items(): walk(x, at + '.' + k)
    elif isinstance(v, list):
        for x in v: walk(x, at)
for t in tools: walk(t['inputSchema'], t['name'])
print(len(tools), 'tools; nullable:', bad or 'none')
print(json.dumps(next(t for t in tools if t['name'] == 'tui_start')['inputSchema']['properties']['rows']))
print(json.dumps(next(t for t in tools if t['name'] == 'tui_signal')['inputSchema']['properties']['target']))
"
```

### B. `null` is still accepted

Prompt Claude Code: `Call shadowpty-dev's tui_start with exactly these arguments: {"command": "echo", "args": ["null-ok"], "rows": null, "cols": null, "session_id": null, "live": false}. Then tui_wait_exit and tui_end. Report each tool result verbatim.` Approve the calls.

### C. The Inspector shows no warnings (manual, browser)

Outside ShadowPTY: `npx @modelcontextprotocol/inspector <repo>/target/release/shadowpty`, connect, open **Tools**, select `tui_start`, `tui_expect` and `tui_signal`. Optionally, run it inside a second ShadowPTY session (`session_id: "schema-null-inspector"`, its own `.cast`) to record the server-side output, and open the printed URL in a browser.

### D. Automated coverage

`!cd <repo> && cargo test --lib -- schema input_schemas_have_no_null_types` passes (`schema::tests::*` and `server::tests::input_schemas_have_no_null_types`).

## Assertions

1. A prints `14 tools; nullable: none`.
2. A prints `rows` as `{"description": "Number of terminal rows (default 24).", "format": "uint16", "maximum": 65535, "minimum": 0, "type": "integer"}` and `target` with a `$ref` to `#/$defs/SignalTargetParam` and no `anyOf`.
3. B: `tui_start` succeeds with the default 24×80 size and session id `default`; `tui_wait_exit` reports exit code 0 with `null-ok` in the output.
4. C: none of the "`type` is an array" warnings appear for any tool.
5. D passes.

## Close

As in the [README](README.md#closing-a-test-session).
