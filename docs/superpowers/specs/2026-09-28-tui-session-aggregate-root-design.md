# Architectural Design: Deepen `TuiSession` into an Autonomous Aggregate Root

- **Date:** 2026-09-28
- **Topic:** Architecture Deepening — Candidate 2
- **Target Component:** `src/session.rs`, `src/pty_manager.rs`
- **Status:** Approved for Implementation

---

## 1. Context & Motivation

In the current implementation of `ShadowPTY`, `PtyManager` ([`src/pty_manager.rs`](file:///Users/pplanel/src/ShadowPTY/.worktrees/feat-alacritty-terminal/src/pty_manager.rs)) has evolved into an 850-line "God Object". It simultaneously manages a dictionary of active sessions and orchestrates all low-level terminal execution details:
- Mutex acquisition and buffer flushing on raw PTY file descriptors.
- Terminal emulation resizing and locking.
- Screen extraction and lock-and-drop mechanics.
- Pattern search loops for stream and screen expect targets.
- Multi-step shell command scripting.
- Child process signaling, zombie reaping (`waitpid`), and background reader thread joining.

Meanwhile, `TuiSession` is an **anemic data struct** holding bare synchronization primitives (`Arc<Mutex<Term>>`, `Arc<SessionOutput>`, `Arc<Mutex<File>>`, `SharedRecorder`, `AtomicBool`) with no behaviors of its own.

This design introduces architectural friction:
1. **Lack of Encapsulation:** `PtyManager` frequently reaches into `TuiSession`'s fields via `with_session()` to clone internal handles and coordinate multi-lock sequences.
2. **Lock Contention Risk:** Slow operations (e.g. 5-second `expect` timeouts or heavy screen rendering) hold or coordinate around the session registry.
3. **Limited Testability:** A running terminal session cannot be tested in isolation without instantiating a full `PtyManager`.

---

## 2. Goals & Non-Goals

### Goals
- Extract `TuiSession` into an autonomous, deep aggregate root in `src/session.rs`.
- Encapsulate all internal primitives (`pty`, `terminal`, `output`, `pty_writer`, `recorder`, `shutdown_flag`, `reader_handle`) as private fields.
- Equip `TuiSession` with complete operational behaviors: `send_input`, `send_paste`, `resize`, `read_screen`, `snapshot`, `expect`, `wait_stable`, `run_script`, and `terminate`.
- Refactor `PtyManager` in `src/pty_manager.rs` into a thin session registry and router mapping `session_id -> Arc<TuiSession>`.
- Maintain 100% backwards compatibility for all public methods on `PtyManager` and all MCP tool calls in `src/server.rs`.
- Maintain 100% test pass rate across existing unit, expect, and integration test suites with zero warnings under strict clippy settings (`-D warnings`).

### Non-Goals
- Altering the MCP tool signatures or external wire protocol.
- Changing the underlying terminal emulator (`alacritty_terminal`) or PTY library (`portable_pty`).
- Modifying Candidate 3 scope (unifying the expect engine state machine will follow separately).

---

## 3. Architecture & Component Responsibilities

```
                 MCP Server / CLI / Tests
                            │
                            ▼
                    ┌───────────────┐
                    │  PtyManager   │  (src/pty_manager.rs)
                    │  (Registry)   │  - HashMap<String, Arc<TuiSession>>
                    └───────┬───────┘  - Lookup, start, stop, list
                            │
               ┌────────────┴────────────┐
               ▼                         ▼
      ┌─────────────────┐       ┌─────────────────┐
      │   TuiSession    │       │   TuiSession    │  (src/session.rs)
      │  ("session_a")  │       │  ("session_b")  │  - PTY process & reader thread
      └────────┬────────┘       └────────┬────────┘  - Terminal emulation (Alacritty)
               │                         │           - Input writing & bracketed paste
               ▼                         ▼           - Output stream buffer & expect
          OS PTY Kernel             OS PTY Kernel    - Screen capture & script workflow
```

### 3.1 `src/session.rs` (`TuiSession`)
- **State Ownership:**
  - `info: ProcessInfo`: Process metadata (PID, rows, cols, command, session ID).
  - `pty: Pty`: OS pseudo-terminal pair and child process handle.
  - `pty_writer: Arc<Mutex<File>>`: Writer file descriptor to the master PTY.
  - `terminal: Arc<Mutex<Term<VoidListener>>>`: Emulated terminal state.
  - `output: Arc<SessionOutput>`: Output ring buffer with pattern matching and sync markers.
  - `recorder: SharedRecorder`: Optional asciicast recording sink.
  - `shutdown_flag: Arc<AtomicBool>`: Cooperative cancellation signal for the reader thread.
  - `reader_handle: Option<thread::JoinHandle<()>>`: OS thread handle for the reader loop.
- **Key Invariants:**
  - Internal mutexes are never exposed outside the module.
  - Terminal lock is acquired strictly for minimal sweep duration via `Screen::capture` (~200 µs) and released immediately.
  - All operations take `&self` (allowing concurrent operations across threads without exclusive mutable references), except `terminate(&mut self)` (or `terminate(self)`).

### 3.2 `src/pty_manager.rs` (`PtyManager`)
- **State Ownership:**
  - `sessions: Arc<Mutex<HashMap<String, Arc<TuiSession>>>>`
- **Responsibilities:**
  - Spawning new sessions via `TuiSession::spawn`.
  - Managing session lifecycle: replacing existing sessions, stopping sessions, listing summaries.
  - Routing tool calls to the target `Arc<TuiSession>`.

---

## 4. API Specification

### 4.1 `TuiSession` Methods (`src/session.rs`)

```rust
impl TuiSession {
    /// Spawns a new PTY session, allocates terminal emulator, and starts background reader.
    pub fn spawn(session_id: &str, config: &PtyConfig<'_>) -> Result<Self>;

    /// Returns session metadata.
    pub fn info(&self) -> &ProcessInfo;

    /// Formats a session summary for listing.
    pub fn summary(&self) -> SessionSummary;

    /// Sends keystrokes with symbolic token parsing (<ENTER>, <UP>, etc.).
    pub async fn send_input(&self, keys: &str) -> Result<usize>;

    /// Sends bracketed paste input (wrapped in DECSET 2004 markers).
    pub async fn send_paste(&self, text: &str) -> Result<usize>;

    /// Resizes both the OS pseudo-terminal and the Alacritty terminal grid.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()>;

    /// Takes a detached immutable screen snapshot (lock held ~200 µs).
    pub fn snapshot(&self) -> Result<Screen>;

    /// Reads formatted tagged text and advances read marker on output buffer.
    pub async fn read_screen(&self) -> Result<String>;

    /// Waits for pattern match either on rendered screen text or in raw output stream.
    pub async fn expect(&self, expectation: &Expectation) -> Result<String>;

    /// Waits until no output has arrived for quiet_period, or times out.
    pub async fn wait_stable(&self, quiet_period: Duration, timeout: Duration) -> Result<()>;

    /// Sequentially executes commands and validates prompt appearances.
    pub async fn run_script(&self, script: &Script<'_>) -> Result<ScriptOutcome>;

    /// Gracefully stops child process, signals PID group, and joins reader thread.
    pub async fn terminate(&mut self) -> Result<()>;
}
```

### 4.2 `PtyManager` Routing (`src/pty_manager.rs`)

```rust
impl PtyManager {
    pub fn new() -> Self;

    pub async fn start_session(&self, id: &str, config: &PtyConfig<'_>) -> Result<ProcessInfo>;
    pub async fn stop_session(&self, id: &str) -> Result<()>;
    pub async fn has_session(&self, id: &str) -> bool;
    pub async fn list_sessions(&self) -> Vec<SessionSummary>;

    // Forwarding methods fetch Arc<TuiSession> under short map lock:
    pub async fn send_input_session(&self, id: &str, keys: &str) -> Result<usize>;
    pub async fn send_paste_session(&self, id: &str, text: &str) -> Result<usize>;
    pub async fn resize_session(&self, id: &str, rows: u16, cols: u16) -> Result<()>;
    pub async fn read_screen_session(&self, id: &str) -> Result<String>;
    pub async fn snapshot_session(&self, id: &str) -> Result<Screen>;
    pub async fn expect_session(&self, id: &str, expectation: &Expectation) -> Result<String>;
    pub async fn wait_stable_session(&self, id: &str, quiet: Duration, timeout: Duration) -> Result<()>;
    pub async fn run_script_session(&self, id: &str, script: &Script<'_>) -> Result<ScriptOutcome>;

    // DEFAULT_SESSION_ID convenience wrappers
    pub async fn start_app(&self, config: &PtyConfig<'_>) -> Result<ProcessInfo>;
    pub async fn stop_app(&self) -> Result<()>;
    pub async fn send_input(&self, keys: &str) -> Result<usize>;
    pub async fn send_paste(&self, text: &str) -> Result<usize>;
    pub async fn resize(&self, rows: u16, cols: u16) -> Result<()>;
    pub async fn read_screen(&self) -> Result<String>;
    pub async fn snapshot(&self) -> Result<Screen>;
    pub async fn expect(&self, expectation: &Expectation) -> Result<String>;
    pub async fn wait_stable(&self, quiet: Duration, timeout: Duration) -> Result<()>;
    pub async fn run_script(&self, script: &Script<'_>) -> Result<ScriptOutcome>;
}
```

---

## 5. Concurrency, Synchronization & Lifecycle

1. **Short Registry Locks:** Looking up a session in `PtyManager` acquires the `HashMap` lock only to clone `Arc<TuiSession>`, dropping the lock immediately. No I/O or pattern matching occurs while holding the map lock.
2. **Session Orderly Teardown:**
   - `terminate(&mut self)` sets `shutdown_flag` to true.
   - Child process is sent `SIGTERM` (Unix), followed by brief wait and `SIGKILL` escalation if necessary.
   - Child zombie is reaped via `waitpid`.
   - The reader thread join handle is joined asynchronously via `tokio::task::spawn_blocking`.
   - Recording file is flushed and closed.
3. **Safety Fallback in `Drop`:**
   - `impl Drop for TuiSession` provides synchronous, best-effort signal delivery and zombie reaping if dropped without an explicit `terminate()`.

---

## 6. Testing & Verification

1. **Direct `TuiSession` Unit Tests (`src/session.rs`):**
   - Spawn, input, and screen read verification without `PtyManager`.
   - Bracketed paste marker wrapping verification.
   - Terminal grid and PTY resize verification.
   - Screen-mode and stream-mode expect verification.
   - `wait_stable` verification.
   - `terminate` verification (reaping child without zombies).
2. **Existing Test Suite Compatibility:**
   - `tests/expect_test.rs` (12 tests) passing 100%.
   - `tests/integration_test.rs` (13 tests) passing 100%.
   - Lib tests (61 tests) passing 100%.
3. **Benchmarks:**
   - Criterion benchmarks compile and execute successfully.
4. **Code Quality:**
   - `cargo fmt --check`
   - `cargo clippy --all-targets -- -D warnings`
