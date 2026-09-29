//! PTY management and TUI screen state synchronization for ShadowPTY.

use std::collections::HashMap;
use std::fs::File;
use std::io::{PipeReader, PipeWriter, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tokio::time::Instant;

use alacritty_terminal::event::{OnResize, VoidListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options, Pty, Shell};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
use rustix_openpty::openpty;
use rustix_openpty::rustix::termios::Winsize;

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

use crate::formatter::{format_screen, screen_text};
use crate::input::parse_input_keys;
use crate::output::{Pattern, SessionOutput};
use crate::recorder::{AsciicastRecorder, SharedRecorder};

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

/// Where [`PtyManager::expect_session`] looks for its pattern.
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

/// Shell commands for [`PtyManager::run_script_session`], and the prompt that follows each.
#[derive(Debug, Clone)]
pub struct Script<'a> {
    pub commands: &'a [String],
    pub prompt: Pattern,
    pub timeout_per_command: Duration,
}

/// Output of one command run by [`PtyManager::run_script_session`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScriptStep {
    pub command: String,
    pub output: String,
}

/// Result of [`PtyManager::run_script_session`]: the commands that completed, and why the
/// script stopped early, if it did.
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

/// Active PTY session state.
pub struct TuiSession {
    pub info: ProcessInfo,
    pub terminal: Arc<std::sync::Mutex<Term<VoidListener>>>,
    output: Arc<SessionOutput>,
    pty_writer: Arc<std::sync::Mutex<File>>,
    pub pty: Pty,
    recorder: SharedRecorder,
    shutdown_flag: Arc<AtomicBool>,
    _reader_handle: Option<JoinHandle<()>>,
}

impl TuiSession {
    /// Allocates a PTY, spawns the command in it and starts the background reader thread.
    fn spawn(session_id: &str, config: &PtyConfig<'_>) -> Result<Self> {
        let options = Options {
            shell: Some(Shell::new(config.command.to_string(), config.args.to_vec())),
            ..Options::default()
        };

        let opened = openpty(
            None,
            Some(&Winsize {
                ws_row: config.rows,
                ws_col: config.cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
        )
        .context("failed to allocate pseudo-terminal")?;
        // macOS discards unread output shortly after the last slave fd closes, so the reader
        // keeps one open until it has drained everything the child wrote
        let slave_keepalive = opened
            .user
            .try_clone()
            .context("failed to clone pty slave")?;
        let pty = tty::from_fd(&options, 0, opened.controller, opened.user)
            .map_err(|e| anyhow::anyhow!("failed to spawn '{}': {e}", config.command))?;

        let pid = pty.child().id();
        let (child_exit, exit_notifier) = std::io::pipe().context("failed to create exit pipe")?;
        thread::Builder::new()
            .name(format!("shadowpty-exit-{session_id}"))
            .spawn(move || watch_child_exit(pid, exit_notifier))
            .context("failed to spawn child exit watcher")?;

        let pty_reader = pty.file().try_clone().context("failed to clone pty file")?;
        let pty_writer = pty.file().try_clone().context("failed to clone pty file")?;

        if let Ok(flags) = fcntl_getfl(&pty_reader) {
            let _ = fcntl_setfl(&pty_reader, flags.difference(OFlags::NONBLOCK));
        }

        let term_size = TermSize {
            columns: config.cols as usize,
            screen_lines: config.rows as usize,
        };

        let terminal = Arc::new(std::sync::Mutex::new(Term::new(
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
            Arc::new(std::sync::Mutex::new(Some(rec)))
        } else {
            Arc::new(std::sync::Mutex::new(None))
        };

        let sinks = ReaderSinks {
            terminal: Arc::clone(&terminal),
            output: Arc::clone(&output),
            recorder: Arc::clone(&recorder),
            shutdown_flag: Arc::clone(&shutdown_flag),
        };
        let reader_handle = thread::Builder::new()
            .name(format!("shadowpty-reader-{session_id}"))
            .spawn(move || {
                run_pty_reader(pty_reader, &child_exit, &sinks);
                drop(slave_keepalive);
            })
            .context("failed to spawn PTY reader thread")?;

        Ok(Self {
            info: ProcessInfo {
                pid: Some(pid),
                command: config.command.to_string(),
                rows: config.rows,
                cols: config.cols,
                session_id: session_id.to_string(),
            },
            terminal,
            output,
            pty_writer: Arc::new(std::sync::Mutex::new(pty_writer)),
            pty,
            recorder,
            shutdown_flag,
            _reader_handle: Some(reader_handle),
        })
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let pid = self.pty.child().id();
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

/// Kills and reaps a session without blocking the async runtime.
///
/// Dropping a `TuiSession` runs `kill` on the process group, and dropping its `Pty` waits for
/// the child to exit, so the drop runs on the blocking thread pool.
async fn teardown(session: TuiSession) {
    let _ = tokio::task::spawn_blocking(move || drop(session)).await;
}

fn no_session(session_id: &str) -> String {
    format!("no active PTY session with id '{session_id}'; call tui_start first")
}

fn lock_terminal(
    terminal: &std::sync::Mutex<Term<VoidListener>>,
) -> std::sync::MutexGuard<'_, Term<VoidListener>> {
    terminal.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Thread-safe manager for concurrent headless PTY sessions, keyed by session id.
#[derive(Clone, Default)]
pub struct PtyManager {
    sessions: Arc<Mutex<HashMap<String, TuiSession>>>,
}

impl PtyManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Runs `f` on the session under the map lock, which is released as soon as `f` returns.
    ///
    /// Use it to clone out the handles an operation needs, so slow work doesn't block other sessions.
    async fn with_session<T>(
        &self,
        session_id: &str,
        f: impl FnOnce(&TuiSession) -> T,
    ) -> Result<T> {
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(session_id)
            .with_context(|| no_session(session_id))?;
        let result = f(session);
        drop(sessions);
        Ok(result)
    }

    /// Spawns a command in a new PTY session, replacing any session with the same id.
    pub async fn start_session(
        &self,
        session_id: &str,
        config: &PtyConfig<'_>,
    ) -> Result<ProcessInfo> {
        // Tear down the old session outside the map lock so other sessions aren't blocked
        let previous = self.sessions.lock().await.remove(session_id);
        if let Some(previous) = previous {
            teardown(previous).await;
        }

        let session = TuiSession::spawn(session_id, config)?;
        let info = session.info.clone();

        // A concurrent start with the same id may have inserted in the meantime
        let replaced = self
            .sessions
            .lock()
            .await
            .insert(session_id.to_string(), session);
        if let Some(replaced) = replaced {
            teardown(replaced).await;
        }

        Ok(info)
    }

    /// Starts the default session.
    pub async fn start_app(&self, config: &PtyConfig<'_>) -> Result<ProcessInfo> {
        self.start_session(DEFAULT_SESSION_ID, config).await
    }

    /// Writes raw bytes to the session's PTY and records them as input.
    async fn write_input(&self, session_id: &str, bytes: &[u8]) -> Result<()> {
        let (writer, recorder) = self
            .with_session(session_id, |s| {
                (Arc::clone(&s.pty_writer), Arc::clone(&s.recorder))
            })
            .await?;

        {
            let mut writer = writer
                .lock()
                .map_err(|_| anyhow::anyhow!("failed to acquire lock on PTY writer"))?;
            writer
                .write_all(bytes)
                .context("failed to write input to PTY")?;
            writer.flush().context("failed to flush PTY writer")?;
        }

        if let Ok(mut rec_guard) = recorder.lock()
            && let Some(rec) = rec_guard.as_mut()
        {
            let _ = rec.record_input(bytes);
        }

        Ok(())
    }

    /// Sends keystrokes with symbolic tokens (e.g. `<ENTER>`, `<UP>`) to the target session.
    pub async fn send_input_session(&self, session_id: &str, keys: &str) -> Result<usize> {
        let bytes = parse_input_keys(keys);
        self.write_input(session_id, &bytes).await?;
        Ok(bytes.len())
    }

    /// Sends input to the default session.
    pub async fn send_input(&self, keys: &str) -> Result<usize> {
        self.send_input_session(DEFAULT_SESSION_ID, keys).await
    }

    /// Sends text wrapped in bracketed paste markers, so shells and editors treat it as one
    /// paste instead of typed keys. Returns the number of bytes of `text` sent.
    pub async fn send_paste_session(&self, session_id: &str, text: &str) -> Result<usize> {
        anyhow::ensure!(
            !text.contains(PASTE_END),
            "text contains the bracketed paste end marker (ESC[201~), which would end the paste early; send it with tui_input instead"
        );
        let payload = format!("{PASTE_START}{text}{PASTE_END}");
        self.write_input(session_id, payload.as_bytes()).await?;
        Ok(text.len())
    }

    /// Sends a bracketed paste to the default session.
    pub async fn send_paste(&self, text: &str) -> Result<usize> {
        self.send_paste_session(DEFAULT_SESSION_ID, text).await
    }

    /// Waits for the expectation's pattern and returns the matched text.
    ///
    /// See [`ExpectTarget`] for where the pattern is searched.
    pub async fn expect_session(
        &self,
        session_id: &str,
        expectation: &Expectation,
    ) -> Result<String> {
        let Expectation {
            pattern,
            target,
            timeout,
        } = expectation;
        let (output, terminal) = self
            .with_session(session_id, |s| {
                (Arc::clone(&s.output), Arc::clone(&s.terminal))
            })
            .await?;

        match target {
            ExpectTarget::Screen => output
                .wait_for(*timeout, || {
                    pattern.find_in(&screen_text(&lock_terminal(&terminal)))
                })
                .await
                .map_err(|e| {
                    let screen = screen_text(&lock_terminal(&terminal));
                    anyhow::anyhow!(
                        "pattern '{}' not found on screen: {e}. Current screen:\n{screen}",
                        pattern.source()
                    )
                }),
            ExpectTarget::Stream => match output.expect(pattern, *timeout).await {
                Ok(found) => Ok(found.matched),
                Err(e) => Err(anyhow::anyhow!(
                    "pattern '{}' not found in output: {e}. Unread output (last {ERROR_TAIL_CHARS} chars):\n{}",
                    pattern.source(),
                    output.unread_tail(ERROR_TAIL_CHARS)
                )),
            },
        }
    }

    /// Waits for an expectation in the default session.
    pub async fn expect(&self, expectation: &Expectation) -> Result<String> {
        self.expect_session(DEFAULT_SESSION_ID, expectation).await
    }

    /// Waits until the session produces no output for `quiet_period`.
    pub async fn wait_stable_session(
        &self,
        session_id: &str,
        quiet_period: Duration,
        max_wait: Duration,
    ) -> Result<()> {
        let output = self
            .with_session(session_id, |s| Arc::clone(&s.output))
            .await?;
        output
            .wait_stable(quiet_period, max_wait)
            .await
            .map_err(|e| anyhow::anyhow!("output did not stay quiet for {quiet_period:?}: {e}"))
    }

    /// Waits until the default session produces no output for `quiet_period`.
    pub async fn wait_stable(&self, quiet_period: Duration, max_wait: Duration) -> Result<()> {
        self.wait_stable_session(DEFAULT_SESSION_ID, quiet_period, max_wait)
            .await
    }

    /// Runs shell commands one at a time, waiting for the prompt after each.
    ///
    /// Output that arrived before the call is ignored. The terminal's echo of each command is
    /// skipped before looking for the prompt, so a command containing the prompt text can't
    /// match its own echo. Stops at the first command whose prompt doesn't appear within
    /// `timeout_per_command`.
    pub async fn run_script_session(
        &self,
        session_id: &str,
        script: &Script<'_>,
    ) -> Result<ScriptOutcome> {
        let Script {
            commands,
            prompt,
            timeout_per_command,
        } = script;
        let newline = Pattern::literal("\n")?;
        let output = self
            .with_session(session_id, |s| Arc::clone(&s.output))
            .await?;

        // Earlier output, such as a prompt already on screen, must not satisfy the first wait
        output.mark_all_read();

        let mut steps = Vec::with_capacity(commands.len());
        for (index, command) in commands.iter().enumerate() {
            let step_number = index + 1;
            let stop = |reason: String| {
                Ok(ScriptOutcome {
                    steps: steps.clone(),
                    error: Some(format!("command {step_number} ('{command}'): {reason}")),
                })
            };
            let deadline = Instant::now() + *timeout_per_command;

            if let Err(e) = self
                .write_input(session_id, format!("{command}\r").as_bytes())
                .await
            {
                return stop(format!("{e:#}"));
            }

            let mut text = String::new();
            match output
                .expect(&newline, deadline.saturating_duration_since(Instant::now()))
                .await
            {
                // The echoed line holds the command (possibly after an earlier prompt): skip it
                Ok(echo) if echo.before.contains(command.trim()) => {}
                // No echo (e.g. echo disabled): the line is the command's own output
                Ok(first_line) => {
                    text.push_str(&first_line.before);
                    text.push('\n');
                }
                Err(e) => {
                    return stop(format!(
                        "no output: {e}. Unread output:\n{}",
                        output.unread_tail(ERROR_TAIL_CHARS)
                    ));
                }
            }

            match output
                .expect(prompt, deadline.saturating_duration_since(Instant::now()))
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
                        output.unread_tail(ERROR_TAIL_CHARS)
                    ));
                }
            }
        }

        Ok(ScriptOutcome { steps, error: None })
    }

    /// Runs shell commands in the default session.
    pub async fn run_script(&self, script: &Script<'_>) -> Result<ScriptOutcome> {
        self.run_script_session(DEFAULT_SESSION_ID, script).await
    }

    /// Resizes the PTY window and the screen grid of the target session.
    pub async fn resize_session(
        &self,
        session_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(u16, u16)> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(session_id)
            .with_context(|| no_session(session_id))?;

        let size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 0,
            cell_height: 0,
        };

        session.pty.on_resize(size);

        let term_size = TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };

        if let Ok(mut terminal) = session.terminal.lock() {
            terminal.resize(term_size);
        }

        session.info.rows = rows;
        session.info.cols = cols;

        if let Ok(mut rec_guard) = session.recorder.lock()
            && let Some(rec) = rec_guard.as_mut()
        {
            let _ = rec.record_resize(cols, rows);
        }

        drop(sessions);

        Ok((rows, cols))
    }

    /// Resizes the default session.
    pub async fn resize(&self, rows: u16, cols: u16) -> Result<(u16, u16)> {
        self.resize_session(DEFAULT_SESSION_ID, rows, cols).await
    }

    /// Reads the current screen of the target session formatted with semantic tags.
    ///
    /// Everything already rendered counts as seen: later stream expects only match newer output.
    pub async fn read_screen_session(&self, session_id: &str) -> Result<String> {
        let (terminal, output) = self
            .with_session(session_id, |s| {
                (Arc::clone(&s.terminal), Arc::clone(&s.output))
            })
            .await?;

        // The reader pushes output while holding the terminal lock, so the screen and the
        // read position are consistent here
        let terminal = lock_terminal(&terminal);
        output.mark_all_read();
        Ok(format_screen(&terminal))
    }

    /// Reads the default session screen.
    pub async fn read_screen(&self) -> Result<String> {
        self.read_screen_session(DEFAULT_SESSION_ID).await
    }

    /// Checks if a session with the given id is currently active.
    pub async fn is_session_active(&self, session_id: &str) -> bool {
        self.sessions.lock().await.contains_key(session_id)
    }

    /// Checks if the default session is currently active.
    pub async fn is_active(&self) -> bool {
        self.is_session_active(DEFAULT_SESSION_ID).await
    }

    /// Lists summaries of all active sessions, sorted by id.
    pub async fn list_sessions(&self) -> Vec<SessionSummary> {
        let sessions = self.sessions.lock().await;
        let mut summaries: Vec<SessionSummary> = sessions
            .iter()
            .map(|(id, s)| SessionSummary {
                id: id.clone(),
                command: s.info.command.clone(),
                pid: s.info.pid,
                rows: s.info.rows,
                cols: s.info.cols,
                recording: s.recorder.lock().is_ok_and(|rec| rec.is_some()),
            })
            .collect();
        drop(sessions);
        summaries.sort_by(|a, b| a.id.cmp(&b.id));
        summaries
    }

    /// Stops the target session, killing its process group and reaping the child.
    pub async fn stop_session(&self, session_id: &str) -> Result<ProcessInfo> {
        let session = self
            .sessions
            .lock()
            .await
            .remove(session_id)
            .with_context(|| no_session(session_id))?;
        let info = session.info.clone();
        teardown(session).await;
        Ok(info)
    }

    /// Stops the default session.
    pub async fn stop_app(&self) -> Result<ProcessInfo> {
        self.stop_session(DEFAULT_SESSION_ID).await
    }
}

/// Everything the reader thread writes PTY output to.
struct ReaderSinks {
    terminal: Arc<std::sync::Mutex<Term<VoidListener>>>,
    output: Arc<SessionOutput>,
    recorder: SharedRecorder,
    shutdown_flag: Arc<AtomicBool>,
}

fn run_pty_reader(mut reader: File, child_exit: &PipeReader, sinks: &ReaderSinks) {
    let ReaderSinks {
        terminal,
        output,
        recorder,
        shutdown_flag,
    } = sinks;
    let mut buffer = [0u8; 4096];
    let mut parser: Processor<StdSyncHandler> = Processor::new();

    while !shutdown_flag.load(Ordering::Relaxed) {
        match next_event(&reader, child_exit, parser.sync_timeout().sync_timeout()) {
            ReaderEvent::Output => {}
            // An app that opens a synchronized update (DECSET 2026) and never closes it would
            // freeze the screen, so flush the frame once its deadline passes, as alacritty's
            // event loop does
            ReaderEvent::SyncTimeout => {
                parser.stop_sync(&mut *lock_terminal(terminal));
                output.notify_screen_changed();
                continue;
            }
            ReaderEvent::ChildExited => {
                tracing::debug!("PTY child exited and its output is drained");
                break;
            }
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                tracing::debug!("PTY reader reached EOF");
                break;
            }
            Ok(n) => {
                let chunk = &buffer[..n];
                let mut term = lock_terminal(terminal);
                parser.advance(&mut *term, chunk);
                // Pushed under the terminal lock so readers see screen and stream together
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

enum ReaderEvent {
    /// The PTY has output, or an error or EOF that the next read will report.
    Output,
    /// A synchronized update is still open after its deadline.
    SyncTimeout,
    /// The child exited and all of its output has been read.
    ChildExited,
}

/// Waits for PTY output, the end of the child, or `sync_deadline`. Output always comes first, so
/// `ChildExited` is only returned once the PTY is drained.
fn next_event(
    reader: &File,
    child_exit: &PipeReader,
    sync_deadline: Option<std::time::Instant>,
) -> ReaderEvent {
    let timeout = sync_deadline.and_then(|deadline| {
        Timespec::try_from(deadline.saturating_duration_since(std::time::Instant::now())).ok()
    });
    let mut fds = [
        PollFd::new(reader, PollFlags::IN),
        PollFd::new(child_exit, PollFlags::IN),
    ];
    loop {
        match poll(&mut fds, timeout.as_ref()) {
            Ok(0) => return ReaderEvent::SyncTimeout,
            Ok(_) if !fds[0].revents().is_empty() => return ReaderEvent::Output,
            Ok(_) => return ReaderEvent::ChildExited,
            Err(rustix::io::Errno::INTR) => {}
            // Let the blocking read surface the error
            Err(_) => return ReaderEvent::Output,
        }
    }
}

/// Blocks until the child exits, then closes `notifier` to wake the reader. Uses `WNOWAIT` so
/// the child is left for `Pty`'s `Drop` to reap.
fn watch_child_exit(pid: u32, notifier: PipeWriter) {
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return;
    };
    // Any error other than EINTR means the child is gone (e.g. already reaped)
    while matches!(
        waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT
        ),
        Err(rustix::io::Errno::INTR)
    ) {}
    drop(notifier);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pty_spawn_and_read() {
        let mgr = PtyManager::new();
        let args = ["hello shadowpty".to_string()];
        let cfg = PtyConfig::new("echo", &args, 10, 40);
        let info = mgr.start_app(&cfg).await.unwrap();

        assert_eq!(info.rows, 10);
        assert_eq!(info.cols, 40);
        assert_eq!(info.session_id, DEFAULT_SESSION_ID);

        let mut screen = String::new();
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            screen = mgr.read_screen().await.unwrap();
            if screen.contains("hello shadowpty") {
                break;
            }
        }
        assert!(screen.contains("hello shadowpty"), "screen was: {screen}");
    }

    #[tokio::test]
    async fn test_pty_resize() {
        let mgr = PtyManager::new();
        let cfg = PtyConfig::new("cat", &[], 10, 40);
        mgr.start_app(&cfg).await.unwrap();

        let (new_rows, new_cols) = mgr.resize(30, 100).await.unwrap();
        assert_eq!(new_rows, 30);
        assert_eq!(new_cols, 100);
    }

    #[tokio::test]
    async fn test_pty_stop_app() {
        let mgr = PtyManager::new();
        assert!(mgr.stop_app().await.is_err());

        let cfg = PtyConfig::new("cat", &[], 10, 40);
        mgr.start_app(&cfg).await.unwrap();
        assert!(mgr.is_active().await);

        let info = mgr.stop_app().await.unwrap();
        assert_eq!(info.command, "cat");
        assert!(!mgr.is_active().await);

        assert!(mgr.stop_app().await.is_err());
    }

    #[tokio::test]
    async fn test_multi_session() {
        let mgr = PtyManager::new();
        let cfg1 = PtyConfig::new("cat", &[], 10, 40);
        let cfg2 = PtyConfig::new("cat", &[], 12, 50);

        mgr.start_session("sess-1", &cfg1).await.unwrap();
        mgr.start_session("sess-2", &cfg2).await.unwrap();

        let sessions = mgr.list_sessions().await;
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["sess-1", "sess-2"]);
        assert_eq!((sessions[1].rows, sessions[1].cols), (12, 50));

        mgr.stop_session("sess-1").await.unwrap();
        assert!(!mgr.is_session_active("sess-1").await);
        assert!(mgr.is_session_active("sess-2").await);

        mgr.stop_session("sess-2").await.unwrap();
        assert!(mgr.list_sessions().await.is_empty());
    }
}
