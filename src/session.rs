//! Autonomous TUI session aggregate root managing OS PTY, terminal emulation,
//! background output reader, and synchronous/asynchronous interactions.

use std::fs::File;
use std::io::{PipeReader, PipeWriter, Read, Write};
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
use rustix::process::{Pid, WaitId, WaitIdOptions, WaitIdStatus, waitid};
use rustix_openpty::openpty;
use rustix_openpty::rustix::termios::Winsize;
use tokio::sync::watch;

use crate::input::parse_input_keys;
use crate::output::{Pattern, SessionOutput, describe_patterns, first_match};
use crate::recorder::{AsciicastRecorder, SharedRecorder};
use crate::screen::Screen;

/// Session id used when a tool call doesn't specify one.
pub const DEFAULT_SESSION_ID: &str = "default";

/// Bracketed paste markers (DECSET 2004).
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// How much recent output or screen text to include in a failed wait's error message.
const ERROR_TAIL_CHARS: usize = 500;

/// How long `terminate` waits for the reader thread before giving up on it.
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the reader waits for the exit watcher after the PTY closes, to record the status.
const EXIT_STATUS_WAIT: Duration = Duration::from_secs(1);

/// How long `wait_exit` keeps waiting for output to drain after the child exits.
const EXIT_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// How much unread output `wait_exit` returns.
const EXIT_OUTPUT_TAIL_CHARS: usize = 4000;

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
    /// `None` while the process is running.
    pub exit_status: Option<ExitStatus>,
}

/// How the session's process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    /// Exited normally with this code.
    #[serde(rename = "exit_code")]
    Code(i32),
    /// Killed by this signal number.
    Signal(i32),
    /// Exited, but the status was no longer available (the child was already reaped).
    Unknown,
}

impl ExitStatus {
    fn from_waitid(status: &WaitIdStatus) -> Self {
        status
            .exit_status()
            .map(Self::Code)
            .or_else(|| status.terminating_signal().map(Self::Signal))
            .unwrap_or(Self::Unknown)
    }

    /// Code written to the recording: the exit code, or `128 + signal` as shells report it.
    #[must_use]
    pub const fn recorded_code(self) -> i32 {
        match self {
            Self::Code(code) => code,
            Self::Signal(signal) => 128 + signal,
            Self::Unknown => -1,
        }
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Code(code) => write!(f, "exited with code {code}"),
            Self::Signal(signal) => match signal_name(*signal) {
                Some(name) => write!(f, "killed by signal {signal} ({name})"),
                None => write!(f, "killed by signal {signal}"),
            },
            Self::Unknown => write!(f, "exited (status unavailable)"),
        }
    }
}

/// Names of common signals whose numbers are the same on Linux and macOS.
const fn signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return None,
    })
}

/// Result of waiting for the process to exit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProcessExit {
    pub status: ExitStatus,
    /// Output the agent hadn't seen yet, as plain text (the last part if it's long).
    pub output: String,
}

/// Where expect searches for its pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectTarget {
    /// Output not yet consumed by an earlier expect or screen read; a match consumes it.
    Stream,
    /// The rendered screen text, re-checked whenever output arrives.
    Screen,
}

/// What to wait for, where, and for how long. With several patterns, the first to match wins.
#[derive(Debug, Clone)]
pub struct Expectation {
    pub patterns: Vec<Pattern>,
    pub target: ExpectTarget,
    pub timeout: Duration,
}

impl Expectation {
    /// Waits for a single pattern.
    #[must_use]
    pub fn new(pattern: Pattern, target: ExpectTarget, timeout: Duration) -> Self {
        Self {
            patterns: vec![pattern],
            target,
            timeout,
        }
    }
}

/// Which pattern an expectation matched, and the matched text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectMatch {
    /// Index into `Expectation::patterns`.
    pub index: usize,
    pub matched: String,
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
    exit: watch::Receiver<Option<ExitStatus>>,
}

fn run_pty_reader(mut reader: File, child_exit: &PipeReader, sinks: &ReaderSinks) {
    let ReaderSinks {
        terminal,
        output,
        recorder,
        shutdown_flag,
        exit,
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
                parser.stop_sync(&mut *lock_mutex(terminal));
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
    // Recorded before closing the stream, so the exit event follows the last output and
    // `wait_exit` sees a complete recording
    record_exit_status(child_exit, exit, recorder);
    output.close();
}

/// Writes the child's exit status to the recording. The PTY can close slightly before the exit
/// watcher reports, so waits up to `EXIT_STATUS_WAIT` for it.
fn record_exit_status(
    child_exit: &PipeReader,
    exit: &watch::Receiver<Option<ExitStatus>>,
    recorder: &SharedRecorder,
) {
    if !recorder.lock().is_ok_and(|rec| rec.is_some()) {
        return;
    }
    if exit.borrow().is_none() {
        wait_for_exit_report(child_exit);
    }
    let status = *exit.borrow();
    if let Some(status) = status
        && let Ok(mut rec_guard) = recorder.lock()
        && let Some(rec) = rec_guard.as_mut()
    {
        let _ = rec.record_exit(status.recorded_code());
    }
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

/// Blocks until the child exits, publishes its status, then closes `notifier` to wake the
/// reader. Uses `WNOWAIT` so the child is left for `Pty`'s `Drop` to reap.
fn watch_child_exit(pid: u32, exit: &watch::Sender<Option<ExitStatus>>, notifier: PipeWriter) {
    let status = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .map_or(ExitStatus::Unknown, wait_for_child);
    exit.send_replace(Some(status));
    drop(notifier);
}

/// Blocks until the exit watcher has published the child's status (it closes the pipe right
/// after), for at most `EXIT_STATUS_WAIT`.
fn wait_for_exit_report(child_exit: &PipeReader) {
    let timeout = Timespec::try_from(EXIT_STATUS_WAIT).ok();
    let mut fds = [PollFd::new(child_exit, PollFlags::IN)];
    while matches!(
        poll(&mut fds, timeout.as_ref()),
        Err(rustix::io::Errno::INTR)
    ) {}
}

/// Opens a PTY of the configured size and spawns the command in it. Also returns a second handle
/// on the PTY slave: macOS discards unread output shortly after the last slave fd closes, so the
/// reader keeps this one open until it has drained everything the child wrote.
fn spawn_in_pty(config: &PtyConfig<'_>) -> Result<(Pty, std::os::fd::OwnedFd)> {
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
    let slave_keepalive = opened
        .user
        .try_clone()
        .context("failed to clone pty slave")?;
    let pty = tty::from_fd(&options, 0, opened.controller, opened.user)
        .map_err(|e| anyhow::anyhow!("failed to spawn '{}': {e}", config.command))?;
    Ok((pty, slave_keepalive))
}

/// Creates the asciicast recorder if the config asks for a recording.
fn create_recorder(config: &PtyConfig<'_>) -> Result<SharedRecorder> {
    let recorder = match config.record_path {
        Some(path) => Some(
            AsciicastRecorder::create(path, config.cols, config.rows, Some(config.command))
                .with_context(|| format!("failed to initialize recorder with path '{path}'"))?,
        ),
        None => None,
    };
    Ok(Arc::new(Mutex::new(recorder)))
}

/// Handles on the thread that watches for the child's exit.
struct ExitWatch {
    /// Becomes readable once the exit is reported; used by the reader.
    child_exit: PipeReader,
    /// A second handle on the same pipe, for the session.
    exit_reported: PipeReader,
    status: watch::Receiver<Option<ExitStatus>>,
}

/// Starts the thread that watches for the child's exit.
fn spawn_exit_watcher(session_id: &str, pid: u32) -> Result<ExitWatch> {
    let (child_exit, notifier) = std::io::pipe().context("failed to create exit pipe")?;
    let exit_reported = child_exit
        .try_clone()
        .context("failed to clone exit pipe")?;
    let (exit_tx, status) = watch::channel(None);
    thread::Builder::new()
        .name(format!("shadowpty-exit-{session_id}"))
        .spawn(move || watch_child_exit(pid, &exit_tx, notifier))
        .context("failed to spawn child exit watcher")?;
    Ok(ExitWatch {
        child_exit,
        exit_reported,
        status,
    })
}

fn wait_for_child(pid: Pid) -> ExitStatus {
    loop {
        match waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        ) {
            Err(rustix::io::Errno::INTR) => {}
            Ok(Some(status)) => return ExitStatus::from_waitid(&status),
            // Any other error means the child is gone (e.g. already reaped)
            Ok(None) | Err(_) => return ExitStatus::Unknown,
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
    exit: watch::Receiver<Option<ExitStatus>>,
    /// Readable once the exit watcher has published the status.
    exit_reported: PipeReader,
}

impl TuiSession {
    /// Allocates a PTY, spawns the command in it and starts the background reader thread.
    pub fn spawn(session_id: &str, config: &PtyConfig<'_>) -> Result<Self> {
        let (pty, slave_keepalive) = spawn_in_pty(config)?;

        let pid = pty.child().id();
        let ExitWatch {
            child_exit,
            exit_reported,
            status: exit,
        } = spawn_exit_watcher(session_id, pid)?;

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

        let recorder = create_recorder(config)?;

        let sinks = ReaderSinks {
            terminal: Arc::clone(&terminal),
            output: Arc::clone(&output),
            recorder: Arc::clone(&recorder),
            shutdown_flag: Arc::clone(&shutdown_flag),
            exit: exit.clone(),
        };
        let reader_handle = thread::Builder::new()
            .name(format!("shadowpty-reader-{session_id}"))
            .spawn(move || {
                run_pty_reader(pty_reader, &child_exit, &sinks);
                drop(slave_keepalive);
            })
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
            exit,
            exit_reported,
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
            exit_status: self.exit_status(),
        }
    }

    /// How the process ended, or `None` while it's running.
    #[must_use]
    pub fn exit_status(&self) -> Option<ExitStatus> {
        *self.exit.borrow()
    }

    /// Waits for the process to exit, then returns its status and the output the agent hadn't
    /// seen yet (marked as read).
    pub async fn wait_exit(&self, timeout: Duration) -> Result<ProcessExit> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut exit = self.exit.clone();
        let status = match tokio::time::timeout_at(deadline, exit.wait_for(Option::is_some)).await {
            Ok(Ok(status)) => status.unwrap_or(ExitStatus::Unknown),
            // The watcher is gone without reporting, so the status can't be known
            Ok(Err(_)) => ExitStatus::Unknown,
            Err(_) => anyhow::bail!(
                "process still running after {timeout:?}. Unread output (last {ERROR_TAIL_CHARS} chars):\n{}",
                self.output.unread_tail(ERROR_TAIL_CHARS)
            ),
        };

        // Let the reader drain what the child wrote before exiting
        let drain = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .max(EXIT_DRAIN_GRACE);
        let _ = self
            .output
            .wait_for(drain, || self.output.is_eof().then_some(()))
            .await;

        let output = self.output.unread_tail(EXIT_OUTPUT_TAIL_CHARS);
        self.output.mark_all_read();
        Ok(ProcessExit { status, output })
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

    /// Plain text of the current screen.
    fn screen_text(&self) -> String {
        let screen = {
            let term = lock_mutex(&self.terminal);
            Screen::capture(&term)
        };
        screen.to_plain_text()
    }

    /// Waits until one of the expectation's patterns matches, on the rendered screen or in the
    /// unread output. With several patterns, the earliest match wins (ties: the first listed).
    pub async fn expect(&self, expectation: &Expectation) -> Result<ExpectMatch> {
        let Expectation {
            patterns,
            target,
            timeout,
        } = expectation;
        anyhow::ensure!(!patterns.is_empty(), "no pattern to wait for");
        let described = describe_patterns(patterns);

        match target {
            ExpectTarget::Screen => self
                .output
                .wait_for(*timeout, || {
                    let text = self.screen_text();
                    let (index, range) = first_match(patterns, text.as_bytes())?;
                    Some(ExpectMatch {
                        index,
                        matched: String::from_utf8_lossy(&text.as_bytes()[range]).into_owned(),
                    })
                })
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "{described} not found on screen: {e}. Current screen:\n{}",
                        self.screen_text()
                    )
                }),
            ExpectTarget::Stream => match self.output.expect_any(patterns, *timeout).await {
                Ok(found) => Ok(ExpectMatch {
                    index: found.index,
                    matched: found.matched,
                }),
                Err(e) => Err(anyhow::anyhow!(
                    "{described} not found in output: {e}. Unread output (last {ERROR_TAIL_CHARS} chars):\n{}",
                    self.output.unread_tail(ERROR_TAIL_CHARS)
                )),
            },
        }
    }

    /// Waits until none of `patterns` is on the rendered screen (e.g. a spinner or "Loading…")
    /// and returns how long that took. Returns at once if they're already absent, and fails as
    /// soon as the process has exited with the text still showing.
    pub async fn wait_gone(&self, patterns: &[Pattern], timeout: Duration) -> Result<Duration> {
        anyhow::ensure!(!patterns.is_empty(), "no pattern to wait for");
        let started = tokio::time::Instant::now();
        self.output
            .wait_for(timeout, || {
                first_match(patterns, self.screen_text().as_bytes())
                    .is_none()
                    .then_some(())
            })
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "{} still on screen: {e}. Current screen:\n{}",
                    describe_patterns(patterns),
                    self.screen_text()
                )
            })?;
        Ok(started.elapsed())
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
            let join = tokio::task::spawn_blocking(move || {
                let _ = handle.join();
            });
            // Never let teardown hang on a reader that doesn't see the exit: kill the child
            // directly and leave the reader thread behind
            if tokio::time::timeout(READER_JOIN_TIMEOUT, join)
                .await
                .is_err()
            {
                tracing::warn!(
                    "PTY reader didn't stop within {READER_JOIN_TIMEOUT:?}; sending SIGKILL"
                );
                #[cfg(unix)]
                if let Some(pid) = self.info().pid {
                    let _ = std::process::Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
        }

        // Normally already recorded by the reader; covers a reader that didn't finish in time
        if let Some(status) = self.exit_status()
            && let Ok(mut rec_guard) = self.recorder.lock()
            && let Some(rec) = rec_guard.as_mut()
        {
            let _ = rec.record_exit(status.recorded_code());
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
        // `Pty`'s `Drop` reaps the child right after this, which would lose its exit status, so
        // let the watcher read it first; the reader thread then records it
        if self.exit_status().is_none() {
            wait_for_exit_report(&self.exit_reported);
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
            patterns: vec![Pattern::literal("DIRECT_SESSION_OK").expect("pat")],
            target: ExpectTarget::Stream,
            timeout: Duration::from_secs(5),
        };
        let matched = session.expect(&exp).await.expect("expect");
        assert_eq!(matched.matched, "DIRECT_SESSION_OK");
        assert_eq!(matched.index, 0);

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
            patterns: vec![Pattern::literal("PASTED_OK").expect("pat")],
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
