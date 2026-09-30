//! Session report: what a session did and what was checked, as JSON Lines.
//!
//! Every session produces [`ReportEvent`]s (inputs, checks, screenshots, exit, summary) and
//! broadcasts them to subscribers such as live viewers. When `tui_start` is given a `report_path`,
//! they are also written to that file, one JSON object per line, flushed after each line so a
//! crash still leaves a valid report.
//!
//! A subscriber first gets the events so far (the `start` entry and the latest
//! [`MAX_REPORT_HISTORY`]), then every new one, so a live viewer opened mid-session still sees
//! the whole story.
//!
//! A *check* is an entry with a boolean `passed` field (`expect`, `wait_gone`, `wait_stable`,
//! `wait_exit`, `run_script`). The `summary` entry counts them; it is written once, when the
//! session ends, after the `exit` entry.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::output::{Pattern, Syntax};
use crate::session::{
    ExitStatus, ExpectMatch, ExpectTarget, Expectation, ProcessExit, PtyConfig, Script,
    ScriptOutcome, SignalDelivery, SignalTarget,
};

/// Version of the report format, written in the `start` entry.
pub const REPORT_VERSION: u32 = 1;

/// How many events a slow subscriber can fall behind before it misses some.
const REPORT_CHANNEL_CAPACITY: usize = 256;

/// How many past events (besides `start`) a new subscriber is sent. A failed check's error can
/// hold a whole screen, so the history is bounded.
pub const MAX_REPORT_HISTORY: usize = 1000;

/// One line of the report: when it was written and what happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportEvent {
    /// Milliseconds since the session started, when the entry was written. For a check, that's
    /// when it finished (it started `elapsed_ms` earlier).
    pub at_ms: u64,
    #[serde(flatten)]
    pub entry: ReportEntry,
}

/// What a report line describes. Serialized with its kind in `type` (e.g. `"wait_gone"`).
///
/// Optional fields that don't apply are written as `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReportEntry {
    /// The session started.
    Start {
        session_id: String,
        command: String,
        args: Vec<String>,
        rows: u16,
        cols: u16,
        pid: Option<u32>,
        record_path: Option<String>,
        /// Unix time in seconds.
        timestamp: u64,
        version: u32,
    },
    /// Keys sent with `tui_input`, as given (e.g. `"ls<ENTER>"`), and how many bytes they became.
    Input { keys: String, bytes: usize },
    /// Text sent with `tui_paste`.
    Paste { text: String, bytes: usize },
    /// The terminal was resized.
    Resize { rows: u16, cols: u16 },
    /// A signal sent with `tui_signal`.
    Signal {
        /// e.g. `"SIGINT"`.
        signal: String,
        /// `"foreground"` or `"process"`.
        target: SignalTarget,
        /// The process group id (`foreground`) or process id (`process`) it was sent to.
        id: i32,
    },
    /// A `tui_expect` check.
    Expect {
        target: ExpectTarget,
        syntax: Syntax,
        /// The patterns as written.
        patterns: Vec<String>,
        timeout_ms: u64,
        passed: bool,
        elapsed_ms: u64,
        /// Which pattern matched, from 0.
        pattern_index: Option<usize>,
        matched: Option<String>,
        /// Screen mode: the row of the match, from 1.
        row: Option<usize>,
        error: Option<String>,
    },
    /// A `tui_wait_gone` check.
    WaitGone {
        syntax: Syntax,
        patterns: Vec<String>,
        timeout_ms: u64,
        passed: bool,
        elapsed_ms: u64,
        error: Option<String>,
    },
    /// A `tui_wait_stable` check.
    WaitStable {
        quiet_period_ms: u64,
        timeout_ms: u64,
        passed: bool,
        elapsed_ms: u64,
        error: Option<String>,
    },
    /// A `tui_wait_exit` check. Passes when the process exits, whatever its exit code.
    WaitExit {
        timeout_ms: u64,
        passed: bool,
        elapsed_ms: u64,
        exit_status: Option<ExitStatus>,
        error: Option<String>,
    },
    /// A `tui_run_script` check. Fails if any command's prompt didn't appear.
    RunScript {
        commands: Vec<String>,
        prompt: String,
        syntax: Syntax,
        /// Per command.
        timeout_ms: u64,
        passed: bool,
        elapsed_ms: u64,
        /// How many commands finished.
        completed: usize,
        error: Option<String>,
    },
    /// A screenshot was taken.
    Screenshot {
        /// `"png"` or `"svg"`.
        format: String,
        /// `None` when it was returned inline.
        path: Option<String>,
        bytes: usize,
    },
    /// The process exited. Written once, after its last output.
    Exit { exit_status: ExitStatus },
    /// Written once, last, when the session ends.
    Summary {
        checks: u32,
        passed: u32,
        failed: u32,
        /// `None` if the exit status was never reported.
        exit_status: Option<ExitStatus>,
        duration_ms: u64,
    },
}

impl ReportEntry {
    /// For a check, whether it passed; `None` for other entries.
    #[must_use]
    pub const fn check_passed(&self) -> Option<bool> {
        match self {
            Self::Expect { passed, .. }
            | Self::WaitGone { passed, .. }
            | Self::WaitStable { passed, .. }
            | Self::WaitExit { passed, .. }
            | Self::RunScript { passed, .. } => Some(*passed),
            _ => None,
        }
    }
}

/// How many checks ran, passed and failed so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ReportTotals {
    pub checks: u32,
    pub passed: u32,
    pub failed: u32,
}

impl ReportTotals {
    const fn count(&mut self, passed: bool) {
        self.checks = self.checks.saturating_add(1);
        if passed {
            self.passed = self.passed.saturating_add(1);
        } else {
            self.failed = self.failed.saturating_add(1);
        }
    }
}

impl std::fmt::Display for ReportTotals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let plural = if self.checks == 1 { "" } else { "s" };
        write!(
            f,
            "{} check{plural}, {} passed, {} failed",
            self.checks, self.passed, self.failed
        )
    }
}

/// The events a session has produced so far, and the channel for the ones still to come.
#[derive(Debug)]
pub struct ReportSubscription {
    /// Past events, oldest first: the `start` entry, then the latest [`MAX_REPORT_HISTORY`].
    pub history: Vec<ReportEvent>,
    /// How many events between `start` and the history were dropped to bound it.
    pub dropped: usize,
    /// Checks so far, counting dropped ones too.
    pub totals: ReportTotals,
    /// Every event after `history`, with none missed or repeated in between.
    pub live: broadcast::Receiver<ReportEvent>,
}

/// How a check ended and how long it took.
#[derive(Debug, Clone, Copy)]
pub struct Outcome<'a, T> {
    pub result: &'a Result<T>,
    pub elapsed: Duration,
}

impl<'a, T> Outcome<'a, T> {
    #[must_use]
    pub const fn new(result: &'a Result<T>, elapsed: Duration) -> Self {
        Self { result, elapsed }
    }

    fn error(&self) -> Option<String> {
        self.result.as_ref().err().map(|e| format!("{e:#}"))
    }
}

/// A screenshot that was rendered (and written, if it has a path).
#[derive(Debug, Clone, Copy)]
pub struct ScreenshotTaken<'a> {
    /// `"png"` or `"svg"`.
    pub format: &'a str,
    pub path: Option<&'a str>,
    pub bytes: usize,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn sources(patterns: &[Pattern]) -> Vec<String> {
    patterns.iter().map(|p| p.source().to_string()).collect()
}

/// The syntax of a pattern list (all patterns of one call share it).
fn syntax_of(patterns: &[Pattern]) -> Syntax {
    patterns.first().map(Pattern::syntax).unwrap_or_default()
}

struct ReportState {
    /// The report file, if the session writes one.
    file: Option<File>,
    totals: ReportTotals,
    /// The `start` event, kept for subscribers whatever the history drops.
    start: Option<ReportEvent>,
    /// The latest other events, for subscribers.
    history: VecDeque<ReportEvent>,
    /// Events dropped from `history`.
    dropped: usize,
    /// The status in the `exit` entry, once written.
    exit_status: Option<ExitStatus>,
    exit_written: bool,
    finished: bool,
}

/// A session's report: produces events, broadcasts them, and writes them to the report file.
pub struct SessionReport {
    started: Instant,
    path: Option<String>,
    events: broadcast::Sender<ReportEvent>,
    state: Mutex<ReportState>,
}

impl SessionReport {
    /// Creates the report of a session that started at `started`. With a `path`, creates (or
    /// truncates) that file.
    pub fn create(path: Option<&str>, started: Instant) -> Result<Self> {
        let file = path
            .map(|path| {
                File::create(path)
                    .with_context(|| format!("failed to create report file at '{path}'"))
            })
            .transpose()?;
        let (events, _) = broadcast::channel(REPORT_CHANNEL_CAPACITY);
        Ok(Self {
            started,
            path: path.map(str::to_string),
            events,
            state: Mutex::new(ReportState {
                file,
                totals: ReportTotals::default(),
                start: None,
                history: VecDeque::new(),
                dropped: 0,
                exit_status: None,
                exit_written: false,
                finished: false,
            }),
        })
    }

    fn state(&self) -> MutexGuard<'_, ReportState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The report file, if the session writes one.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Whether anyone reads the events: a report file or a subscriber.
    #[must_use]
    pub fn is_observed(&self) -> bool {
        self.path.is_some() || self.events.receiver_count() > 0
    }

    /// Returns the events so far and a receiver for the rest. Events are written and
    /// broadcast under the state lock held here, so none falls between the two.
    #[must_use]
    pub fn subscribe(&self) -> ReportSubscription {
        let state = self.state();
        let subscription = ReportSubscription {
            history: state
                .start
                .iter()
                .chain(state.history.iter())
                .cloned()
                .collect(),
            dropped: state.dropped,
            totals: state.totals,
            live: self.events.subscribe(),
        };
        drop(state);
        subscription
    }

    /// Checks run so far.
    #[must_use]
    pub fn totals(&self) -> ReportTotals {
        self.state().totals
    }

    /// Adds an entry, unless the report is finished.
    pub fn record(&self, entry: ReportEntry) {
        let mut state = self.state();
        if !state.finished {
            self.write_entry(&mut state, entry);
        }
        drop(state);
    }

    /// Stamps, counts, writes and broadcasts one entry. Runs under the state lock, so entries
    /// keep their order in the file and for subscribers.
    fn write_entry(&self, state: &mut ReportState, entry: ReportEntry) {
        if let Some(passed) = entry.check_passed() {
            state.totals.count(passed);
        }
        let event = ReportEvent {
            at_ms: millis(self.started.elapsed()),
            entry,
        };
        if let Some(file) = state.file.as_mut() {
            let written = serde_json::to_vec(&event)
                .map_err(anyhow::Error::from)
                .and_then(|mut line| {
                    line.push(b'\n');
                    file.write_all(&line)?;
                    file.flush()?;
                    Ok(())
                });
            if let Err(e) = written {
                tracing::warn!("stopped writing the session report: {e:#}");
                state.file = None;
            }
        }
        if matches!(event.entry, ReportEntry::Start { .. }) {
            state.start = Some(event.clone());
        } else {
            if state.history.len() == MAX_REPORT_HISTORY {
                state.history.pop_front();
                state.dropped += 1;
            }
            state.history.push_back(event.clone());
        }
        // No subscribers is fine; a slow one misses old events instead of blocking the writer
        let _ = self.events.send(event);
    }

    /// Records the session's start. `pid` is the spawned child's.
    pub fn record_start(&self, session_id: &str, config: &PtyConfig<'_>, pid: Option<u32>) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.record(ReportEntry::Start {
            session_id: session_id.to_string(),
            command: config.command.to_string(),
            args: config.args.to_vec(),
            rows: config.rows,
            cols: config.cols,
            pid,
            record_path: config.record_path.map(str::to_string),
            timestamp,
            version: REPORT_VERSION,
        });
    }

    pub fn record_input(&self, keys: &str, bytes: usize) {
        self.record(ReportEntry::Input {
            keys: keys.to_string(),
            bytes,
        });
    }

    pub fn record_paste(&self, text: &str, bytes: usize) {
        self.record(ReportEntry::Paste {
            text: text.to_string(),
            bytes,
        });
    }

    pub fn record_resize(&self, rows: u16, cols: u16) {
        self.record(ReportEntry::Resize { rows, cols });
    }

    pub fn record_signal(&self, name: &str, delivery: SignalDelivery) {
        self.record(ReportEntry::Signal {
            signal: name.to_string(),
            target: delivery.target,
            id: delivery.id,
        });
    }

    pub fn record_expect(&self, expectation: &Expectation, outcome: &Outcome<'_, ExpectMatch>) {
        let found = outcome.result.as_ref().ok();
        self.record(ReportEntry::Expect {
            target: expectation.target,
            syntax: syntax_of(&expectation.patterns),
            patterns: sources(&expectation.patterns),
            timeout_ms: millis(expectation.timeout),
            passed: found.is_some(),
            elapsed_ms: millis(outcome.elapsed),
            pattern_index: found.map(|found| found.index),
            matched: found.map(|found| found.matched.clone()),
            row: found.and_then(|found| found.line.as_ref().map(|line| line.row + 1)),
            error: outcome.error(),
        });
    }

    pub fn record_wait_gone(
        &self,
        patterns: &[Pattern],
        timeout: Duration,
        outcome: &Outcome<'_, Duration>,
    ) {
        self.record(ReportEntry::WaitGone {
            syntax: syntax_of(patterns),
            patterns: sources(patterns),
            timeout_ms: millis(timeout),
            passed: outcome.result.is_ok(),
            elapsed_ms: millis(outcome.elapsed),
            error: outcome.error(),
        });
    }

    pub fn record_wait_stable(
        &self,
        quiet_period: Duration,
        timeout: Duration,
        outcome: &Outcome<'_, ()>,
    ) {
        self.record(ReportEntry::WaitStable {
            quiet_period_ms: millis(quiet_period),
            timeout_ms: millis(timeout),
            passed: outcome.result.is_ok(),
            elapsed_ms: millis(outcome.elapsed),
            error: outcome.error(),
        });
    }

    pub fn record_wait_exit(&self, timeout: Duration, outcome: &Outcome<'_, ProcessExit>) {
        self.record(ReportEntry::WaitExit {
            timeout_ms: millis(timeout),
            passed: outcome.result.is_ok(),
            elapsed_ms: millis(outcome.elapsed),
            exit_status: outcome.result.as_ref().ok().map(|exit| exit.status),
            error: outcome.error(),
        });
    }

    /// Records a script run. A script that stopped early (`ScriptOutcome::error`) failed.
    pub fn record_run_script(&self, script: &Script<'_>, outcome: &Outcome<'_, ScriptOutcome>) {
        let (completed, error) = match outcome.result {
            Ok(run) => (run.steps.len(), run.error.clone()),
            Err(e) => (0, Some(format!("{e:#}"))),
        };
        self.record(ReportEntry::RunScript {
            commands: script.commands.to_vec(),
            prompt: script.prompt.source().to_string(),
            syntax: script.prompt.syntax(),
            timeout_ms: millis(script.timeout_per_command),
            passed: error.is_none(),
            elapsed_ms: millis(outcome.elapsed),
            completed,
            error,
        });
    }

    pub fn record_screenshot(&self, shot: &ScreenshotTaken<'_>) {
        self.record(ReportEntry::Screenshot {
            format: shot.format.to_string(),
            path: shot.path.map(str::to_string),
            bytes: shot.bytes,
        });
    }

    /// Records the process's exit. Only the first call writes an entry, and none is written
    /// once the report is finished.
    pub fn record_exit(&self, status: ExitStatus) {
        let mut state = self.state();
        if !state.finished {
            self.write_exit(&mut state, status);
        }
        drop(state);
    }

    fn write_exit(&self, state: &mut ReportState, status: ExitStatus) {
        if state.exit_written {
            return;
        }
        state.exit_written = true;
        state.exit_status = Some(status);
        self.write_entry(
            state,
            ReportEntry::Exit {
                exit_status: status,
            },
        );
    }

    /// Ends the report: writes the `exit` entry if it isn't there yet (and `status` is known),
    /// then the summary, and closes the file. Later calls, and later entries, are ignored.
    pub fn finish(&self, status: Option<ExitStatus>) {
        let mut state = self.state();
        if state.finished {
            drop(state);
            return;
        }
        if let Some(status) = status {
            self.write_exit(&mut state, status);
        }
        let totals = state.totals;
        let exit_status = state.exit_status;
        self.write_entry(
            &mut state,
            ReportEntry::Summary {
                checks: totals.checks,
                passed: totals.passed,
                failed: totals.failed,
                exit_status,
                duration_ms: millis(self.started.elapsed()),
            },
        );
        state.finished = true;
        state.file = None;
        drop(state);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::session::ScreenLine;
    use serde_json::json;

    fn to_json(entry: ReportEntry) -> serde_json::Value {
        serde_json::to_value(ReportEvent { at_ms: 42, entry }).unwrap()
    }

    #[test]
    fn test_event_is_flat_with_type_and_at_ms() {
        assert_eq!(
            to_json(ReportEntry::Input {
                keys: "ls<ENTER>".to_string(),
                bytes: 3,
            }),
            json!({ "type": "input", "at_ms": 42, "keys": "ls<ENTER>", "bytes": 3 })
        );
        assert_eq!(
            to_json(ReportEntry::Signal {
                signal: "SIGTERM".to_string(),
                target: SignalTarget::Foreground,
                id: 4321,
            }),
            json!({
                "type": "signal", "at_ms": 42, "signal": "SIGTERM", "target": "foreground",
                "id": 4321
            })
        );
        assert_eq!(
            to_json(ReportEntry::Exit {
                exit_status: ExitStatus::Signal(9),
            }),
            json!({ "type": "exit", "at_ms": 42, "exit_status": { "signal": 9 } })
        );
        assert_eq!(
            to_json(ReportEntry::Summary {
                checks: 2,
                passed: 1,
                failed: 1,
                exit_status: None,
                duration_ms: 900,
            }),
            json!({
                "type": "summary", "at_ms": 42, "checks": 2, "passed": 1, "failed": 1,
                "exit_status": null, "duration_ms": 900
            })
        );
    }

    #[test]
    fn test_expect_entry_from_a_screen_match() {
        let report = SessionReport::create(None, Instant::now()).unwrap();
        let mut events = report.subscribe().live;
        let expectation = Expectation {
            patterns: vec![
                Pattern::glob("Err*").unwrap(),
                Pattern::glob("READY").unwrap(),
            ],
            target: ExpectTarget::Screen,
            timeout: Duration::from_secs(5),
        };
        let result = Ok(ExpectMatch {
            index: 1,
            matched: "READY".to_string(),
            before: String::new(),
            after: String::new(),
            line: Some(ScreenLine {
                row: 2,
                text: "Status: READY".to_string(),
            }),
        });
        report.record_expect(
            &expectation,
            &Outcome::new(&result, Duration::from_millis(120)),
        );

        let mut line = serde_json::to_value(events.try_recv().unwrap()).unwrap();
        line.as_object_mut().unwrap().remove("at_ms");
        assert_eq!(
            line,
            json!({
                "type": "expect", "target": "screen", "syntax": "glob",
                "patterns": ["Err*", "READY"], "timeout_ms": 5000, "passed": true,
                "elapsed_ms": 120, "pattern_index": 1, "matched": "READY", "row": 3,
                "error": null
            })
        );
    }

    #[test]
    fn test_failed_script_counts_as_failed_check() {
        let report = SessionReport::create(None, Instant::now()).unwrap();
        let commands = ["true".to_string(), "false".to_string()];
        let script = Script {
            commands: &commands,
            prompt: Pattern::literal("$ ").unwrap(),
            timeout_per_command: Duration::from_secs(1),
        };
        let stopped = Ok(ScriptOutcome {
            steps: Vec::new(),
            error: Some("command 1 ('true'): prompt not seen".to_string()),
        });
        report.record_run_script(&script, &Outcome::new(&stopped, Duration::ZERO));
        let failed_wait: Result<()> = Err(anyhow::anyhow!("timed out"));
        report.record_wait_stable(
            Duration::from_millis(100),
            Duration::from_secs(1),
            &Outcome::new(&failed_wait, Duration::from_secs(1)),
        );
        report.record_input("x", 1);

        assert_eq!(
            report.totals(),
            ReportTotals {
                checks: 2,
                passed: 0,
                failed: 2,
            }
        );
        assert_eq!(report.totals().to_string(), "2 checks, 0 passed, 2 failed");
    }

    #[test]
    fn test_finish_writes_exit_then_summary_once() {
        let report = SessionReport::create(None, Instant::now()).unwrap();
        let mut events = report.subscribe().live;
        report.finish(Some(ExitStatus::Code(0)));
        report.finish(Some(ExitStatus::Code(1)));
        report.record_exit(ExitStatus::Code(2));
        report.record_input("late", 4);

        let kinds: Vec<ReportEntry> = std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| event.entry)
            .collect();
        assert_eq!(kinds.len(), 2, "{kinds:?}");
        assert_eq!(
            kinds[0],
            ReportEntry::Exit {
                exit_status: ExitStatus::Code(0)
            }
        );
        assert!(matches!(
            kinds[1],
            ReportEntry::Summary {
                exit_status: Some(ExitStatus::Code(0)),
                checks: 0,
                ..
            }
        ));
    }

    #[test]
    fn test_late_subscriber_gets_start_and_recent_history() {
        let report = SessionReport::create(None, Instant::now()).unwrap();
        let args = ["-c".to_string(), "true".to_string()];
        report.record_start("s", &PtyConfig::new("sh", &args, 43, 155), Some(1));
        let failed: Result<()> = Err(anyhow::anyhow!("timed out"));
        report.record_wait_stable(
            Duration::from_millis(100),
            Duration::from_secs(1),
            &Outcome::new(&failed, Duration::from_secs(1)),
        );
        for _ in 0..MAX_REPORT_HISTORY {
            report.record_input("x", 1);
        }

        let mut subscription = report.subscribe();
        assert_eq!(subscription.history.len(), MAX_REPORT_HISTORY + 1);
        assert!(matches!(
            subscription.history[0].entry,
            ReportEntry::Start { .. }
        ));
        assert!(matches!(
            subscription.history[1].entry,
            ReportEntry::Input { .. }
        ));
        // The dropped check still counts
        assert_eq!(subscription.dropped, 1);
        assert_eq!(subscription.totals.failed, 1);

        report.record_resize(10, 20);
        assert_eq!(
            subscription.live.try_recv().unwrap().entry,
            ReportEntry::Resize { rows: 10, cols: 20 }
        );
        assert!(subscription.live.try_recv().is_err());
    }

    #[test]
    fn test_exit_is_written_once() {
        let report = SessionReport::create(None, Instant::now()).unwrap();
        let mut events = report.subscribe().live;
        report.record_exit(ExitStatus::Signal(9));
        report.record_exit(ExitStatus::Signal(9));
        report.finish(None);

        let entries: Vec<ReportEntry> = std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| event.entry)
            .collect();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(matches!(
            entries[1],
            ReportEntry::Summary {
                exit_status: Some(ExitStatus::Signal(9)),
                ..
            }
        ));
    }
}
