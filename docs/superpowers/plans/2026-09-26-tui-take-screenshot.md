# TUI Take Screenshot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement a `tui_take_screenshot` MCP tool that returns an SVG representation of the active terminal session using `termsnap-lib`.

**Architecture:** We will add `termsnap_lib::Term` to the `TuiSession` struct in `PtyManager`. The background reader thread will feed bytes to both the existing `vt100::Parser` and the new `termsnap_lib::Term`. A new `take_screenshot` method in `PtyManager` will extract the screen state and render it as an SVG string, which is then returned by a new `tui_take_screenshot` MCP tool.

**Tech Stack:** Rust, `tokio`, `rmcp`, `termsnap-lib`

**Spec:** `docs/superpowers/specs/2026-09-26-screenshot-feature-design.md`

## Global Constraints
- Run `cargo clippy` and `cargo test` after each task to ensure nothing breaks.
- No placeholder implementations.

---

### Task 1: Add Dependency and Update TuiSession

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/pty_manager.rs`

**Interfaces:**
- Produces: `snap_term` field in `TuiSession` initialized in `start_app`.

- [ ] **Step 1: Add `termsnap-lib` dependency**
Modify `Cargo.toml` to add `termsnap-lib = "0.4.0"` to the `[dependencies]` block.

- [ ] **Step 2: Check compilation**
Run: `cargo check`
Expected: PASS

- [ ] **Step 3: Update `TuiSession` struct**
In `src/pty_manager.rs`, modify the `TuiSession` struct to include the new field. Add `snap_term: Arc<std::sync::Mutex<termsnap_lib::Term<termsnap_lib::VoidPtyWriter>>>` right after `pub parser: ...`.

- [ ] **Step 4: Update `start_app` initialization**
In `src/pty_manager.rs` inside `start_app`, initialize `snap_term` right after `parser`:
```rust
        let snap_term = Arc::new(std::sync::Mutex::new(
            termsnap_lib::Term::new(config.rows, config.cols, termsnap_lib::VoidPtyWriter)
        ));
```
Add it to the returned `TuiSession` instance. Also, pass a clone of it to `run_pty_reader`:
```rust
        let reader_snap = Arc::clone(&snap_term);
        let reader_handle = thread::Builder::new()
            .name("shadowpty-reader".to_string())
            .spawn(move || {
                run_pty_reader(reader, &reader_parser, &reader_snap, &reader_recorder, &reader_shutdown);
            })
            .context("failed to spawn PTY reader thread")?;
```
Update the `run_pty_reader` signature to accept `snap_term: &Arc<std::sync::Mutex<termsnap_lib::Term<termsnap_lib::VoidPtyWriter>>>` as the third parameter.

- [ ] **Step 5: Run tests to verify**
Run: `cargo test`
Expected: PASS (all existing tests should still pass)

- [ ] **Step 6: Commit**
```bash
git add Cargo.toml src/pty_manager.rs
git commit -m "feat: add termsnap-lib and TuiSession state"
```

### Task 2: Process Terminal Bytes and Extract SVG

**Files:**
- Modify: `src/pty_manager.rs`

**Interfaces:**
- Consumes: `snap_term` from `TuiSession`.
- Produces: `PtyManager::take_screenshot(&self) -> Result<String>` which returns an SVG.

- [ ] **Step 1: Feed bytes to `snap_term`**
In `src/pty_manager.rs` inside `run_pty_reader`, update the byte processing logic. Right after `locked_parser.process(chunk);`, add:
```rust
                if let Ok(mut locked_snap) = snap_term.lock() {
                    for &byte in chunk {
                        locked_snap.process(byte);
                    }
                }
```

- [ ] **Step 2: Update `resize` method**
In `src/pty_manager.rs` inside `PtyManager::resize`, add resizing logic for `snap_term` right after the `parser` resizing block:
```rust
        if let Ok(mut locked_snap) = session.snap_term.lock() {
            locked_snap.resize(rows, cols);
        }
```

- [ ] **Step 3: Implement `take_screenshot`**
In `src/pty_manager.rs` inside `PtyManager`, add the new method:
```rust
    /// Returns the current TUI screen rendered as an SVG string.
    pub async fn take_screenshot(&self) -> Result<String> {
        let session_lock = self.session.lock().await;
        let session = session_lock
            .as_ref()
            .context("no active PTY session; call tui_start first")?;

        let snap_lock = session
            .snap_term
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to acquire lock on termsnap terminal"))?;

        let screen = snap_lock.current_screen();
        let fonts = ["Menlo", "Consolas", "monospace"];
        let svg = screen.to_svg(&fonts, termsnap_lib::FontMetrics::default()).to_string();

        drop(snap_lock);
        drop(session_lock);

        Ok(svg)
    }
```

- [ ] **Step 4: Add test for screenshot generation**
In `src/pty_manager.rs` inside `mod tests`, add a new test:
```rust
    #[tokio::test]
    async fn test_pty_take_screenshot() {
        let mgr = PtyManager::new();
        let args = ["hello shadowpty screenshot".to_string()];
        let cfg = PtyConfig::new("echo", &args, 10, 40);
        mgr.start_app(&cfg).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let svg = mgr.take_screenshot().await.unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("hello shadowpty screenshot"));
    }
```

- [ ] **Step 5: Run test to verify**
Run: `cargo test test_pty_take_screenshot`
Expected: PASS

- [ ] **Step 6: Commit**
```bash
git add src/pty_manager.rs
git commit -m "feat: process terminal bytes and add take_screenshot to PtyManager"
```

### Task 3: Expose `tui_take_screenshot` MCP Tool

**Files:**
- Modify: `src/server.rs`

**Interfaces:**
- Consumes: `PtyManager::take_screenshot`
- Produces: `tui_take_screenshot` MCP tool endpoint

- [ ] **Step 1: Add MCP tool to `ShadowPtyServer`**
In `src/server.rs` inside the `#[tool_router(server_handler)]` block, add:
```rust
    /// Generates an SVG screenshot of the current terminal screen.
    #[tool(
        name = "tui_take_screenshot",
        description = "Generates an SVG screenshot of the current terminal screen state using termsnap."
    )]
    pub async fn tui_take_screenshot(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        match self.manager.take_screenshot().await {
            Ok(svg_string) => Ok(CallToolResult::success(vec![
                rmcp::model::ContentBlock::text(svg_string),
            ])),
            Err(e) => Ok(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(format!("Failed to take screenshot: {e:#}")),
            ])),
        }
    }
```

- [ ] **Step 2: Run all tests to ensure MCP tool builds properly**
Run: `cargo test`
Expected: PASS

- [ ] **Step 3: Commit**
```bash
git add src/server.rs
git commit -m "feat: expose tui_take_screenshot MCP tool"
```
