# @azimovlabs/mcp-shadow-pty

[![npm version](https://img.shields.io/npm/v/@azimovlabs/mcp-shadow-pty.svg)](https://www.npmjs.com/package/@azimovlabs/mcp-shadow-pty)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](https://github.com/pplanel/ShadowPTY/blob/main/LICENSE-MIT)

**ShadowPTY** is a high-performance, headless Model Context Protocol (MCP) server written in Rust that enables LLM agents, automated test suites, and CI pipelines to interactively drive, inspect, and **record** Text User Interface (TUI) applications.

This npm package provides zero-configuration instant execution via `npx` by downloading and caching the precompiled native binary for macOS and Linux.

---

## Quick Start via `npx`

Run without installing:

```bash
npx -y @azimovlabs/mcp-shadow-pty
```

---

## MCP Client Configuration

### Claude Desktop / Claude CLI

Add to your `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "shadow-pty": {
      "command": "npx",
      "args": [
        "-y",
        "@azimovlabs/mcp-shadow-pty"
      ]
    }
  }
}
```

### Antigravity (`antigravity-cli`)

Add to `~/.gemini/config/mcp_config.json`:

```json
{
  "mcpServers": {
    "shadow-pty": {
      "command": "npx",
      "args": [
        "-y",
        "@azimovlabs/mcp-shadow-pty"
      ]
    }
  }
}
```

### Cursor

Add an MCP server in Cursor settings:
- **Name**: `shadow-pty`
- **Type**: `command`
- **Command**: `npx -y @azimovlabs/mcp-shadow-pty`

---

## Features

- 📹 **`asciicast v3` Session Recording**: Record full PTY sessions to `.cast` files with microsecond delta timestamps.
- ⚡ **Native PTY Allocation**: Real pseudo-terminal via `portable-pty`.
- 🖥️ **Headless VT100 Emulation**: Maintains in-memory 2D virtual screen buffer via `vt100`.
- 🏷️ **Semantic Style Tagging**: Screen reads formatted into token-efficient tags (`<fg:green><bold>...`).
- ⌨️ **Key Token Translation**: Send `<ENTER>`, `<ESC>`, `<UP>`, `<CTRL+C>`, etc.

---

## Source Repository

For source code, issue tracking, and documentation, visit:  
**https://github.com/pplanel/ShadowPTY**
