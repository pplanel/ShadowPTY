# 01 · Tool titles, behavior hints and server instructions

**Feature name:** `tool-annotations`
**Commit:** `092303d`

## What was implemented

- Every one of the 14 tools has a human title (top-level `title` and `annotations.title`) and behavior hints: `readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`.
- The server sends usage instructions at initialization (start, act, wait instead of sleeping, read, end every session).
- The server introduces itself as `shadowpty` with the crate version, instead of `rmcp 3.4.0`.

Expected hints:

| Title | Tool | Hints |
| :--- | :--- | :--- |
| Start TUI session | `tui_start` | destructive, open-world |
| Send keys | `tui_input` | destructive, open-world |
| Paste text | `tui_paste` | destructive, open-world |
| Run shell commands | `tui_run_script` | destructive, open-world |
| Send signal | `tui_signal` | destructive |
| End session | `tui_end` | destructive, idempotent |
| Take screenshot | `tui_take_screenshot` | destructive, idempotent |
| Resize terminal | `tui_resize` | not read-only, not destructive, idempotent |
| Read screen | `tui_read` | read-only |
| Wait for text | `tui_expect` | read-only |
| Wait for text to disappear | `tui_wait_gone` | read-only |
| Wait for quiet screen | `tui_wait_stable` | read-only |
| Wait for exit | `tui_wait_exit` | read-only |
| List sessions | `tui_list_sessions` | read-only |

## Session

Open Claude Code as in the [README](README.md#opening-a-test-session) with `<feature>` = `tool-annotations`.

## Steps

### A. Titles and hints, as Claude Code shows them

1. Type `/mcp` and `<ENTER>`. `tui_expect` (`screen_mode: true`) for `Manage MCP servers`.
2. Assert the list shows `shadowpty-dev` with `14 tools`.
3. Move to `shadowpty-dev` (`<DOWN>` until it's highlighted, after any built-in servers) and `<ENTER>`. `tui_expect` for `Shadowpty-dev MCP Server`.
4. Assert the details show `Status: ✔ connected` and `Tools: 14 tools`.
5. `<ENTER>` on **View tools**. `tui_expect` for `Tools for shadowpty-dev`.
6. `tui_read` and compare every row with the table above. Claude Code prints the title, then the hints it shows: `read-only`, `destructive`, `open-world` (it doesn't show `idempotent`, and shows nothing for `Resize terminal`).
7. `tui_take_screenshot` to `<repo>/mcp-tests/auto-test/tool-annotations-tools.png`.
8. `<ESC>` until back at the prompt.

### B. Server name, version and instructions, from the raw protocol

Run this in the Claude Code session as a `!` command (one line):

```sh
!{ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' '{"jsonrpc":"2.0","method":"notifications/initialized"}' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'; sleep 1; } | <repo>/target/release/shadowpty 2>/dev/null > <repo>/mcp-tests/auto-test/tool-annotations-rpc.jsonl; python3 -c "import json; r=[json.loads(l) for l in open('<repo>/mcp-tests/auto-test/tool-annotations-rpc.jsonl')]; i=r[0]['result']; t=r[1]['result']['tools']; print(i['serverInfo'], 'instructions:', 'tui_start' in i.get('instructions',''), 'tools:', len(t), 'all titled:', all(x.get('title') and x['annotations'].get('title') for x in t), 'all hinted:', all('readOnlyHint' in x['annotations'] and 'openWorldHint' in x['annotations'] for x in t))"
```

`tui_expect` for `all hinted:` and read the line.

### C. Automated coverage

Run `!cd <repo> && cargo test --lib server::tests` and expect `every_tool_declares_title_and_hints`, `server_info_carries_instructions` and `mcpb_manifest_lists_every_tool` to pass.

## Assertions

1. `/mcp` lists `shadowpty-dev` as connected with 14 tools.
2. **View tools** shows the 14 titles from the table, each with exactly the hints listed (as `read-only` / `destructive` / `open-world`), and `Resize terminal` with none.
3. Step B prints `{'name': 'shadowpty', 'version': '<Cargo.toml version>'}`, `instructions: True`, `tools: 14`, `all titled: True`, `all hinted: True`.
4. Step C passes.

## Close

As in the [README](README.md#closing-a-test-session).
