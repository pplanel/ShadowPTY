# ShadowPTY (`termcp`)

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![Rust Edition: 2024](https://img.shields.io/badge/Rust-2024%20(1.88%2B)-orange.svg)](https://www.rust-lang.org)

**ShadowPTY** is a high-performance, headless Model Context Protocol (MCP) server written in Rust that enables LLM agents and automated test suites to interactively drive, inspect, and test Text User Interface (TUI) applications.

It allocates a real pseudo-terminal (PTY) on macOS and Linux, maintains an in-memory 2D virtual screen grid using `vt100`, and parses ANSI escape codes into structured, semantic markup with full color and style fidelity.

---

## Features

- **Native PTY Allocation**: Leverages `portable-pty` for real process execution, job control, signals, and dynamic resizing.
- **Headless Screen Buffer**: Maintains an accurate 2D virtual terminal state using `vt100`.
- **Semantic Style Tagging**: Screen reads are formatted with XML-like tags (e.g. `<fg:red><bold>Error</bold></fg>`), collapsing adjacent matching spans to avoid token bloat.
- **Key Token Translation**: Accepts intuitive key tokens like `<ENTER>`, `<ESC>`, `<UP>`, `<DOWN>`, `<TAB>`, `<BACKSPACE>`, `<CTRL+C>`, `<ALT+X>`, `<F1>`-`<F12>`, as well as raw text.
- **MCP Native**: Implements the standard Model Context Protocol over `stdio` using `rmcp` 3.4+.
- **Protocol Safety**: Logging and diagnostics go strictly to `stderr`, keeping JSON-RPC communication on `stdout` clean and uncorrupted.
- **Reproducible Environment**: Includes a Nix Flake (`flake.nix`) with `crane` and `rust-overlay`.

---

## MCP Tools

ShadowPTY exposes 4 MCP tools:

### 1. `tui_start`
Spawns a command inside a new pseudo-terminal session, tearing down any previous session.

- **Parameters**:
  - `command` (*string*, required): Executable to launch (e.g. `"htop"`, `"lazygit"`, `"bash"`).
  - `args` (*array of strings*, optional): Command-line arguments.
  - `rows` (*integer*, optional, default: 24): Initial terminal rows.
  - `cols` (*integer*, optional, default: 80): Initial terminal columns.
  - `record_path` (*string*, optional): File path where session will be recorded in [asciicast v3](https://docs.asciinema.org/manual/asciicast/v3/) format (`.cast`).
- **Example**:
  ```json
  {
    "command": "htop",
    "rows": 30,
    "cols": 100,
    "record_path": "/tmp/htop-session.cast"
  }
  ```

### 2. `tui_input`
Sends keystrokes and control sequences to the active application.

- **Parameters**:
  - `keys` (*string*, required): Text or key tokens.
- **Supported Special Tokens**:
  - `<ENTER>`, `<RETURN>`, `<ESC>`, `<ESCAPE>`, `<TAB>`, `<SPACE>`, `<BACKSPACE>`, `<DELETE>`
  - `<UP>`, `<DOWN>`, `<LEFT>`, `<RIGHT>`
  - `<HOME>`, `<END>`, `<PAGEUP>`, `<PAGEDOWN>`
  - `<F1>` through `<F12>`
  - `<CTRL+X>` or `<C-X>` (e.g. `<CTRL+C>`, `<CTRL+D>`)
  - `<ALT+X>` or `<M-X>`
- **Example**:
  ```json
  { "keys": "echo hello<ENTER>" }
  ```

### 3. `tui_resize`
Resizes the pseudo-terminal window and updates the screen grid to test responsive TUI behavior.

- **Parameters**:
  - `rows` (*integer*, required): New number of rows.
  - `cols` (*integer*, required): New number of columns.
- **Example**:
  ```json
  { "rows": 40, "cols": 120 }
  ```

### 4. `tui_read`
Captures the current state of the terminal screen rendered with semantic tags.

- **Parameters**: None.
- **Example Output**:
  ```
  <fg:green><bold>SUCCESS</bold></fg> Process completed in 0.42s
  <fg:bright-black>Press [q] to exit</fg>
  ```

---

## Installation & Setup

### Building from Source

Ensure you have Rust 1.88+ installed:

```bash
git clone https://github.com/pplanel/termcp.git
cd termcp
cargo build --release
```

The binary will be at `target/release/termcp`.

### Using Nix Flake

Run directly with Nix:

```bash
nix run github:pplanel/termcp
```

Or enter a reproducible development shell:

```bash
nix develop
```

---

## Integrating with MCP Clients

### Claude Desktop / Claude CLI

Add to your `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "shadowpty": {
      "command": "/path/to/termcp/target/release/termcp",
      "args": []
    }
  }
}
```

### Antigravity (`antigravity-cli`)

Add to `~/.gemini/config/mcp_config.json`:

```json
{
  "mcpServers": {
    "shadowpty": {
      "command": "/path/to/termcp/target/release/termcp",
      "args": []
    }
  }
}
```

---

## Testing & Quality

Run the test suite:

```bash
# Run unit and integration tests
cargo test

# Run strict Clippy checks
cargo clippy --all-targets

# Check formatting
cargo fmt --check
```

---

## Architecture

```mermaid
flowchart LR
    Client["MCP Client (LLM / Suite)"] <-->|JSON-RPC via stdio| Server["ShadowPTY (rmcp)"]
    Server <--> PtyMgr["PtyManager (Arc<Mutex<TuiSession>>)"]
    PtyMgr -->|Write keys / Resize| Master["PTY Master (portable-pty)"]
    Master <--> Slave["PTY Slave"] <--> Child["Child Process (TUI App)"]
    Master -->|Byte stream| Reader["Reader Thread"]
    Reader -->|parser.process()| Parser["VT100 Parser (vt100)"]
    Parser -->|Screen cells| Formatter["Semantic Markup Formatter"]
    Formatter -->|Formatted screen| Server
```

---

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
