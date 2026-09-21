//! PTY management and TUI screen state synchronization for ShadowPTY.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system};
use tokio::sync::Mutex;
use vt100::Parser;

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
    pub parser: Arc<std::sync::Mutex<Parser>>,
    pub writer: Box<dyn Write + Send>,
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    recorder: SharedRecorder,
    shutdown_flag: Arc<AtomicBool>,
    reader_handle: Option<JoinHandle<()>>,
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        let _ = self.child.kill();
        let exit_status = self.child.wait().ok();
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
        if let Ok(mut rec_guard) = self.recorder.lock() {
            if let Some(rec) = rec_guard.as_mut() {
                let code = exit_status.map_or(0, |status| i32::from(!status.success()));
                let _ = rec.record_exit(code);
            }
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

    /// Spawns a new TUI application inside a PTY, terminating any previous session.
    pub async fn start_app(&self, config: &PtyConfig<'_>) -> Result<ProcessInfo> {
        let mut session_lock = self.session.lock().await;

        // Drop existing session gracefully
        *session_lock = None;

        let pty_system = native_pty_system();
        let size = PtySize {
            rows: config.rows,
            cols: config.cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let PtyPair { master, slave } = pty_system
            .openpty(size)
            .context("failed to allocate pseudo-terminal")?;

        let mut cmd = CommandBuilder::new(config.command);
        for arg in config.args {
            cmd.arg(arg);
        }

        let child = slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn command '{}' in PTY", config.command))?;

        let pid = child.process_id();
        let reader = master
            .try_clone_reader()
            .context("failed to clone master PTY reader")?;
        let writer = master
            .take_writer()
            .context("failed to take master PTY writer")?;

        let parser = Arc::new(std::sync::Mutex::new(Parser::new(
            config.rows,
            config.cols,
            0,
        )));
        let shutdown_flag = Arc::new(AtomicBool::new(false));

        let recorder = if let Some(path) = config.record_path {
            let rec =
                AsciicastRecorder::create(path, config.cols, config.rows, Some(config.command))
                    .with_context(|| format!("failed to initialize recorder with path '{path}'"))?;
            Arc::new(std::sync::Mutex::new(Some(rec)))
        } else {
            Arc::new(std::sync::Mutex::new(None))
        };

        // Background reader thread feeding bytes to vt100::Parser and recorder
        let reader_parser = Arc::clone(&parser);
        let reader_shutdown = Arc::clone(&shutdown_flag);
        let reader_recorder = Arc::clone(&recorder);
        let reader_handle = thread::Builder::new()
            .name("shadowpty-reader".to_string())
            .spawn(move || {
                run_pty_reader(reader, &reader_parser, &reader_recorder, &reader_shutdown);
            })
            .context("failed to spawn PTY reader thread")?;

        let info = ProcessInfo {
            pid,
            command: config.command.to_string(),
            rows: config.rows,
            cols: config.cols,
        };

        *session_lock = Some(TuiSession {
            info: info.clone(),
            parser,
            writer,
            master,
            child,
            recorder,
            shutdown_flag,
            reader_handle: Some(reader_handle),
        });
        drop(session_lock);

        Ok(info)
    }

    /// Sends keystrokes with symbolic tokens (e.g. `<ENTER>`, `<UP>`) to the PTY.
    pub async fn send_input(&self, keys: &str) -> Result<usize> {
        let mut session_lock = self.session.lock().await;
        let session = session_lock
            .as_mut()
            .context("no active PTY session; call tui_start first")?;

        let bytes = parse_input_keys(keys);
        session
            .writer
            .write_all(&bytes)
            .context("failed to write keys to PTY")?;
        session
            .writer
            .flush()
            .context("failed to flush PTY writer")?;

        if let Ok(mut rec_guard) = session.recorder.lock() {
            if let Some(rec) = rec_guard.as_mut() {
                let _ = rec.record_input(&bytes);
            }
        }

        drop(session_lock);

        Ok(bytes.len())
    }

    /// Resizes the PTY terminal window and updates the screen parser dimensions.
    pub async fn resize(&self, rows: u16, cols: u16) -> Result<(u16, u16)> {
        let mut session_lock = self.session.lock().await;
        let session = session_lock
            .as_mut()
            .context("no active PTY session; call tui_start first")?;

        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        session
            .master
            .resize(size)
            .context("failed to resize PTY master")?;

        if let Ok(mut parser) = session.parser.lock() {
            parser.screen_mut().set_size(rows, cols);
        }

        session.info.rows = rows;
        session.info.cols = cols;

        if let Ok(mut rec_guard) = session.recorder.lock() {
            if let Some(rec) = rec_guard.as_mut() {
                let _ = rec.record_resize(cols, rows);
            }
        }

        drop(session_lock);

        Ok((rows, cols))
    }

    /// Reads the current TUI screen state formatted with semantic tags.
    pub async fn read_screen(&self) -> Result<String> {
        let session_lock = self.session.lock().await;
        let session = session_lock
            .as_ref()
            .context("no active PTY session; call tui_start first")?;

        let parser = session
            .parser
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to acquire lock on vt100 parser"))?;

        let result = format_screen(parser.screen());
        drop(parser);
        drop(session_lock);

        Ok(result)
    }

    /// Checks if a session is currently active.
    pub async fn is_active(&self) -> bool {
        let session_lock = self.session.lock().await;
        session_lock.is_some()
    }
}

fn run_pty_reader(
    mut reader: Box<dyn Read + Send>,
    parser: &Arc<std::sync::Mutex<Parser>>,
    recorder: &SharedRecorder,
    shutdown_flag: &Arc<AtomicBool>,
) {
    let mut buffer = [0u8; 4096];

    while !shutdown_flag.load(Ordering::Relaxed) {
        match reader.read(&mut buffer) {
            Ok(0) => {
                // EOF reached (child closed or exited)
                tracing::debug!("PTY reader reached EOF");
                break;
            }
            Ok(n) => {
                let chunk = &buffer[..n];
                if let Ok(mut locked_parser) = parser.lock() {
                    locked_parser.process(chunk);
                }
                if let Ok(mut rec_guard) = recorder.lock() {
                    if let Some(rec) = rec_guard.as_mut() {
                        let _ = rec.record_output(chunk);
                    }
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

        // Give echo a moment to write and exit
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
}
