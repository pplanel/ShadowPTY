# RFC: The Live Viewer as an MCP App

> **RFC Status:** Proposed (spike on branch `feat/live-viewer-mcp-app`)
> **Date:** 2026-10-04
> **Builds on:** the live viewer (`src/live/`, [reference](../reference.md#live-viewer)), [RFC-screenshots-on-shared-term.md](./RFC-screenshots-on-shared-term.md) (SVG render)
> **Spec:** MCP Apps, `io.modelcontextprotocol/ui`, [2026-01-26 (Stable)](https://github.com/modelcontextprotocol/ext-apps/blob/main/specification/2026-01-26/apps.mdx)

---

## 1. Goal

When the person wants to watch a session, show it **inside the conversation**, next to the agent's messages, instead of handing them a `http://127.0.0.1:PORT/...?t=TOKEN` link to open in a browser tab. Same screen the agent reads (one emulator), view-only, live while the session runs.

Hosts that render MCP Apps today: Claude (web), Claude Desktop, VS Code Copilot, Goose, … ([client matrix](https://modelcontextprotocol.io/extensions/client-matrix)). Hosts that don't (Claude Code 2.1.288 declares `extensions: none`, see `src/client_log.rs`) keep the localhost viewer.

## 2. Constraints (from the spec and the hosts)

| Constraint | Source | Consequence |
| :--- | :--- | :--- |
| The UI is an HTML resource (`ui://…`, `text/html;profile=mcp-app`) the host renders in a sandboxed iframe for a **tool** whose `_meta.ui.resourceUri` points at it | spec §Resource Discovery | We need a tool to hang it on, and `resources/list` + `resources/read` |
| `_meta.ui` is part of the **tool definition** (static, from `tools/list`) | spec | Every call of that tool mounts a viewer, so the tool must be one you call *to watch* |
| Default CSP: `default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'none'` | spec §Host Behavior | Everything inline; images as `data:` URLs; no `fetch`/`EventSource` unless declared |
| No server→view push: the host doesn't forward server notifications; "Views request fresh data by calling tools via `tools/call`" | spec §Interactive Phase | Frames must be **pulled** by the page through `tools/call` |
| `_meta.ui.visibility: ["app"]` hides a tool from the model; the host MUST reject view calls to tools without `"app"`; default is `["model","app"]` | spec §Visibility | A poll tool hidden from the model; and *every* existing tool is callable by our page by default (§6) |
| The host MAY block view `tools/call` or ask the user first | spec §Sandbox proxy, rule 8 | Polling must tolerate errors; sending keys from the page may need approval |
| Tool results over ~150k chars get swapped for a file pointer in Claude | `build-mcp-app` skill, payload-budgeting | Frames must stay small |
| Claude Desktop caches UI resources aggressively | `build-mcp-app` skill | Version the URI or accept a ⌘Q to see page changes |

Measured on a busy 43×155 screen (`ls -la /usr/bin` plus colors), through the real server: **SVG 12,096 chars** (1,096 gzipped, 12,707 JSON-escaped) vs **PNG 573,868 base64 chars**. SVG is the only viable frame format for polling.

## 3. Proposal

### 3.1 Which tool carries the UI: a new `tui_watch`

| Option | Verdict |
| :--- | :--- |
| `tui_start` carries `resourceUri` | ✗ Static metadata: *every* start would mount a viewer, including headless runs the person never asked to watch. |
| `tui_take_screenshot` carries it | ✗ A screenshot is a still the model looks at; a live viewer per screenshot multiplies iframes. |
| **New `tui_watch { session_id }`** (read-only, model-visible) | ✓ One call = one viewer. Without UI support it returns the localhost link, so the agent has one tool for "let the person watch" on every client. |

Text result for UI hosts: `Showing session 'x' live in the conversation (view-only); it updates by itself.` For others: the localhost link, as `live: true` gives today. The capture form's "Watch live" answer (§3.6) and `tui_start live: true` keep working.

### 3.2 How frames reach the iframe: long-poll `tui_view_poll`

`tui_view_poll { session_id, since?, wait_ms? }`, `visibility: ["app"]`, returns one JSON object:

```json
{ "session_id": "default", "state": "running", "exit_status": null,
  "revision": 41, "rows": 43, "cols": 155, "svg": "<svg …>" }
```

- Without `since`: the current frame now. With `since = revision`: waits (≤ 10 s, `MAX_POLL_WAIT`) for the session's revision counter or exit status to change, then renders; on timeout answers with `svg: null` (unchanged).
- `state`: `running` | `exited` (process ended, session open) | `closed` (gone: `tui_end`, replaced, or the server restarted). The page stops polling on `closed`, so iframes left in old conversations go quiet instead of polling forever.
- The page caps itself at 15 frames/s (`MAX_FPS`), so a busy app costs at most 15 round trips/s through the host, ~0.7 ms of server CPU each (capture + SVG, per `cargo bench -- render`).

**Why not the localhost SSE stream from the iframe?** `connect-src 'none'` by default; declaring `connectDomains: ["http://127.0.0.1:PORT"]` doesn't work in practice: the port and token change every run while the resource (and its CSP metadata) is static and cached by Desktop; the token would have to be embedded in the page; and on Claude web the iframe is an HTTPS page on a public origin reaching loopback over plain HTTP, which browsers restrict (mixed content / Private Network Access). Polling through the host needs no network access at all. *(The browser-policy point is an assumption, not tested; the first two are enough.)*

**Why not Tasks or progress notifications?** The host doesn't route them to the view (§2), and Claude Code doesn't declare Tasks.

### 3.3 The page

`assets/live/app.html`, one self-contained file (~8 KB) served via `include_str!`, like `assets/live/index.html`:

- Speaks the view side of the protocol directly over `postMessage` (`ui/initialize` with `{protocolVersion: "2026-01-26", appInfo, appCapabilities}` → `ui/notifications/initialized`; handles `tool-input`, `tool-result`, `host-context-changed`, `ui/resource-teardown`; sends `size-changed`). Shapes checked against `App.connect()` in [`ext-apps/src/app.ts`](https://github.com/modelcontextprotocol/ext-apps/blob/main/src/app.ts). The alternative, inlining `@modelcontextprotocol/ext-apps/app-with-deps` (~300 KB), would add an npm build step to a Rust project for ~100 lines of protocol.
- Reads `session_id` from `ui/notifications/tool-input`, then polls.
- Shows the frame as `<img src="data:image/svg+xml,…">`, like the localhost page: SVG in an image can't run script, and `img-src data:` is in the default CSP.
- Follows the host theme (`hostContext.theme`, `styles.variables`).
- Supersession: a newer viewer of the same session (a second `tui_watch`) tells older ones over `BroadcastChannel` to stop polling and dim.

### 3.4 Negotiation and fallback

- Server: declares `extensions["io.modelcontextprotocol/ui"] = {mimeTypes: ["text/html;profile=mcp-app"]}` and the `resources` capability. This needs a hand-written `get_info` (the `#[tool_handler]` macro only enables tools).
- Client support = the client declares the extension with our MIME type, read per request (`context.client_capabilities()`, which covers both `initialize` and 2026-07-28 per-request capabilities).
- `list_tools` hides app-only tools from clients without UI support: they ignore `_meta.ui`, so they'd show `tui_view_poll` to the model. The list now depends on the client, so it no longer sends the shared-cache hints the macro set.
- Hosts that ignore `_meta.ui` get `tui_watch`'s text, which for them is the localhost link. Nothing else changes for them.

### 3.5 Invariants (CLAUDE.md)

- **One emulator**: frames are `TuiSession::snapshot()` + `render_svg`, the same as `tui_take_screenshot` and the localhost viewer.
- **Snapshot under the lock, render outside**: done on a blocking thread, as in `src/live/frames.rs`.
- **No lock across a wait**: the poll clones the `watch` receivers, drops the session `Arc` (keeps a `Weak`), then waits. It never keeps a session alive after `tui_end`.
- **Only the reader reads the PTY**: polling only reads the revision counter and snapshots.
- Tool errors, not panics: an unknown session yields `state: closed`, not an error, so stale views end cleanly.

### 3.6 Interaction (later, opt-in)

View-only first. Letting the person type into the session from the viewer is the natural next step and the riskiest:

- Default visibility means the page can already call `tui_input`, `tui_signal`, `tui_end`, … through the host. Our page doesn't; a later version that does should use a dedicated app-only tool (e.g. `tui_view_input`) so the model-facing tools keep their meaning.
- The person and the agent would race for the same PTY. Needs an explicit "take control" switch in the page, `ui/update-model-context` so the agent knows the person is typing, and report entries marked as the person's input.
- Hosts may require approval per view `tools/call` (rule 8): typing could prompt on every key.

### 3.7 Packaging

- npm and MCPB: nothing new to ship; the page is compiled in. `mcpb/manifest.json` lists `tui_watch` (the manifest test skips app-only tools).
- Claude Desktop runs local stdio servers, so an MCPB-installed ShadowPTY can show the viewer in Desktop with no tunnel.
- Bump the resource URI (or add a version query) when the page changes, because Desktop caches it.

## 4. Spike (branch `feat/live-viewer-mcp-app`)

| Piece | Where |
| :--- | :--- |
| Extension/visibility helpers, resource, `poll` | `src/live/app.rs` |
| `tui_watch`, `tui_view_poll`, `get_info`, `list_tools`, `list_resources`, `read_resource` | `src/server.rs` |
| Page | `assets/live/app.html` |
| Tests (both lifecycles): UI host sees `_meta.ui.resourceUri`, the app-only tool and the resource; poll returns a frame, then waits and returns `svg: null`, then `closed` after `tui_end`; non-UI host gets the link and no `tui_view_poll` | `tests/app_view_test.rs`, unit tests in `src/live/app.rs` |

**Not verified:** the page in a real host. Everything server-side is tested over a real rmcp client. The page's protocol was written from the spec and `app.ts`, not run in Claude Desktop/claude.ai.

## 5. Open questions

1. **Hosts and long-polls.** Do Claude Desktop/claude.ai time out or rate-limit a view's `tools/call` held for up to 10 s? If so, shorten `wait_ms` (the page passes it) or poll on a timer.
2. **Approval prompts.** Does Claude ask the person before view-initiated `tools/call` to `tui_view_poll`? If yes, polling is unusable and the design falls back to the `tui_watch` result (one frame) plus a manual refresh.
3. **`BroadcastChannel` in the sandbox.** If unavailable (opaque origin), every older viewer keeps polling until its session closes. Cheap, but could be capped server-side per session.
4. **Timeline and checks.** The localhost page shows report events (inputs, checks, pass/fail). Extending the poll with `reports_since` needs numbered events; today `LiveEntry` keeps an unnumbered, capped deque. Should the app share `LiveEntry`/the frame producer so N viewers cost one render?
5. **`tui_start live: true` / the capture form on UI hosts.** Should `live: true` stop starting the HTTP server when the client renders apps, and tell the agent to call `tui_watch` instead? Probably yes, once Q1–Q2 are answered.
6. **Display modes.** Offer `fullscreen` (declared in `appCapabilities`) with a button for wide terminals.
7. **Server-side gate for `tui_view_poll`.** Should calls to it from clients without UI support be refused outright, or is hiding it from their tool list enough?

## 6. Plan

| Phase | Scope | Exit criterion |
| :--- | :--- | :--- |
| 0 (spike branch) | RFC + spike: `tui_watch`, `tui_view_poll`, resource, page, tests | CI green; review this RFC |
| 1 | Try it in Claude Desktop (MCPB) and claude.ai (custom connector through a tunnel); answer Q1–Q3; add a `/app-preview` route on the localhost server with a fake host for page development | Viewer updates live in Desktop without prompts |
| 2 | Timeline/verdict in the page (numbered report events, `reports_since`); share renders with `LiveEntry`; `live: true` and the capture form route to `tui_watch` on UI hosts; skill and server instructions mention `tui_watch` | Feature parity with the localhost page |
| 3 | Optional "take control" input from the page (§3.6), behind an explicit switch | Person input logged as theirs; agent notified |
