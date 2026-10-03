# @azimovlabs/mcp-shadow-pty

[![npm version](https://img.shields.io/npm/v/@azimovlabs/mcp-shadow-pty.svg)](https://www.npmjs.com/package/@azimovlabs/mcp-shadow-pty)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/pplanel/ShadowPTY/blob/main/LICENSE-MIT)
[![MCP server](https://img.shields.io/badge/MCP-server-green.svg)](https://modelcontextprotocol.io)

**ShadowPTY** lets your AI assistant use terminal apps the way a person does, and prove it works. It's an [MCP](https://modelcontextprotocol.io) server, written in Rust, that gives Claude, Gemini, Cursor and other assistants a real terminal: open an app, type, press keys, read the screen with its colors, wait for things to appear, take screenshots, and record everything.

This package runs the precompiled native binary for macOS and Linux through `npx`. Nothing else to install.

[![An agent running fastfetch through ShadowPTY](https://raw.githubusercontent.com/pplanel/ShadowPTY/main/assets/demo.gif)](https://github.com/pplanel/ShadowPTY)

---

## Connect it to your assistant

**Claude Code**

```bash
claude mcp add shadow-pty -- npx -y @azimovlabs/mcp-shadow-pty
```

**Claude Desktop, Gemini CLI / Antigravity, Cursor and other MCP clients**: add this to the client's MCP config (`claude_desktop_config.json`, `~/.gemini/config/mcp_config.json`, Cursor's MCP settings, …):

```json
{
  "mcpServers": {
    "shadow-pty": {
      "command": "npx",
      "args": ["-y", "@azimovlabs/mcp-shadow-pty"]
    }
  }
}
```

Then ask for something, e.g. *"Open `htop` in ShadowPTY, sort by memory and show me a screenshot."*

---

## What it can do

| | |
| :--- | :--- |
| ⌨️ **Type and press keys** | Text, Enter, arrows, F-keys, Ctrl/Alt combos, pasted scripts |
| 👀 **Read the screen** | Text with colors and styles, exactly as laid out |
| ⏳ **Wait for things** | Until some text appears or disappears, the screen settles, or the app exits (with its exit code), with no fixed sleeps |
| 📸 **Screenshots** | PNG the assistant can see, or SVG for pixel-exact comparisons |
| 🎬 **Recordings** | Standard [asciinema](https://asciinema.org) files with real timing |
| ✅ **Test reports** | A JSON Lines log of every check (passed or failed, and how long it took) and a summary, e.g. `5 checks, 4 passed, 1 failed` |
| 🚦 **Send signals** | Interrupt, terminate, pause and resume the app (`INT`, `TERM`, `STOP`, `CONT`, …) without closing it |
| 📺 **Watch live** | A private local web page showing the screen and every input and check as the assistant works |
| 🙋 **Asks you first** | On the first session, a form asks whether you want a recording, a report or the live page (opened in your browser for you); your answer sticks for later sessions |
| 📐 **Resize** | Test how the app adapts to small and large windows |
| 🧩 **Several apps at once** | Each in its own named session |
| 🧹 **Clean shutdown** | Closing a session stops the app and anything it spawned |

Full-screen, colorful apps render like in a modern terminal: ShadowPTY uses [Alacritty](https://alacritty.org)'s terminal engine, and screen text and screenshots come from the same view, so they always agree.

Works on **macOS** (Apple Silicon) and **Linux** (x86_64, aarch64). Requires Node.js 18 or later to run through `npx`.

---

## Learn more

- [ShadowPTY on GitHub](https://github.com/pplanel/ShadowPTY): overview, use cases, TDD workflow, FAQ.
- [Technical reference](https://github.com/pplanel/ShadowPTY/blob/main/docs/reference.md): every tool and parameter, screen format, recording and report formats.
- [Issues](https://github.com/pplanel/ShadowPTY/issues)

Licensed under either of Apache License 2.0 or MIT, at your option.
