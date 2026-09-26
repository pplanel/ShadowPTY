//! PTY management and TUI screen state synchronization for ShadowPTY.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::fs::File;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use alacritty_terminal::tty::{self, Options, Pty, Shell};
use alacritty_terminal::term::{Term, Config};
use alacritty_terminal::event::{VoidListener, WindowSize, OnResize};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use alacritty_terminal::grid::Dimensions;
use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};

struct TermSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize { self.screen_lines }
    fn screen_lines(&self) -> usize { self.screen_lines }
    fn columns(&self) -> usize { self.columns }
}

use crate::formatter::format_screen;
use crate::input::parse_input_keys;
use crate::recorder::{AsciicastRecorder, SharedRecorder};

/// Information about a running process session.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProcessInfo {
    pub pid: Option<u32>,
    pub command: String,
    pub rows: u16,
    pub cols: u16,
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
    pub pty_writer: File,
    pub pty: Pty,
    recorder: SharedRecorder,
    shutdown_flag: Arc<AtomicBool>,
    _reader_handle: Option<JoinHandle<()>>,
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

/// Thread-safe manager for the current headless PTY session.
#[derive(Clone, Default)]
pub struct PtyManager {
    session: Arc<Mutex<Option<TuiSession>>>,
}

impl PtyManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            session: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn start_app(&self, config: &PtyConfig<'_>) -> Result<ProcessInfo> {
        let mut session_lock = self.session.lock().await;

        *session_lock = None;

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

        let terminal = Arc::new(std::sync::Mutex::new(Term::new(Config::default(), &term_size, VoidListener)));
        let shutdown_flag = Arc::new(AtomicBool::new(false));

        let recorder = if let Some(path) = config.record_path {
            let rec =
                AsciicastRecorder::create(path, config.cols, config.rows, Some(config.command))
                    .with_context(|| format!("failed to initialize recorder with path '{path}'"))?;
            Arc::new(std::sync::Mutex::new(Some(rec)))
        } else {
            Arc::new(std::sync::Mutex::new(None))
        };

        let reader_terminal = Arc::clone(&terminal);
        let reader_shutdown = Arc::clone(&shutdown_flag);
        let reader_recorder = Arc::clone(&recorder);
        let reader_handle = thread::Builder::new()
            .name("shadowpty-reader".to_string())
            .spawn(move || {
                run_pty_reader(pty_reader, &reader_terminal, &reader_recorder, &reader_shutdown);
            })
            .context("failed to spawn PTY reader thread")?;

        let info = ProcessInfo {
            pid: Some(pid),
            command: config.command.to_string(),
            rows: config.rows,
            cols: config.cols,
        };

        *session_lock = Some(TuiSession {
            info: info.clone(),
            terminal,
            pty_writer,
            pty,
            recorder,
            shutdown_flag,
            _reader_handle: Some(reader_handle),
        });
        drop(session_lock);

        Ok(info)
    }

    pub async fn send_input(&self, keys: &str) -> Result<usize> {
        let mut session_lock = self.session.lock().await;
        let session = session_lock
            .as_mut()
            .context("no active PTY session; call tui_start first")?;

        let bytes = parse_input_keys(keys);
        session
            .pty_writer
            .write_all(&bytes)
            .context("failed to write keys to PTY")?;
        session
            .pty_writer
            .flush()
            .context("failed to flush PTY writer")?;

        if let Ok(mut rec_guard) = session.recorder.lock()
            && let Some(rec) = rec_guard.as_mut()
        {
            let _ = rec.record_input(&bytes);
        }

        drop(session_lock);

        Ok(bytes.len())
    }

    pub async fn resize(&self, rows: u16, cols: u16) -> Result<(u16, u16)> {
        let mut session_lock = self.session.lock().await;
        let session = session_lock
            .as_mut()
            .context("no active PTY session; call tui_start first")?;

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

        drop(session_lock);

        Ok((rows, cols))
    }

    pub async fn read_screen(&self) -> Result<String> {
        let session_lock = self.session.lock().await;
        let session = session_lock
            .as_ref()
            .context("no active PTY session; call tui_start first")?;

        let terminal = session
            .terminal
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to acquire lock on terminal"))?;

        let result = format_screen(&terminal);
        drop(terminal);
        drop(session_lock);

        Ok(result)
    }

    pub async fn is_active(&self) -> bool {
        let session_lock = self.session.lock().await;
        session_lock.is_some()
    }

    pub async fn stop_app(&self) -> Result<ProcessInfo> {
        let mut session_lock = self.session.lock().await;
        let session = session_lock
            .take()
            .context("no active PTY session; call tui_start first")?;
        drop(session_lock);
        let info = session.info.clone();
        drop(session);
        Ok(info)
    }
}

fn run_pty_reader(
    mut reader: File,
    terminal: &Arc<std::sync::Mutex<Term<VoidListener>>>,
    recorder: &SharedRecorder,
    shutdown_flag: &Arc<AtomicBool>,
) {
    let mut buffer = [0u8; 4096];
    let mut parser: Processor<StdSyncHandler> = Processor::new();

    while !shutdown_flag.load(Ordering::Relaxed) {
        match reader.read(&mut buffer) {
            Ok(0) => {
                tracing::debug!("PTY reader reached EOF");
                break;
            }
            Ok(n) => {
                let chunk = &buffer[..n];
                if let Ok(mut term) = terminal.lock() {
                    parser.advance(&mut *term, chunk);
                }
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

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let screen = mgr.read_screen().await.unwrap();
        assert!(screen.contains("hello shadowpty"));
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
}
