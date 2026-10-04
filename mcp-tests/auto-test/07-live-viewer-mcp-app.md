# 07 · Live viewer as an MCP App (RFC + spike)

**Feature name:** `mcp-app-viewer`
**Commits:** `e68994f` (RFC on `feat/mcp-best-practices`), spike on branch `feat/live-viewer-mcp-app`

## What was implemented

- `docs/proposals/RFC-live-viewer-mcp-app.md`: how to show a session inline in the conversation on hosts that render MCP Apps (`io.modelcontextprotocol/ui`: Claude Desktop, claude.ai), keeping the localhost viewer for the others.
- Spike, **only on `feat/live-viewer-mcp-app`** (not merged):
  - `tui_watch` ("Watch session", read-only): carries `_meta.ui.resourceUri = ui://shadowpty/live-viewer.html`. On a host with MCP Apps the reply says the session is shown inline; on others it returns the localhost link (`This client can't show the viewer inline. The person can watch session '<id>' at http://127.0.0.1:…`).
  - `tui_view_poll` ("Poll live view"): the page's frame poll, `visibility: ["app"]`, **hidden from clients without MCP Apps**.
  - The resource `ui://shadowpty/live-viewer.html` (`text/html;profile=mcp-app`), `assets/live/app.html`.

## Setup

The spike lives on its own branch, so build it separately and point a second MCP config at it:

```sh
git -C <repo> worktree add <repo>/../shadowpty-mcp-app feat/live-viewer-mcp-app
cd <repo>/../shadowpty-mcp-app && cargo build --release
```

Write `<repo>/mcp-tests/auto-test/shadowpty-app.mcp.json` like the README's config, but named `shadowpty-app` and running `<repo>/../shadowpty-mcp-app/target/release/shadowpty`.

## Session

Open Claude Code as in the [README](README.md#opening-a-test-session) with `<feature>` = `mcp-app-viewer`, using `shadowpty-app.mcp.json` instead of `shadowpty-dev.mcp.json`.

## Steps

### A. RFC present (on `feat/mcp-best-practices`)

`!git -C <repo> show feat/mcp-best-practices:docs/proposals/RFC-live-viewer-mcp-app.md | head -5` shows the title and `RFC Status: Proposed (spike on branch \`feat/live-viewer-mcp-app\`)`.

### B. Claude Code (no MCP Apps): the viewer tool is visible, the poll tool isn't

1. `/mcp` → `shadowpty-app` → **View tools**. `tui_take_screenshot` → `<dir>/mcp-app-viewer-tools.png`.
2. Assert the list has 15 tools: the 14 from plan 01 plus **Watch session** (`read-only`), and **no "Poll live view"**.

### C. Claude Code gets the localhost link

1. Prompt: `Use shadowpty-app: tui_start {"command": "sh", "args": ["-c", "for i in 1 2 3 4 5; do date; sleep 1; done; sleep 60"], "session_id": "w1", "live": false}, then tui_watch {"session_id": "w1"}, then tui_end {"session_id": "w1"}. Show each result verbatim.`
2. Assert `tui_watch` replies `This client can't show the viewer inline. The person can watch session 'w1' at http://127.0.0.1:<port>/…` (don't open the link; this plan never uses the live view).

### D. Automated coverage (on the spike branch)

`!cd <repo>/../shadowpty-mcp-app && cargo test --test app_view_test && cargo test --lib live::app`

Expect `test_host_with_ui_sees_viewer_tool_resource_and_frames` and `test_host_without_ui_gets_a_link_and_no_app_only_tool` to pass, plus the unit tests in `src/live/app.rs`.

### E. Inline viewer in a host with MCP Apps (manual, not drivable from a terminal)

Not verified yet; this is the spike's open question. In Claude Desktop with the spike build configured as an MCP server: ask Claude to start a session that prints the date every second and to call `tui_watch`. Record:
- whether the viewer renders inline, updates about once a second, and stops after `tui_end`;
- whether Claude Desktop asks for permission on each `tui_view_poll` call (RFC open question 2), times out the 10 s long-poll (question 1), or rate-limits it.

## Assertions

1. A: the RFC is on `feat/mcp-best-practices` and points to the spike branch.
2. B: Claude Code lists **Watch session** and hides **Poll live view** (15 tools).
3. C: `tui_watch` falls back to a localhost link in Claude Code.
4. D: the spike's tests pass.
5. E (when run): record the findings in `mcp-app-viewer-result.md`; they answer the RFC's open questions 1–3.

## Close

As in the [README](README.md#closing-a-test-session). Remove the worktree when done: `git -C <repo> worktree remove <repo>/../shadowpty-mcp-app`.
