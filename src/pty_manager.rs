//! PTY session management and routing for ShadowPTY.
//!
//! [`PtyManager`] manages active concurrent headless [`TuiSession`] instances,
//! routing requests by session ID.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rustix::process::Signal;
use tokio::sync::{Mutex, broadcast};

use crate::output::Pattern;
pub use crate::report::{ReportEvent, ReportTotals, ScreenshotTaken};
use crate::screen::Screen;
pub use crate::session::{
    DEFAULT_SESSION_ID, ExitStatus, ExpectMatch, ExpectTarget, Expectation, ProcessExit,
    ProcessInfo, PtyConfig, ScreenLine, Script, ScriptOutcome, ScriptStep, SessionSummary,
    SignalDelivery, SignalTarget, TuiSession,
};

fn no_session(session_id: &str) -> String {
    format!("no active PTY session with id '{session_id}'; call tui_start first")
}

/// A session that was stopped, and the totals of its report.
#[derive(Debug, Clone)]
pub struct StoppedSession {
    pub info: ProcessInfo,
    /// The report file, if the session wrote one. It's complete (exit and summary) by the time
    /// `stop_session` returns.
    pub report_path: Option<String>,
    pub totals: ReportTotals,
}

/// Thread-safe manager for concurrent headless PTY sessions, keyed by session id.
#[derive(Clone, Default)]
pub struct PtyManager {
    sessions: Arc<Mutex<HashMap<String, Arc<TuiSession>>>>,
}

impl PtyManager {
    /// Creates a new empty `PtyManager`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Fetches an Arc to the target session under the short registry lock.
    async fn get_session(&self, session_id: &str) -> Result<Arc<TuiSession>> {
        let sessions = self.sessions.lock().await;
        sessions
            .get(session_id)
            .cloned()
            .with_context(|| no_session(session_id))
    }

    /// Spawns a command in a new PTY session, replacing any session with the same id.
    pub async fn start_session(
        &self,
        session_id: &str,
        config: &PtyConfig<'_>,
    ) -> Result<ProcessInfo> {
        let previous = self.sessions.lock().await.remove(session_id);
        if let Some(previous) = previous {
            tokio::task::spawn_blocking(move || drop(previous)).await?;
        }

        let session = Arc::new(TuiSession::spawn(session_id, config)?);
        let info = session.info();

        let replaced = self
            .sessions
            .lock()
            .await
            .insert(session_id.to_string(), session);
        if let Some(replaced) = replaced {
            tokio::task::spawn_blocking(move || drop(replaced)).await?;
        }

        Ok(info)
    }

    /// Starts the default session.
    pub async fn start_app(&self, config: &PtyConfig<'_>) -> Result<ProcessInfo> {
        self.start_session(DEFAULT_SESSION_ID, config).await
    }

    /// Sends keystrokes with symbolic tokens (e.g. `<ENTER>`, `<UP>`) to the target session.
    pub async fn send_input_session(&self, session_id: &str, keys: &str) -> Result<usize> {
        let session = self.get_session(session_id).await?;
        session.send_input(keys)
    }

    /// Sends input to the default session.
    pub async fn send_input(&self, keys: &str) -> Result<usize> {
        self.send_input_session(DEFAULT_SESSION_ID, keys).await
    }

    /// Sends text wrapped in bracketed paste markers to the target session.
    pub async fn send_paste_session(&self, session_id: &str, text: &str) -> Result<usize> {
        let session = self.get_session(session_id).await?;
        session.send_paste(text)
    }

    /// Sends a bracketed paste to the default session.
    pub async fn send_paste(&self, text: &str) -> Result<usize> {
        self.send_paste_session(DEFAULT_SESSION_ID, text).await
    }

    /// Waits for the expectation's pattern and returns the matched text.
    pub async fn expect_session(
        &self,
        session_id: &str,
        expectation: &Expectation,
    ) -> Result<ExpectMatch> {
        let session = self.get_session(session_id).await?;
        session.expect(expectation).await
    }

    /// Sends a signal to the target session's process without ending the session.
    pub async fn signal_session(
        &self,
        session_id: &str,
        signal: Signal,
        target: SignalTarget,
    ) -> Result<SignalDelivery> {
        let session = self.get_session(session_id).await?;
        session.send_signal(signal, target)
    }

    /// Sends a signal to the default session's foreground process group.
    pub async fn signal(&self, signal: Signal) -> Result<SignalDelivery> {
        self.signal_session(DEFAULT_SESSION_ID, signal, SignalTarget::Foreground)
            .await
    }

    /// Waits for an expectation in the default session.
    pub async fn expect(&self, expectation: &Expectation) -> Result<ExpectMatch> {
        self.expect_session(DEFAULT_SESSION_ID, expectation).await
    }

    /// Waits until none of `patterns` is on the session's screen; returns how long it took.
    pub async fn wait_gone_session(
        &self,
        session_id: &str,
        patterns: &[Pattern],
        timeout: Duration,
    ) -> Result<Duration> {
        let session = self.get_session(session_id).await?;
        session.wait_gone(patterns, timeout).await
    }

    /// Waits until none of `patterns` is on the default session's screen.
    pub async fn wait_gone(&self, patterns: &[Pattern], timeout: Duration) -> Result<Duration> {
        self.wait_gone_session(DEFAULT_SESSION_ID, patterns, timeout)
            .await
    }

    /// Waits for the session's process to exit and returns its status and unread output.
    pub async fn wait_exit_session(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<ProcessExit> {
        let session = self.get_session(session_id).await?;
        session.wait_exit(timeout).await
    }

    /// Waits for the default session's process to exit.
    pub async fn wait_exit(&self, timeout: Duration) -> Result<ProcessExit> {
        self.wait_exit_session(DEFAULT_SESSION_ID, timeout).await
    }

    /// Waits until the session produces no output for `quiet_period`.
    pub async fn wait_stable_session(
        &self,
        session_id: &str,
        quiet_period: Duration,
        timeout: Duration,
    ) -> Result<()> {
        let session = self.get_session(session_id).await?;
        session.wait_stable(quiet_period, timeout).await
    }

    /// Waits until the default session produces no output for `quiet_period`.
    pub async fn wait_stable(&self, quiet_period: Duration, timeout: Duration) -> Result<()> {
        self.wait_stable_session(DEFAULT_SESSION_ID, quiet_period, timeout)
            .await
    }

    /// Runs shell commands in the target session, waiting for the prompt after each.
    pub async fn run_script_session(
        &self,
        session_id: &str,
        script: &Script<'_>,
    ) -> Result<ScriptOutcome> {
        let session = self.get_session(session_id).await?;
        session.run_script(script).await
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
        let session = self.get_session(session_id).await?;
        session.resize(rows, cols)
    }

    /// Resizes the default session.
    pub async fn resize(&self, rows: u16, cols: u16) -> Result<(u16, u16)> {
        self.resize_session(DEFAULT_SESSION_ID, rows, cols).await
    }

    /// Reads the current screen of the target session formatted with semantic tags.
    pub async fn read_screen_session(&self, session_id: &str) -> Result<String> {
        let session = self.get_session(session_id).await?;
        session.read_screen()
    }

    /// Reads the default session screen.
    pub async fn read_screen(&self) -> Result<String> {
        self.read_screen_session(DEFAULT_SESSION_ID).await
    }

    /// Takes a detached screen snapshot of the target session.
    pub async fn snapshot_session(&self, session_id: &str) -> Result<Screen> {
        let session = self.get_session(session_id).await?;
        session.snapshot()
    }

    /// Takes a detached screen snapshot of the default session.
    pub async fn snapshot(&self) -> Result<Screen> {
        self.snapshot_session(DEFAULT_SESSION_ID).await
    }

    /// Subscribes to the target session's report events (sent whether or not it writes a report
    /// file). Events from before the call aren't replayed.
    pub async fn subscribe_report_session(
        &self,
        session_id: &str,
    ) -> Result<broadcast::Receiver<ReportEvent>> {
        let session = self.get_session(session_id).await?;
        Ok(session.subscribe_report())
    }

    /// Adds a screenshot taken of the target session to its report.
    pub async fn record_screenshot_session(
        &self,
        session_id: &str,
        shot: &ScreenshotTaken<'_>,
    ) -> Result<()> {
        let session = self.get_session(session_id).await?;
        session.record_screenshot(shot);
        Ok(())
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
        let mut summaries: Vec<SessionSummary> = sessions.values().map(|s| s.summary()).collect();
        drop(sessions);
        summaries.sort_by(|a, b| a.id.cmp(&b.id));
        summaries
    }

    /// Stops the target session, killing its process group and reaping the child. Its report
    /// gets its exit and summary entries.
    pub async fn stop_session(&self, session_id: &str) -> Result<StoppedSession> {
        let session = self
            .sessions
            .lock()
            .await
            .remove(session_id)
            .with_context(|| no_session(session_id))?;
        let stopped = StoppedSession {
            info: session.info(),
            report_path: session.report_path(),
            totals: session.report_totals(),
        };
        tokio::task::spawn_blocking(move || drop(session)).await?;
        Ok(stopped)
    }

    /// Stops the default session.
    pub async fn stop_app(&self) -> Result<StoppedSession> {
        self.stop_session(DEFAULT_SESSION_ID).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
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

        let stopped = mgr.stop_app().await.unwrap();
        assert_eq!(stopped.info.command, "cat");
        assert_eq!(stopped.report_path, None);
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
