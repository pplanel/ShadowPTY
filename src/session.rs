//! Autonomous TUI session aggregate root managing OS PTY, terminal emulation,
//! background output reader, and synchronous/asynchronous interactions.

use std::fs::File;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use alacritty_terminal::event::{OnResize, VoidListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options, Pty, Shell};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use anyhow::{Context, Result};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

use crate::input::parse_input_keys;
use crate::output::{Pattern, SessionOutput};
use crate::recorder::{AsciicastRecorder, SharedRecorder};
use crate::screen::Screen;

/// Session id used when a tool call doesn't specify one.
pub const DEFAULT_SESSION_ID: &str = "default";

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
    /// Output not yet consumed by an earlier expect or screen read; a match consumes it.
    Stream,
    /// The rendered screen text, re-checked whenever output arrives.
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

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn read_rwlock<T>(rwlock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    rwlock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write_rwlock<T>(rwlock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    rwlock.write().unwrap_or_else(PoisonError::into_inner)
}

/// Everything the reader thread writes PTY output to.
struct ReaderSinks {
    terminal: Arc<Mutex<Term<VoidListener>>>,
    output: Arc<SessionOutput>,
    recorder: SharedRecorder,
    shutdown_flag: Arc<AtomicBool>,
}

fn run_pty_reader(mut reader: File, sinks: &ReaderSinks) {
    let ReaderSinks {
        terminal,
        output,
        recorder,
        shutdown_flag,
    } = sinks;
    let mut buffer = [0u8; 4096];
    let mut parser: Processor<StdSyncHandler> = Processor::new();

    while !shutdown_flag.load(Ordering::Relaxed) {
        if let Some(deadline) = parser.sync_timeout().sync_timeout()
            && !wait_readable(&reader, deadline)
        {
            parser.stop_sync(&mut *lock_mutex(terminal));
            output.notify_screen_changed();
            continue;
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                tracing::debug!("PTY reader reached EOF");
                break;
            }
            Ok(n) => {
                let chunk = &buffer[..n];
                let mut term = lock_mutex(terminal);
                parser.advance(&mut *term, chunk);
                output.push(chunk);
                drop(term);
                if let Ok(mut rec_guard) = recorder.lock()
                    && let Some(rec) = rec_guard.as_mut()
                {
                    let _ = rec.record_output(chunk);
                }
            }
            Err(e) => {
                tracing::debug!("PTY reader read error: {e}");
                break;
            }
        }
    }
    output.close();
}

/// Waits until `reader` has data or `deadline` passes. Returns `false` on timeout.
fn wait_readable(reader: &File, deadline: std::time::Instant) -> bool {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let Ok(timeout) = Timespec::try_from(remaining) else {
        return true;
    };
    let mut fds = [PollFd::new(reader, PollFlags::IN)];
    loop {
        match poll(&mut fds, Some(&timeout)) {
            Ok(ready) => return ready > 0,
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return true,
        }
    }
}

/// An autonomous, self-contained interactive PTY session.
pub struct TuiSession {
    info: RwLock<ProcessInfo>,
    pty: Mutex<Pty>,
    pty_writer: Arc<Mutex<File>>,
    terminal: Arc<Mutex<Term<VoidListener>>>,
    output: Arc<SessionOutput>,
    recorder: SharedRecorder,
    shutdown_flag: Arc<AtomicBool>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
}

impl TuiSession {
    /// Allocates a PTY, spawns the command in it and starts the background reader thread.
    pub fn spawn(session_id: &str, config: &PtyConfig<'_>) -> Result<Self> {
        let size = WindowSize {
            num_lines: config.rows,
            num_cols: config.cols,
            cell_width: 0,
            cell_height: 0,
        };

        let options = Options {
            shell: Some(Shell::new(config.command.to_string(), config.args.to_vec())),
            ..Options::default()
        };

        let pty = tty::new(&options, size, 0)
            .map_err(|e| anyhow::anyhow!("failed to allocate pseudo-terminal: {e}"))?;

        let pid = pty.child().id();

        let pty_reader = pty.file().try_clone().context("failed to clone pty file")?;
        let pty_writer = pty.file().try_clone().context("failed to clone pty file")?;

        if let Ok(flags) = fcntl_getfl(&pty_reader) {
            let _ = fcntl_setfl(&pty_reader, flags.difference(OFlags::NONBLOCK));
        }

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
            let rec =
                AsciicastRecorder::create(path, config.cols, config.rows, Some(config.command))
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
            info: RwLock::new(ProcessInfo {
                pid: Some(pid),
                command: config.command.to_string(),
                rows: config.rows,
                cols: config.cols,
                session_id: session_id.to_string(),
            }),
            terminal,
            output,
            pty_writer: Arc::new(Mutex::new(pty_writer)),
            pty: Mutex::new(pty),
            recorder,
            shutdown_flag,
            reader_handle: Mutex::new(Some(reader_handle)),
        })
    }

    /// Returns a copy of the current process info.
    #[must_use]
    pub fn info(&self) -> ProcessInfo {
        read_rwlock(&self.info).clone()
    }

    /// Generates a summary for listing sessions.
    #[must_use]
    pub fn summary(&self) -> SessionSummary {
        let info = read_rwlock(&self.info);
        SessionSummary {
            id: info.session_id.clone(),
            command: info.command.clone(),
            pid: info.pid,
            rows: info.rows,
            cols: info.cols,
            recording: self.recorder.lock().is_ok_and(|rec| rec.is_some()),
        }
    }

    /// Writes raw bytes to the PTY and records them.
    fn write_input(&self, bytes: &[u8]) -> Result<()> {
        {
            let mut writer = self
                .pty_writer
                .lock()
                .map_err(|_| anyhow::anyhow!("failed to acquire lock on PTY writer"))?;
            writer
                .write_all(bytes)
                .context("failed to write input to PTY")?;
            writer.flush().context("failed to flush PTY writer")?;
        }

        if let Ok(mut rec_guard) = self.recorder.lock()
            && let Some(rec) = rec_guard.as_mut()
        {
            let _ = rec.record_input(bytes);
        }

        Ok(())
    }

    /// Sends keystrokes with symbolic token parsing (e.g. `<ENTER>`, `<UP>`).
    pub fn send_input(&self, keys: &str) -> Result<usize> {
        let bytes = parse_input_keys(keys);
        self.write_input(&bytes)?;
        Ok(bytes.len())
    }

    /// Sends bracketed paste input (DECSET 2004).
    pub fn send_paste(&self, text: &str) -> Result<usize> {
        anyhow::ensure!(
            !text.contains(PASTE_END),
            "text contains the bracketed paste end marker (ESC[201~), which would end the paste early"
        );
        let payload = format!("{PASTE_START}{text}{PASTE_END}");
        self.write_input(payload.as_bytes())?;
        Ok(text.len())
    }

    /// Resizes both the OS pseudo-terminal and the emulated grid.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(u16, u16)> {
        let size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 0,
            cell_height: 0,
        };

        {
            let mut pty = lock_mutex(&self.pty);
            pty.on_resize(size);
        }

        let term_size = TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };

        {
            let mut terminal = lock_mutex(&self.terminal);
            terminal.resize(term_size);
        }

        {
            let mut info = write_rwlock(&self.info);
            info.rows = rows;
            info.cols = cols;
        }

        if let Ok(mut rec_guard) = self.recorder.lock()
            && let Some(rec) = rec_guard.as_mut()
        {
            let _ = rec.record_resize(cols, rows);
        }

        Ok((rows, cols))
    }

    /// Reads formatted tagged text and marks all current output as seen.
    pub fn read_screen(&self) -> Result<String> {
        let screen = {
            let term = lock_mutex(&self.terminal);
            self.output.mark_all_read();
            Screen::capture(&term)
        };
        Ok(screen.to_tagged_text())
    }

    /// Takes a detached screen snapshot (lock held ~200 µs).
    pub fn snapshot(&self) -> Result<Screen> {
        let screen = {
            let term = lock_mutex(&self.terminal);
            Screen::capture(&term)
        };
        Ok(screen)
    }

    /// Waits for pattern match either on rendered screen text or in raw output stream.
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
                        let term = lock_mutex(&self.terminal);
                        Screen::capture(&term)
                    };
                    pattern.find_in(&screen.to_plain_text())
                })
                .await
                .map_err(|e| {
                    let screen = {
                        let term = lock_mutex(&self.terminal);
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

    /// Waits until no output has arrived for `quiet_period`, or times out.
    pub async fn wait_stable(&self, quiet_period: Duration, timeout: Duration) -> Result<()> {
        self.output
            .wait_stable(quiet_period, timeout)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Sequentially executes commands and validates prompt appearances.
    pub async fn run_script(&self, script: &Script<'_>) -> Result<ScriptOutcome> {
        let Script {
            commands,
            prompt,
            timeout_per_command,
        } = script;
        let newline = Pattern::literal("\n")?;

        self.output.mark_all_read();

        let mut steps = Vec::with_capacity(commands.len());
        for (index, command) in commands.iter().enumerate() {
            let step_number = index + 1;
            let stop = |reason: String| {
                Ok(ScriptOutcome {
                    steps: steps.clone(),
                    error: Some(format!("command {step_number} ('{command}'): {reason}")),
                })
            };
            let deadline = tokio::time::Instant::now() + *timeout_per_command;

            if let Err(e) = self.write_input(format!("{command}\r").as_bytes()) {
                return stop(format!("{e:#}"));
            }

            let mut text = String::new();
            match self
                .output
                .expect(
                    &newline,
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                )
                .await
            {
                Ok(echo) if echo.before.contains(command.trim()) => {}
                Ok(first_line) => {
                    text.push_str(&first_line.before);
                    text.push('\n');
                }
                Err(e) => {
                    return stop(format!(
                        "no output: {e}. Unread output:\n{}",
                        self.output.unread_tail(ERROR_TAIL_CHARS)
                    ));
                }
            }

            match self
                .output
                .expect(
                    prompt,
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                )
                .await
            {
                Ok(found) => {
                    text.push_str(&found.before);
                    steps.push(ScriptStep {
                        command: command.clone(),
                        output: text.trim().to_string(),
                    });
                }
                Err(e) => {
                    return stop(format!(
                        "prompt '{}' not seen: {e}. Unread output (last {ERROR_TAIL_CHARS} chars):\n{}",
                        prompt.source(),
                        self.output.unread_tail(ERROR_TAIL_CHARS)
                    ));
                }
            }
        }

        Ok(ScriptOutcome { steps, error: None })
    }

    /// Orderly termination: signals child process, reaps zombie, joins reader thread.
    pub async fn terminate(&self) -> Result<()> {
        #[cfg(unix)]
        if let Some(pid) = self.info().pid {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &format!("-{pid}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }

        self.shutdown_flag.store(true, Ordering::SeqCst);

        // Escalation to SIGKILL on Unix if not terminated
        #[cfg(unix)]
        if let Some(pid) = self.info().pid {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &format!("-{pid}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }

        let handle = self.reader_handle.lock().ok().and_then(|mut h| h.take());
        if let Some(handle) = handle {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = handle.join();
            })
            .await;
        }

        if let Ok(mut rec_guard) = self.recorder.lock()
            && let Some(rec) = rec_guard.as_mut()
        {
            let _ = rec.record_exit(0);
        }

        Ok(())
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Ok(info) = self.info.read()
            && let Some(pid) = info.pid
        {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &format!("-{pid}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        self.shutdown_flag.store(true, Ordering::SeqCst);
        if let Ok(mut rec_guard) = self.recorder.lock()
            && let Some(rec) = rec_guard.as_mut()
        {
            let _ = rec.record_exit(0);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_session_spawn_input_and_snapshot() {
        let args = vec![
            "-c".to_string(),
            "echo 'DIRECT_SESSION_OK'; sleep 2".to_string(),
        ];
        let config = PtyConfig::new("sh", &args, 24, 80);
        let session = TuiSession::spawn("test_direct", &config).expect("spawn");

        let exp = Expectation {
            pattern: Pattern::literal("DIRECT_SESSION_OK").expect("pat"),
            target: ExpectTarget::Stream,
            timeout: Duration::from_secs(5),
        };
        let matched = session.expect(&exp).await.expect("expect");
        assert_eq!(matched, "DIRECT_SESSION_OK");

        let snap = session.snapshot().expect("snapshot");
        assert_eq!(snap.rows, 24);
        assert_eq!(snap.cols, 80);

        let info = session.info();
        assert_eq!(info.session_id, "test_direct");
        assert!(info.pid.is_some());

        session.terminate().await.expect("terminate");
    }

    #[tokio::test]
    async fn test_session_bracketed_paste() {
        let args = vec!["-i".to_string()];
        let config = PtyConfig::new("sh", &args, 24, 80);
        let session = TuiSession::spawn("test_paste", &config).expect("spawn");

        session.send_paste("echo 'PASTED_OK'\n").expect("paste");

        let exp = Expectation {
            pattern: Pattern::literal("PASTED_OK").expect("pat"),
            target: ExpectTarget::Screen,
            timeout: Duration::from_secs(5),
        };
        session.expect(&exp).await.expect("expect pasted");

        let err = session
            .send_paste("evil\x1b[201~hack")
            .expect_err("marker rejection");
        assert!(format!("{err:#}").contains("bracketed paste end marker"));

        session.terminate().await.expect("terminate");
    }

    #[tokio::test]
    async fn test_session_resize() {
        let args = vec!["-c".to_string(), "sleep 5".to_string()];
        let config = PtyConfig::new("sh", &args, 24, 80);
        let session = TuiSession::spawn("test_resize", &config).expect("spawn");

        let (rows, cols) = session.resize(43, 155).expect("resize");
        assert_eq!(rows, 43);
        assert_eq!(cols, 155);

        let snap = session.snapshot().expect("snapshot");
        assert_eq!(snap.rows, 43);
        assert_eq!(snap.cols, 155);

        session.terminate().await.expect("terminate");
    }
}
