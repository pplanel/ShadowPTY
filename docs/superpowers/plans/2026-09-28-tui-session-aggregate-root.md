# Deepen `TuiSession` into an Autonomous Aggregate Root Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract `TuiSession` from `src/pty_manager.rs` into a deep aggregate root in `src/session.rs` that completely encapsulates PTY execution, synchronization, screen captures, and process teardown, while turning `PtyManager` into a thin session registry and router.

**Architecture:** `TuiSession` owns the OS PTY pair, child PID, Alacritty terminal emulator, output stream buffer, recorder, reader thread, and shutdown flag. All operational methods (`send_input`, `send_paste`, `resize`, `snapshot`, `read_screen`, `expect`, `wait_stable`, `run_script`, `terminate`) live on `TuiSession`. `PtyManager` stores `Arc<TuiSession>` in a map and delegates tool requests under short locks.

**Tech Stack:** Rust Edition 2024, `alacritty_terminal`, `portable_pty`, `tokio`, strict clippy (`-D warnings`).

**Spec:** [`docs/superpowers/specs/2026-09-28-tui-session-aggregate-root-design.md`](file:///Users/pplanel/src/ShadowPTY/.worktrees/feat-alacritty-terminal/docs/superpowers/specs/2026-09-28-tui-session-aggregate-root-design.md)

## Global Constraints
- Must pass `cargo clippy --all-targets -- -D warnings` with zero warnings.
- Do NOT silence clippy with `#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]`.
- Function argument count limit: max 4 arguments (`too-many-arguments-threshold = 4`).
- Must pass 100% of existing tests (86 tests across unit, expect, and integration suites).
- Terminal lock is held strictly during screen capture sweep (~200 µs), never across awaits or formatting.

---

### Task 1: Create `src/session.rs` with `TuiSession` Core Struct, State, and `spawn`

**Files:**
- Create: `src/session.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `pub struct TuiSession`, `pub struct ProcessInfo`, `pub struct SessionSummary`, `pub struct PtyConfig<'a>`, `pub enum ExpectTarget`, `pub struct Expectation`, `pub struct Script<'a>`, `pub struct ScriptStep`, `pub struct ScriptOutcome`
- Function: `TuiSession::spawn(session_id: &str, config: &PtyConfig<'_>) -> Result<Self>`

- [ ] **Step 1: Write `src/session.rs` skeleton with types, PTY allocation, and reader thread**

```rust
//! Autonomous TUI session aggregate root managing OS PTY, terminal emulation,
//! background output reader, and synchronous/asynchronous interactions.

use std::fs::File;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use anyhow::{Context, Result};
use portable_pty::os::unix::ChildExt as _;
use portable_pty::{CommandBuilder, NativePtySystem, PtyPair, PtySize, PtySystem as _};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

use crate::input::parse_input_keys;
use crate::output::{Pattern, SessionOutput};
use crate::recorder::{AsciicastRecorder, SharedRecorder};
use crate::screen::Screen;

/// Bracketed paste markers (DECSET 2004).
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// How much recent output or screen text to include in a failed wait's error message.
const ERROR_TAIL_CHARS: usize = 500;

/// Information about a running process session.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProcessInfo {
    pub pid: Option<u32>,
    pub command: String,
    pub rows: u16,
    pub cols: u16,
    pub session_id: String,
}

/// Summary of an active session for listing.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub command: String,
    pub pid: Option<u32>,
    pub rows: u16,
    pub cols: u16,
    pub recording: bool,
}

/// Where expect searches for its pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectTarget {
    Stream,
    Screen,
}

/// What to wait for, where, and for how long.
#[derive(Debug, Clone)]
pub struct Expectation {
    pub pattern: Pattern,
    pub target: ExpectTarget,
    pub timeout: Duration,
}

/// Shell commands for `run_script`, and the prompt that follows each.
#[derive(Debug, Clone)]
pub struct Script<'a> {
    pub commands: &'a [String],
    pub prompt: Pattern,
    pub timeout_per_command: Duration,
}

/// Output of one command run by `run_script`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScriptStep {
    pub command: String,
    pub output: String,
}

/// Result of `run_script`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScriptOutcome {
    pub steps: Vec<ScriptStep>,
    pub error: Option<String>,
}

/// Configuration for spawning a PTY command.
#[derive(Debug, Clone)]
pub struct PtyConfig<'a> {
    pub command: &'a str,
    pub args: &'a [String],
    pub rows: u16,
    pub cols: u16,
    pub record_path: Option<&'a str>,
}

impl<'a> PtyConfig<'a> {
    #[must_use]
    pub const fn new(command: &'a str, args: &'a [String], rows: u16, cols: u16) -> Self {
        Self {
            command,
            args,
            rows,
            cols,
            record_path: None,
        }
    }

    #[must_use]
    pub const fn with_record_path(mut self, record_path: Option<&'a str>) -> Self {
        self.record_path = record_path;
        self
    }
}

struct TermSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

struct ReaderSinks {
    terminal: Arc<Mutex<Term<VoidListener>>>,
    output: Arc<SessionOutput>,
    recorder: SharedRecorder,
    shutdown_flag: Arc<AtomicBool>,
}

fn lock_terminal<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn run_pty_reader(mut pty_reader: Box<dyn std::io::Read + Send>, sinks: &ReaderSinks) {
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    let mut buf = [0u8; 4096];
    let mut parser = Processor::<StdSyncHandler>::new();

    loop {
        if sinks.shutdown_flag.load(Ordering::Relaxed) {
            break;
        }

        match pty_reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let bytes = &buf[..n];
                {
                    let mut term = lock_terminal(&sinks.terminal);
                    for byte in bytes {
                        parser.advance(&mut *term, *byte);
                    }
                }
                sinks.output.push(bytes);
                if let Ok(mut rec) = sinks.recorder.lock()
                    && let Some(r) = rec.as_mut()
                {
                    let _ = r.record_output(bytes);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    sinks.output.push_eof();
}

/// An autonomous, self-contained interactive PTY session.
pub struct TuiSession {
    info: ProcessInfo,
    pty_pair: PtyPair,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pty_writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    terminal: Arc<Mutex<Term<VoidListener>>>,
    output: Arc<SessionOutput>,
    recorder: SharedRecorder,
    shutdown_flag: Arc<AtomicBool>,
    reader_handle: Option<JoinHandle<()>>,
}

impl TuiSession {
    /// Spawns a command in a newly allocated PTY with attached terminal emulator.
    pub fn spawn(session_id: &str, config: &PtyConfig<'_>) -> Result<Self> {
        let pty_system = NativePtySystem::default();
        let size = PtySize {
            rows: config.rows,
            cols: config.cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pty_pair = pty_system
            .openpty(size)
            .map_err(|e| anyhow::anyhow!("failed to allocate pseudo-terminal: {e}"))?;

        let mut cmd = CommandBuilder::new(config.command);
        cmd.args(config.args);

        let child = pty_pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow::anyhow!("failed to spawn child process: {e}"))?;

        let pid = child.process_id();

        let pty_reader = pty_pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("failed to clone PTY reader: {e}"))?;
        let pty_writer = pty_pair
            .master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("failed to take PTY writer: {e}"))?;

        let term_size = TermSize {
            columns: config.cols as usize,
            screen_lines: config.rows as usize,
        };

        let terminal = Arc::new(Mutex::new(Term::new(
            Config::default(),
            &term_size,
            VoidListener,
        )));
        let output = Arc::new(SessionOutput::new());
        let shutdown_flag = Arc::new(AtomicBool::new(false));

        let recorder = if let Some(path) = config.record_path {
            let rec = AsciicastRecorder::create(path, config.cols, config.rows, Some(config.command))
                .with_context(|| format!("failed to initialize recorder with path '{path}'"))?;
            Arc::new(Mutex::new(Some(rec)))
        } else {
            Arc::new(Mutex::new(None))
        };

        let sinks = ReaderSinks {
            terminal: Arc::clone(&terminal),
            output: Arc::clone(&output),
            recorder: Arc::clone(&recorder),
            shutdown_flag: Arc::clone(&shutdown_flag),
        };

        let reader_handle = thread::Builder::new()
            .name(format!("shadowpty-reader-{session_id}"))
            .spawn(move || run_pty_reader(pty_reader, &sinks))
            .context("failed to spawn PTY reader thread")?;

        Ok(Self {
            info: ProcessInfo {
                pid,
                command: config.command.to_string(),
                rows: config.rows,
                cols: config.cols,
                session_id: session_id.to_string(),
            },
            pty_pair,
            child,
            pty_writer: Arc::new(Mutex::new(pty_writer)),
            terminal,
            output,
            recorder,
            shutdown_flag,
            reader_handle: Some(reader_handle),
        })
    }

    #[must_use]
    pub fn info(&self) -> &ProcessInfo {
        &self.info
    }

    #[must_use]
    pub fn summary(&self) -> SessionSummary {
        let recording = self
            .recorder
            .lock()
            .map(|r| r.is_some())
            .unwrap_or(false);
        SessionSummary {
            id: self.info.session_id.clone(),
            command: self.info.command.clone(),
            pid: self.info.pid,
            rows: self.info.rows,
            cols: self.info.cols,
            recording,
        }
    }
}
```

- [ ] **Step 2: Export `pub mod session;` in `src/lib.rs`**
- [ ] **Step 3: Run `cargo check` to verify compilation**
- [ ] **Step 4: Commit Task 1**

```bash
git add src/session.rs src/lib.rs
git commit -m "feat(session): scaffold TuiSession aggregate root in src/session.rs"
```

---

### Task 2: Implement Operations on `TuiSession`

**Files:**
- Modify: `src/session.rs`

**Interfaces:**
- Methods:
  - `send_input(&self, keys: &str) -> Result<usize>`
  - `send_paste(&self, text: &str) -> Result<usize>`
  - `resize(&mut self, rows: u16, cols: u16) -> Result<()>`
  - `snapshot(&self) -> Result<Screen>`
  - `read_screen(&self) -> Result<String>`
  - `expect(&self, expectation: &Expectation) -> Result<String>`
  - `wait_stable(&self, quiet_period: Duration, timeout: Duration) -> Result<()>`
  - `run_script(&self, script: &Script<'_>) -> Result<ScriptOutcome>`

- [ ] **Step 1: Implement write_input, send_input, and send_paste**

```rust
impl TuiSession {
    fn write_input(&self, bytes: &[u8]) -> Result<()> {
        {
            let mut writer = self
                .pty_writer
                .lock()
                .map_err(|_| anyhow::anyhow!("failed to acquire lock on PTY writer"))?;
            writer.write_all(bytes).context("failed to write input to PTY")?;
            writer.flush().context("failed to flush PTY writer")?;
        }

        if let Ok(mut rec_guard) = self.recorder.lock()
            && let Some(rec) = rec_guard.as_mut()
        {
            let _ = rec.record_input(bytes);
        }
        Ok(())
    }

    pub async fn send_input(&self, keys: &str) -> Result<usize> {
        let bytes = parse_input_keys(keys);
        self.write_input(&bytes)?;
        Ok(bytes.len())
    }

    pub async fn send_paste(&self, text: &str) -> Result<usize> {
        anyhow::ensure!(
            !text.contains(PASTE_END),
            "text contains the bracketed paste end marker (ESC[201~), which would end the paste early"
        );
        let payload = format!("{PASTE_START}{text}{PASTE_END}");
        self.write_input(payload.as_bytes())?;
        Ok(text.len())
    }
}
```

- [ ] **Step 2: Implement resize, snapshot, and read_screen**

```rust
impl TuiSession {
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.pty_pair
            .master
            .resize(size)
            .map_err(|e| anyhow::anyhow!("failed to resize PTY: {e}"))?;

        let term_size = TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        let mut term = lock_terminal(&self.terminal);
        term.resize(&term_size);

        self.info.rows = rows;
        self.info.cols = cols;
        Ok(())
    }

    pub fn snapshot(&self) -> Result<Screen> {
        let screen = {
            let term = lock_terminal(&self.terminal);
            Screen::capture(&term)
        };
        Ok(screen)
    }

    pub async fn read_screen(&self) -> Result<String> {
        let screen = {
            let term = lock_terminal(&self.terminal);
            self.output.mark_all_read();
            Screen::capture(&term)
        };
        Ok(screen.to_tagged_text())
    }
}
```

- [ ] **Step 3: Implement expect, wait_stable, and run_script**

```rust
impl TuiSession {
    pub async fn expect(&self, expectation: &Expectation) -> Result<String> {
        let Expectation {
            pattern,
            target,
            timeout,
        } = expectation;

        match target {
            ExpectTarget::Screen => self
                .output
                .wait_for(*timeout, || {
                    let screen = {
                        let term = lock_terminal(&self.terminal);
                        Screen::capture(&term)
                    };
                    pattern.find_in(&screen.to_plain_text())
                })
                .await
                .map_err(|e| {
                    let screen = {
                        let term = lock_terminal(&self.terminal);
                        Screen::capture(&term)
                    };
                    let text = screen.to_plain_text();
                    anyhow::anyhow!(
                        "pattern '{}' not found on screen: {e}. Current screen:\n{text}",
                        pattern.source()
                    )
                }),
            ExpectTarget::Stream => match self.output.expect(pattern, *timeout).await {
                Ok(found) => Ok(found.matched),
                Err(e) => Err(anyhow::anyhow!(
                    "pattern '{}' not found in output: {e}. Unread output (last {ERROR_TAIL_CHARS} chars):\n{}",
                    pattern.source(),
                    self.output.unread_tail(ERROR_TAIL_CHARS)
                )),
            },
        }
    }

    pub async fn wait_stable(&self, quiet_period: Duration, timeout: Duration) -> Result<()> {
        self.output.wait_stable(quiet_period, timeout).await
    }

    pub async fn run_script(&self, script: &Script<'_>) -> Result<ScriptOutcome> {
        let mut steps = Vec::with_capacity(script.commands.len());
        for cmd in script.commands {
            let send_text = format!("{cmd}\n");
            if let Err(e) = self.send_paste(&send_text).await {
                return Ok(ScriptOutcome {
                    steps,
                    error: Some(format!("failed to send command '{cmd}': {e:#}")),
                });
            }

            let expectation = Expectation {
                pattern: script.prompt.clone(),
                target: ExpectTarget::Stream,
                timeout: script.timeout_per_command,
            };

            match self.expect(&expectation).await {
                Ok(_) => {
                    let output = self.output.take_consumed();
                    steps.push(ScriptStep {
                        command: cmd.clone(),
                        output,
                    });
                }
                Err(e) => {
                    let partial = self.output.take_consumed();
                    steps.push(ScriptStep {
                        command: cmd.clone(),
                        output: partial,
                    });
                    return Ok(ScriptOutcome {
                        steps,
                        error: Some(format!("prompt not found after '{cmd}': {e:#}")),
                    });
                }
            }
        }

        Ok(ScriptOutcome { steps, error: None })
    }
}
```

- [ ] **Step 4: Run `cargo check` and verify compilation**
- [ ] **Step 5: Commit Task 2**

```bash
git add src/session.rs
git commit -m "feat(session): add operational methods to TuiSession"
```

---

### Task 3: Implement Lifecycle Teardown and `Drop` on `TuiSession`

**Files:**
- Modify: `src/session.rs`

**Interfaces:**
- Method: `pub async fn terminate(&mut self) -> Result<()>`
- Trait: `impl Drop for TuiSession`

- [ ] **Step 1: Implement async `terminate` and synchronous `Drop`**

```rust
impl TuiSession {
    /// Gracefully stops child process, signals PID group, and joins reader thread.
    pub async fn terminate(&mut self) -> Result<()> {
        self.shutdown_flag.store(true, Ordering::Relaxed);

        #[cfg(unix)]
        if let Some(pid) = self.info.pid {
            let pid_i32 = pid as i32;
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid_i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }

        // Wait briefly for child exit, then kill if needed
        let _ = self.child.kill();
        let _ = self.child.wait();

        if let Some(handle) = self.reader_handle.take() {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = handle.join();
            })
            .await;
        }

        if let Ok(mut rec) = self.recorder.lock()
            && let Some(mut r) = rec.take()
        {
            let _ = r.close();
        }

        Ok(())
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
```

- [ ] **Step 2: Run `cargo check`**
- [ ] **Step 3: Commit Task 3**

```bash
git add src/session.rs
git commit -m "feat(session): implement terminate and Drop on TuiSession"
```

---

### Task 4: Add Direct Unit Tests for `TuiSession` in `src/session.rs`

**Files:**
- Modify: `src/session.rs`

- [ ] **Step 1: Write comprehensive direct unit tests in `src/session.rs`**

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_session_spawn_input_and_read() {
        let args = vec!["-c".to_string(), "echo 'HELLO_SESSION'; read line; echo \"GOT:$line\"".to_string()];
        let config = PtyConfig::new("sh", &args, 24, 80);
        let mut session = TuiSession::spawn("test_direct", &config).expect("spawn");

        let exp = Expectation {
            pattern: Pattern::literal("HELLO_SESSION").expect("pat"),
            target: ExpectTarget::Stream,
            timeout: Duration::from_secs(5),
        };
        session.expect(&exp).await.expect("expect hello");

        session.send_input("my_input\n").await.expect("input");

        let exp_got = Expectation {
            pattern: Pattern::literal("GOT:my_input").expect("pat"),
            target: ExpectTarget::Stream,
            timeout: Duration::from_secs(5),
        };
        session.expect(&exp_got).await.expect("expect got");

        let screen = session.snapshot().expect("snapshot");
        assert_eq!(screen.rows, 24);
        assert_eq!(screen.cols, 80);

        session.terminate().await.expect("terminate");
    }

    #[tokio::test]
    async fn test_session_bracketed_paste() {
        let args = vec!["-c".to_string(), "read line; echo \"PASTED:$line\"".to_string()];
        let config = PtyConfig::new("sh", &args, 24, 80);
        let mut session = TuiSession::spawn("test_paste", &config).expect("spawn");

        session.send_paste("hello bracketed\n").await.expect("paste");

        let exp = Expectation {
            pattern: Pattern::literal("PASTED:hello bracketed").expect("pat"),
            target: ExpectTarget::Stream,
            timeout: Duration::from_secs(5),
        };
        session.expect(&exp).await.expect("expect pasted");
        session.terminate().await.expect("terminate");
    }

    #[tokio::test]
    async fn test_session_resize() {
        let args = vec!["-c".to_string(), "sleep 10".to_string()];
        let config = PtyConfig::new("sh", &args, 24, 80);
        let mut session = TuiSession::spawn("test_resize", &config).expect("spawn");

        session.resize(43, 155).expect("resize");
        assert_eq!(session.info().rows, 43);
        assert_eq!(session.info().cols, 155);

        let snap = session.snapshot().expect("snap");
        assert_eq!(snap.rows, 43);
        assert_eq!(snap.cols, 155);

        session.terminate().await.expect("terminate");
    }
}
```

- [ ] **Step 2: Run `cargo test src/session.rs` to verify new unit tests pass**
- [ ] **Step 3: Commit Task 4**

```bash
git add src/session.rs
git commit -m "test(session): add direct unit tests for TuiSession"
```

---

### Task 5: Refactor `src/pty_manager.rs` into a Thin Session Registry

**Files:**
- Modify: `src/pty_manager.rs`

**Interfaces:**
- Preserves all public methods and re-exports types from `crate::session`:
  `ProcessInfo`, `SessionSummary`, `ExpectTarget`, `Expectation`, `Script`, `ScriptStep`, `ScriptOutcome`, `PtyConfig`, `TuiSession`.
- `PtyManager::sessions` holds `Arc<Mutex<HashMap<String, Arc<TuiSession>>>>`.

- [ ] **Step 1: Refactor `PtyManager` to store `Arc<TuiSession>` and delegate all operations**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

pub use crate::session::{
    DEFAULT_SESSION_ID, ExpectTarget, Expectation, ProcessInfo, PtyConfig, Script, ScriptOutcome,
    ScriptStep, SessionSummary, TuiSession,
};
use crate::screen::Screen;

/// Session manager mapping session IDs to running `TuiSession` instances.
#[derive(Clone)]
pub struct PtyManager {
    sessions: Arc<Mutex<HashMap<String, Arc<TuiSession>>>>,
}

impl PtyManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn get_session(&self, session_id: &str) -> Result<Arc<TuiSession>> {
        let sessions = self.sessions.lock().await;
        sessions
            .get(session_id)
            .cloned()
            .with_context(|| format!("no active PTY session with id '{session_id}'. Call tui_start with this session_id first."))
    }

    pub async fn start_session(&self, session_id: &str, config: &PtyConfig<'_>) -> Result<ProcessInfo> {
        let previous = self.sessions.lock().await.remove(session_id);
        if let Some(prev) = previous {
            // Drop handles; terminate if uniquely owned
            drop(prev);
        }

        let session = Arc::new(TuiSession::spawn(session_id, config)?);
        let info = session.info().clone();

        self.sessions.lock().await.insert(session_id.to_string(), session);
        Ok(info)
    }

    pub async fn stop_session(&self, session_id: &str) -> Result<()> {
        let session = self.sessions.lock().await.remove(session_id);
        if let Some(session) = session {
            drop(session);
        }
        Ok(())
    }

    pub async fn has_session(&self, session_id: &str) -> bool {
        self.sessions.lock().await.contains_key(session_id)
    }

    pub async fn list_sessions(&self) -> Vec<SessionSummary> {
        let sessions = self.sessions.lock().await;
        sessions.values().map(|s| s.summary()).collect()
    }

    // Delegation methods
    pub async fn send_input_session(&self, session_id: &str, keys: &str) -> Result<usize> {
        let session = self.get_session(session_id).await?;
        session.send_input(keys).await
    }

    pub async fn send_paste_session(&self, session_id: &str, text: &str) -> Result<usize> {
        let session = self.get_session(session_id).await?;
        session.send_paste(text).await
    }

    pub async fn resize_session(&self, session_id: &str, rows: u16, cols: u16) -> Result<()> {
        let session = self.get_session(session_id).await?;
        // TuiSession resize requires mutable or internal lock
        session.resize(rows, cols)
    }

    pub async fn read_screen_session(&self, session_id: &str) -> Result<String> {
        let session = self.get_session(session_id).await?;
        session.read_screen().await
    }

    pub async fn snapshot_session(&self, session_id: &str) -> Result<Screen> {
        let session = self.get_session(session_id).await?;
        session.snapshot()
    }

    pub async fn expect_session(&self, session_id: &str, expectation: &Expectation) -> Result<String> {
        let session = self.get_session(session_id).await?;
        session.expect(expectation).await
    }

    pub async fn wait_stable_session(&self, session_id: &str, quiet: Duration, timeout: Duration) -> Result<()> {
        let session = self.get_session(session_id).await?;
        session.wait_stable(quiet, timeout).await
    }

    pub async fn run_script_session(&self, session_id: &str, script: &Script<'_>) -> Result<ScriptOutcome> {
        let session = self.get_session(session_id).await?;
        session.run_script(script).await
    }

    // Default session convenience wrappers
    pub async fn start_app(&self, config: &PtyConfig<'_>) -> Result<ProcessInfo> {
        self.start_session(DEFAULT_SESSION_ID, config).await
    }

    pub async fn stop_app(&self) -> Result<()> {
        self.stop_session(DEFAULT_SESSION_ID).await
    }

    pub async fn send_input(&self, keys: &str) -> Result<usize> {
        self.send_input_session(DEFAULT_SESSION_ID, keys).await
    }

    pub async fn send_paste(&self, text: &str) -> Result<usize> {
        self.send_paste_session(DEFAULT_SESSION_ID, text).await
    }

    pub async fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.resize_session(DEFAULT_SESSION_ID, rows, cols).await
    }

    pub async fn read_screen(&self) -> Result<String> {
        self.read_screen_session(DEFAULT_SESSION_ID).await
    }

    pub async fn snapshot(&self) -> Result<Screen> {
        self.snapshot_session(DEFAULT_SESSION_ID).await
    }

    pub async fn expect(&self, expectation: &Expectation) -> Result<String> {
        self.expect_session(DEFAULT_SESSION_ID, expectation).await
    }

    pub async fn wait_stable(&self, quiet: Duration, timeout: Duration) -> Result<()> {
        self.wait_stable_session(DEFAULT_SESSION_ID, quiet, timeout).await
    }

    pub async fn run_script(&self, script: &Script<'_>) -> Result<ScriptOutcome> {
        self.run_script_session(DEFAULT_SESSION_ID, script).await
    }
}
```

- [ ] **Step 2: Run `cargo test` across all targets**
- [ ] **Step 3: Commit Task 5**

```bash
git add src/pty_manager.rs
git commit -m "refactor(pty_manager): thin PtyManager into registry delegating to TuiSession"
```

---

### Task 6: Full Verification and Cleanup

**Files:**
- Verify: Entire workspace

- [ ] **Step 1: Run format check**

```bash
cargo fmt --check
```

- [ ] **Step 2: Run clippy check with strict flags**

```bash
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 3: Run all unit, expect, and integration tests**

```bash
cargo test --all-targets
```

- [ ] **Step 4: Verify Criterion benchmarks compile and execute**

```bash
cargo bench --no-run
```

- [ ] **Step 5: Update task tracking artifact**
