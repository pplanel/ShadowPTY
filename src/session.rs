//! Autonomous TUI session aggregate root managing OS PTY, terminal emulation,
//! background output reader, and synchronous/asynchronous interactions.

use std::fs::File;
use std::io::{PipeReader, PipeWriter, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{OnResize, VoidListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options, Pty, Shell};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use anyhow::{Context, Result};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, WaitIdStatus, kill_process, kill_process_group, waitid,
};
use rustix_openpty::openpty;
use rustix_openpty::rustix::termios::Winsize;
use tokio::sync::{broadcast, watch};

use crate::input::parse_input_keys;
use crate::output::{Pattern, SessionOutput, describe_patterns, first_match};
use crate::recorder::{AsciicastRecorder, SharedRecorder};
use crate::report::{Outcome, ReportEvent, ReportTotals, ScreenshotTaken, SessionReport};
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

/// How long an ended session's `Pty` is kept, so the exit watcher can read the killed child's
/// status before `Pty`'s `Drop` reaps it.
const REAP_DELAY_LIMIT: Duration = Duration::from_secs(10);

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
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

/// Which pattern an expectation matched, the matched text, and where it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectMatch {
    /// Index into `Expectation::patterns`.
    pub index: usize,
    pub matched: String,
    /// Stream mode: unread output before the match (now consumed). Empty in screen mode.
    pub before: String,
    /// Stream mode: output right after the match, still unread. Empty in screen mode.
    pub after: String,
    /// Screen mode: the row where the match starts.
    pub line: Option<ScreenLine>,
}

/// A row of the rendered screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenLine {
    /// 0-based row index.
    pub row: usize,
    /// The row's text, trailing spaces trimmed.
    pub text: String,
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
    /// Where to write the JSON Lines session report, if anywhere.
    pub report_path: Option<&'a str>,
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
            report_path: None,
        }
    }

    #[must_use]
    pub const fn with_record_path(mut self, record_path: Option<&'a str>) -> Self {
        self.record_path = record_path;
        self
    }

    #[must_use]
    pub const fn with_report_path(mut self, report_path: Option<&'a str>) -> Self {
        self.report_path = report_path;
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
    report: Arc<SessionReport>,
    shutdown_flag: Arc<AtomicBool>,
    exit: watch::Receiver<Option<ExitStatus>>,
}

fn run_pty_reader(mut reader: File, child_exit: &PipeReader, sinks: &ReaderSinks) {
    let ReaderSinks {
        terminal,
        output,
        recorder,
        shutdown_flag,
        ..
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
    // `wait_exit` sees a complete recording and report
    record_exit_status(child_exit, sinks);
    output.close();
}

/// Writes the child's exit status to the recording and the report. The PTY can close slightly
/// before the exit watcher reports, so waits up to `EXIT_STATUS_WAIT` for it.
fn record_exit_status(child_exit: &PipeReader, sinks: &ReaderSinks) {
    let ReaderSinks {
        recorder,
        report,
        exit,
        ..
    } = sinks;
    // Only wait when someone will see the status
    let recording = recorder.lock().is_ok_and(|rec| rec.is_some());
    if exit.borrow().is_none() && (recording || report.is_observed()) {
        wait_for_exit_report(child_exit, EXIT_STATUS_WAIT);
    }
    let Some(status) = *exit.borrow() else {
        return;
    };
    if let Ok(mut rec_guard) = recorder.lock()
        && let Some(rec) = rec_guard.as_mut()
    {
        let _ = rec.record_exit(status.recorded_code());
    }
    report.record_exit(status);
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

/// Sends `signal` to the child's process group and to the child itself. The child only starts
/// its own group (setsid) right after it's forked, so a session ended immediately after `tui_start`
/// can have no group yet; signalling the process too makes sure it's hit either way. The child is
/// only reaped when `Pty` drops, after every caller of this, so its pid can't have been reused.
fn signal_child(pid: Option<u32>, signal: Signal) {
    if let Some(pid) = pid
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(Pid::from_raw)
    {
        let _ = kill_process_group(pid, signal);
        let _ = kill_process(pid, signal);
    }
}

/// Blocks until the exit watcher has published the child's status (it closes the pipe right
/// after), for at most `limit`.
fn wait_for_exit_report(child_exit: &PipeReader, limit: Duration) {
    let timeout = Timespec::try_from(limit).ok();
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
    /// When the session started; the time base of its report.
    started: Instant,
    report: Arc<SessionReport>,
    /// Taken when the session drops; see `Drop`.
    pty: Mutex<Option<Pty>>,
    pty_writer: Arc<Mutex<File>>,
    terminal: Arc<Mutex<Term<VoidListener>>>,
    output: Arc<SessionOutput>,
    recorder: SharedRecorder,
    shutdown_flag: Arc<AtomicBool>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    exit: watch::Receiver<Option<ExitStatus>>,
    /// Readable once the exit watcher has published the status.
    /// Taken when the session drops; see `Drop`.
    exit_reported: Option<PipeReader>,
}

impl TuiSession {
    /// Allocates a PTY, spawns the command in it and starts the background reader thread.
    pub fn spawn(session_id: &str, config: &PtyConfig<'_>) -> Result<Self> {
        let started = Instant::now();
        // Created before spawning, so a bad path fails without starting anything
        let report = Arc::new(SessionReport::create(config.report_path, started)?);
        let (pty, slave_keepalive) = spawn_in_pty(config)?;

        let pid = pty.child().id();
        report.record_start(session_id, config, Some(pid));
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
            report: Arc::clone(&report),
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
            started,
            report,
            terminal,
            output,
            pty_writer: Arc::new(Mutex::new(pty_writer)),
            pty: Mutex::new(Some(pty)),
            recorder,
            shutdown_flag,
            reader_handle: Mutex::new(Some(reader_handle)),
            exit,
            exit_reported: Some(exit_reported),
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

    /// When the session started.
    #[must_use]
    pub const fn started(&self) -> Instant {
        self.started
    }

    /// Receives the session's report events from now on, whether or not it writes a report
    /// file.
    #[must_use]
    pub fn subscribe_report(&self) -> broadcast::Receiver<ReportEvent> {
        self.report.subscribe()
    }

    /// Checks run so far, and how many passed and failed.
    #[must_use]
    pub fn report_totals(&self) -> ReportTotals {
        self.report.totals()
    }

    /// The report file, if the session writes one.
    #[must_use]
    pub fn report_path(&self) -> Option<String> {
        self.report.path().map(str::to_string)
    }

    /// Adds a screenshot taken of this session to its report.
    pub fn record_screenshot(&self, shot: &ScreenshotTaken<'_>) {
        self.report.record_screenshot(shot);
    }

    /// Waits for the process to exit, then returns its status and the output the agent hadn't
    /// seen yet (marked as read).
    pub async fn wait_exit(&self, timeout: Duration) -> Result<ProcessExit> {
        let started = Instant::now();
        let result = self.wait_exit_unreported(timeout).await;
        self.report
            .record_wait_exit(timeout, &Outcome::new(&result, started.elapsed()));
        result
    }

    async fn wait_exit_unreported(&self, timeout: Duration) -> Result<ProcessExit> {
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
        self.report.record_input(keys, bytes.len());
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
        self.report.record_paste(text, text.len());
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

        if let Some(pty) = lock_mutex(&self.pty).as_mut() {
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
        self.report.record_resize(rows, cols);

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
        let started = Instant::now();
        let result = self.expect_unreported(expectation).await;
        self.report
            .record_expect(expectation, &Outcome::new(&result, started.elapsed()));
        result
    }

    async fn expect_unreported(&self, expectation: &Expectation) -> Result<ExpectMatch> {
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
                    let bytes = text.as_bytes();
                    let (index, range) = first_match(patterns, bytes)?;
                    let row = bytes[..range.start].split(|&b| b == b'\n').count() - 1;
                    Some(ExpectMatch {
                        index,
                        matched: String::from_utf8_lossy(&bytes[range]).into_owned(),
                        before: String::new(),
                        after: String::new(),
                        line: Some(ScreenLine {
                            row,
                            text: text.split('\n').nth(row).unwrap_or_default().to_string(),
                        }),
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
                    before: found.before,
                    after: found.after,
                    line: None,
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
        let started = Instant::now();
        let result = self.wait_gone_unreported(patterns, timeout).await;
        self.report
            .record_wait_gone(patterns, timeout, &Outcome::new(&result, started.elapsed()));
        result
    }

    async fn wait_gone_unreported(
        &self,
        patterns: &[Pattern],
        timeout: Duration,
    ) -> Result<Duration> {
        anyhow::ensure!(!patterns.is_empty(), "no pattern to wait for");
        let started = Instant::now();
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
        let started = Instant::now();
        let result = self
            .output
            .wait_stable(quiet_period, timeout)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"));
        self.report.record_wait_stable(
            quiet_period,
            timeout,
            &Outcome::new(&result, started.elapsed()),
        );
        result
    }

    /// Sequentially executes commands and validates prompt appearances.
    pub async fn run_script(&self, script: &Script<'_>) -> Result<ScriptOutcome> {
        let started = Instant::now();
        let result = self.run_script_unreported(script).await;
        self.report
            .record_run_script(script, &Outcome::new(&result, started.elapsed()));
        result
    }

    async fn run_script_unreported(&self, script: &Script<'_>) -> Result<ScriptOutcome> {
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
        let pid = self.info().pid;
        signal_child(pid, Signal::TERM);
        self.shutdown_flag.store(true, Ordering::SeqCst);
        // Escalate right away: interactive shells ignore SIGTERM
        signal_child(pid, Signal::KILL);

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
                signal_child(pid, Signal::KILL);
            }
        }

        // Normally already recorded by the reader; covers a reader that didn't finish in time
        let mut exit = self.exit.clone();
        let _ = tokio::time::timeout(EXIT_STATUS_WAIT, exit.wait_for(Option::is_some)).await;
        let status = self.exit_status();
        if let Some(status) = status
            && let Ok(mut rec_guard) = self.recorder.lock()
            && let Some(rec) = rec_guard.as_mut()
        {
            let _ = rec.record_exit(status.recorded_code());
        }
        self.report.finish(status);

        Ok(())
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        let pid = read_rwlock(&self.info).pid;
        signal_child(pid, Signal::KILL);
        self.shutdown_flag.store(true, Ordering::SeqCst);

        let exit_reported = self.exit_reported.take();
        // Finish the report now, so it's complete when the session is gone. The killed child's
        // status normally arrives within milliseconds.
        if self.report.is_observed() {
            if let Some(exit_reported) = &exit_reported
                && self.exit_status().is_none()
            {
                wait_for_exit_report(exit_reported, EXIT_STATUS_WAIT);
            }
            self.report.finish(self.exit_status());
        }

        // `Pty`'s `Drop` reaps the child, and once it's reaped its exit status is gone. So keep
        // the `Pty` alive in the background until the exit watcher has read the status (the
        // reader thread then records it), instead of racing it here.
        let pty = lock_mutex(&self.pty).take();
        if let (Some(pty), Some(exit_reported)) = (pty, exit_reported) {
            let report = Arc::clone(&self.report);
            let exit = self.exit.clone();
            let reaper = thread::Builder::new()
                .name(format!("shadowpty-reap-{}", pid.unwrap_or_default()))
                .spawn(move || {
                    wait_for_exit_report(&exit_reported, REAP_DELAY_LIMIT);
                    // Normally already finished by `drop`
                    let status = *exit.borrow();
                    report.finish(status);
                    drop(pty);
                });
            if let Err(e) = reaper {
                tracing::warn!("failed to spawn PTY reaper thread: {e}");
            }
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
