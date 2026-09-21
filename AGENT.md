# AGENT.md - ShadowPTY MCP Server Development

**Role & Context**
You are an expert Rust systems programmer and architect. Your objective is to build "ShadowPTY", a custom Model Context Protocol (MCP) server written in Rust. 
This server will allow other LLM agents to test Text User Interface (TUI) applications headlessly by allocating a native pseudo-terminal (PTY) and parsing the ANSI output into a clean, text-based grid that preserves layout and color information.

## Tech Stack & Tooling
*   **Language:** Rust (Edition 2024)
*   **Core Crates:**
    *   `portable-pty`: For native pseudo-terminal allocation, process spawning, and dynamic resizing.
    *   `vt100`: For in-memory ANSI sequence parsing, screen state buffering, and cell attribute extraction.
    *   `tokio`: Async runtime (required for standard MCP implementations over stdio).
    *   `serde` / `serde_json`: For MCP tool payload parsing.
    *   *MCP SDK:* Use a standard Rust MCP crate (e.g., `mcp-rs` or a custom JSON-RPC over stdin/stdout implementation).
*   **Environment:** The project must be fully reproducible via a `flake.nix` (using `crane` or `naersk`) to integrate seamlessly into a Nix-darwin/NixOS cluster architecture.

## System Architecture

The server must implement a persistent state manager (`PtyManager`) wrapped in `Arc<Mutex<TuiState>>` (or `RwLock`).

### 1. The State Object (`TuiState`)
*   Needs to hold the `Box<dyn Write + Send>` (or equivalent master PTY handle) to inject keystrokes and send resize signals.
*   Needs to hold the `vt100::Parser` to maintain the 2D grid of the screen and its dynamic dimensions.
*   *Critical:* When a process is spawned via `portable-pty`, a background `std::thread` or `tokio::task` must continuously read the PTY's `Reader` stream, taking chunks of bytes and feeding them directly into `parser.process()`.

### 2. MCP Tools Implementation
The server must expose exactly four Tools via the Model Context Protocol:

*   **`tui_start`**: 
    *   *Input:* `command` (String), `args` (Vec<String>), `rows` (u16, optional, default 24), `cols` (u16, optional, default 80).
    *   *Behavior:* Tears down any existing PTY. Uses `NativePtySystem` to allocate a new terminal with the requested size. Spawns the command. Resets the `vt100` parser to match the dimensions. Spawns the background reader loop.
    *   *Returns:* A success message confirming the process PID and initial dimensions.
*   **`tui_input`**:
    *   *Input:* `keys` (String).
    *   *Behavior:* Parses special string tokens (e.g., `<ENTER>` -> `\r`, `<ESC>` -> `\x1B`, `<UP>` -> `\x1B[A`) and writes the raw bytes to the PTY Writer. Flushes the stream.
    *   *Returns:* Confirmation of keys sent.
*   **`tui_resize`**:
    *   *Input:* `rows` (u16), `cols` (u16).
    *   *Behavior:* Sends a resize signal to the PTY master (using `portable-pty`'s resize API) and updates the `vt100::Parser` dimensions. This is strictly used to test if the TUI application's layout adapts correctly to window changes.
    *   *Returns:* Confirmation of the new terminal dimensions.
*   **`tui_read`**:
    *   *Input:* None.
    *   *Behavior:* Locks the state, reads the `vt100::Screen`, and iterates over rows and cells. It must **preserve color and formatting metadata**. Instead of stripping attributes, output the grid using a semantic markup (e.g., `<fg:red>Error</fg>` or a structured JSON representation of the cells) so the testing agent can assert that specific UI elements are rendered with the correct foreground/background colors and styles. 

## Step-by-Step Implementation Plan

1.  **Initialize Project:** 
    *   Generate the `Cargo.toml`.
    *   Draft a robust `flake.nix` providing the development shell (`rustc`, `cargo`, `rustfmt`, `clippy`) and the build derivation.
2.  **Implement Core PTY Logic:**
    *   Create the `pty_manager.rs` module.
    *   Implement the `PtyManager::new()`, `start_app()`, `send_input()`, `resize()`, and `read_screen()` logic using `portable-pty` and `vt100`.
    *   Ensure the background thread handles process termination gracefully without panicking.
3.  **Wire the MCP Server:**
    *   In `main.rs`, set up the Tokio runtime.
    *   Initialize the MCP Server instance communicating over standard input/output (stdio). Note: all logging/tracing *must* go to `stderr`, otherwise it will corrupt the MCP JSON-RPC protocol on `stdout`.
    *   Register the four tools and bind them to the `PtyManager` methods.
4.  **Formatting & Color Extraction:**
    *   Carefully map `vt100::Color` and attributes (bold, underline) into a readable, concise string format in `tui_read` so the LLM context window isn't bloated, but UI styling is verifiable.
5.  **Error Handling & Safety:**
    *   Use `anyhow::Result` for all fallible operations.
    *   Ensure thread-safe locking mechanisms around the VT100 parser to prevent data races between the background reader and the `tui_read` / `tui_resize` tools.

## Rules of Engagement
*   Write idiomatic, modular Rust code. Follow strict Clippy guidelines (Edition 2024).
*   Do not hallucinate external crate features. Stick strictly to the official documentation capabilities of `portable-pty` and `vt100`.
*   Prioritize terminal stability. If the child TUI app crashes, the MCP server must survive and report the error cleanly on the next tool call.
