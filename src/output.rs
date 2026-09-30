//! Raw output stream shared between a session's reader thread and the tools that wait on it.
//!
//! The reader thread pushes every chunk it reads from the PTY. Waiting tools (`tui_expect`,
//! `tui_wait_stable`, `tui_run_script`) never read the PTY themselves: they subscribe to a
//! revision counter that is bumped on every chunk and re-check their condition when it changes.
//!
//! Stream matching starts at a *read position*. A successful stream expect moves it past the
//! match, and `tui_read` moves it to the end, so an expect never matches output the agent has
//! already seen.

use std::ops::Range;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use alacritty_terminal::vte::{Parser, Perform};
use regex::bytes::Regex;
use tokio::sync::watch;
use tokio::time::Instant;

/// Maximum raw output kept for stream matching; older bytes are dropped.
const MAX_BUFFER_BYTES: usize = 1 << 20;

/// Why a wait ended without its condition being met.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WaitError {
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("the process exited")]
    Eof,
}

/// A literal string or regular expression to wait for.
#[derive(Debug, Clone)]
pub struct Pattern {
    source: String,
    regex: Regex,
}

impl Pattern {
    /// Matches `text` verbatim. Fails only if `text` is too large to compile.
    pub fn literal(text: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            source: text.to_string(),
            regex: Regex::new(&regex::escape(text))?,
        })
    }

    /// Compiles `pattern` as a regular expression.
    pub fn regex(pattern: &str) -> Result<Self, regex::Error> {
        Ok(Self {
            source: pattern.to_string(),
            regex: Regex::new(pattern)?,
        })
    }

    /// Builds a literal or regex pattern, as selected by a tool's `is_regex` flag.
    pub fn new(pattern: &str, is_regex: bool) -> Result<Self, regex::Error> {
        if is_regex {
            Self::regex(pattern)
        } else {
            Self::literal(pattern)
        }
    }

    /// The pattern as the caller wrote it.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the first match in `text`, if any.
    #[must_use]
    pub fn find_in(&self, text: &str) -> Option<String> {
        self.regex
            .find(text.as_bytes())
            .map(|m| String::from_utf8_lossy(m.as_bytes()).into_owned())
    }
}

/// Finds the pattern whose first match starts earliest in `haystack`; on a tie, the one listed
/// first. Returns its index and the byte range of the match.
#[must_use]
pub fn first_match(patterns: &[Pattern], haystack: &[u8]) -> Option<(usize, Range<usize>)> {
    patterns
        .iter()
        .enumerate()
        .filter_map(|(index, pattern)| pattern.regex.find(haystack).map(|m| (index, m.range())))
        .min_by_key(|(index, range)| (range.start, *index))
}

/// Describes patterns for messages: `'a'`, or `any of 'a', 'b'`.
#[must_use]
pub fn describe_patterns(patterns: &[Pattern]) -> String {
    let quoted: Vec<String> = patterns
        .iter()
        .map(|pattern| format!("'{}'", pattern.source()))
        .collect();
    match quoted.as_slice() {
        [single] => single.clone(),
        _ => format!("any of {}", quoted.join(", ")),
    }
}

/// A stream match: the matched text and the unread output before it, both as plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMatch {
    /// Which of the patterns matched (always 0 for a single pattern).
    pub index: usize,
    pub matched: String,
    pub before: String,
}

struct OutputState {
    buffer: Vec<u8>,
    /// Absolute stream offset of `buffer[0]`.
    base: usize,
    /// Absolute stream offset up to which output has been consumed.
    read_pos: usize,
    eof: bool,
}

impl OutputState {
    const fn end(&self) -> usize {
        self.base + self.buffer.len()
    }

    fn unread(&self) -> &[u8] {
        &self.buffer[self.read_pos - self.base..]
    }
}

/// Output of one session, written by its reader thread.
pub struct SessionOutput {
    state: Mutex<OutputState>,
    revision: watch::Sender<u64>,
}

impl Default for SessionOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionOutput {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(OutputState {
                buffer: Vec::new(),
                base: 0,
                read_pos: 0,
                eof: false,
            }),
            revision: watch::Sender::new(0),
        }
    }

    fn state(&self) -> MutexGuard<'_, OutputState> {
        // The state stays consistent even if a holder panicked, so recover from poisoning
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn bump(&self) {
        self.revision.send_modify(|rev| *rev = rev.wrapping_add(1));
    }

    /// Appends a chunk read from the PTY and wakes waiters.
    pub fn push(&self, chunk: &[u8]) {
        {
            let mut state = self.state();
            state.buffer.extend_from_slice(chunk);
            let excess = state.buffer.len().saturating_sub(MAX_BUFFER_BYTES);
            if excess > 0 {
                state.buffer.drain(..excess);
                state.base += excess;
                state.read_pos = state.read_pos.max(state.base);
            }
        }
        self.bump();
    }

    /// Wakes waiters when the screen changed without new output, e.g. when a synchronized
    /// update is flushed on timeout.
    pub fn notify_screen_changed(&self) {
        self.bump();
    }

    /// Marks the stream as ended (the process closed the PTY) and wakes waiters.
    pub fn close(&self) {
        self.state().eof = true;
        self.bump();
    }

    #[must_use]
    pub fn is_eof(&self) -> bool {
        self.state().eof
    }

    /// Marks everything received so far as consumed.
    pub fn mark_all_read(&self) {
        let mut state = self.state();
        state.read_pos = state.end();
    }

    /// Returns the last `max_chars` characters of unread output as plain text.
    #[must_use]
    pub fn unread_tail(&self, max_chars: usize) -> String {
        let text = plain_text(self.state().unread());
        let skip = text.chars().count().saturating_sub(max_chars);
        text.chars().skip(skip).collect()
    }

    /// Re-evaluates `check` whenever new output arrives, until it returns `Some`, the process
    /// exits, or `timeout` elapses. `check` runs once more after the last chunk before EOF.
    pub async fn wait_for<T>(
        &self,
        timeout: Duration,
        mut check: impl FnMut() -> Option<T>,
    ) -> Result<T, WaitError> {
        let deadline = Instant::now() + timeout;
        let mut revisions = self.revision.subscribe();
        loop {
            // Mark the current revision as seen before checking, so nothing pushed after the
            // check can be missed
            revisions.borrow_and_update();
            if let Some(value) = check() {
                return Ok(value);
            }
            if self.is_eof() {
                return Err(WaitError::Eof);
            }
            match tokio::time::timeout_at(deadline, revisions.changed()).await {
                Ok(Ok(())) => {}
                // The sender lives in `self`, so it can't be dropped while we borrow it
                Ok(Err(_)) => return Err(WaitError::Eof),
                Err(_) => return Err(WaitError::Timeout(timeout)),
            }
        }
    }

    /// Waits for `pattern` in the unread output and consumes up to the end of the match.
    pub async fn expect(
        &self,
        pattern: &Pattern,
        timeout: Duration,
    ) -> Result<StreamMatch, WaitError> {
        self.expect_any(std::slice::from_ref(pattern), timeout)
            .await
    }

    /// Waits for whichever of `patterns` matches earliest in the unread output (on a tie, the
    /// one listed first) and consumes up to the end of that match.
    pub async fn expect_any(
        &self,
        patterns: &[Pattern],
        timeout: Duration,
    ) -> Result<StreamMatch, WaitError> {
        self.wait_for(timeout, || {
            let mut state = self.state();
            let unread = state.unread();
            let (index, range) = first_match(patterns, unread)?;
            let result = StreamMatch {
                index,
                matched: plain_text(&unread[range.clone()]),
                before: plain_text(&unread[..range.start]),
            };
            state.read_pos += range.end;
            drop(state);
            Some(result)
        })
        .await
    }

    /// Waits until no output has arrived for `quiet_period`. Returns immediately once the
    /// process has exited, since its output can no longer change.
    pub async fn wait_stable(
        &self,
        quiet_period: Duration,
        max_wait: Duration,
    ) -> Result<(), WaitError> {
        let deadline = Instant::now() + max_wait;
        let mut revisions = self.revision.subscribe();
        revisions.borrow_and_update();
        loop {
            if self.is_eof() {
                return Ok(());
            }
            let quiet_until = Instant::now() + quiet_period;
            if quiet_until > deadline {
                return Err(WaitError::Timeout(max_wait));
            }
            match tokio::time::timeout_at(quiet_until, revisions.changed()).await {
                // New output: restart the quiet period
                Ok(Ok(())) => {
                    revisions.borrow_and_update();
                }
                Ok(Err(_)) | Err(_) => return Ok(()),
            }
        }
    }
}

/// Converts raw terminal output to plain text: escape sequences and carriage returns are
/// dropped, printable characters, newlines and tabs are kept.
#[must_use]
pub fn plain_text(bytes: &[u8]) -> String {
    struct Collector(String);

    impl Perform for Collector {
        fn print(&mut self, c: char) {
            self.0.push(c);
        }

        fn execute(&mut self, byte: u8) {
            if matches!(byte, b'\n' | b'\t') {
                self.0.push(char::from(byte));
            }
        }
    }

    let mut collector = Collector(String::new());
    Parser::new().advance(&mut collector, bytes);
    collector.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(s: &str) -> Pattern {
        Pattern::literal(s).unwrap()
    }

    #[test]
    fn test_plain_text_strips_escapes() {
        assert_eq!(
            plain_text(b"\x1b[31mred\x1b[0m\r\nnext\x1b]0;title\x07"),
            "red\nnext"
        );
    }

    #[tokio::test]
    async fn test_expect_consumes_match() {
        let output = SessionOutput::new();
        output.push(b"one PROMPT two PROMPT");

        let first = output
            .expect(&literal("PROMPT"), Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(first.before, "one ");
        let second = output
            .expect(&literal("PROMPT"), Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(second.before, " two ");
        assert_eq!(
            output
                .expect(&literal("PROMPT"), Duration::from_millis(10))
                .await,
            Err(WaitError::Timeout(Duration::from_millis(10)))
        );
    }

    #[tokio::test]
    async fn test_expect_any_takes_earliest_match() {
        let output = SessionOutput::new();
        output.push(b"login: Permission denied\nPassword: ");
        let patterns = [literal("Password:"), literal("Permission denied")];

        let found = output
            .expect_any(&patterns, Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(found.index, 1);
        assert_eq!(found.matched, "Permission denied");
        assert_eq!(found.before, "login: ");

        // Only output up to the winning match was consumed
        let next = output
            .expect_any(&patterns, Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(next.index, 0);
    }

    #[tokio::test]
    async fn test_expect_any_tie_goes_to_first_listed() {
        let output = SessionOutput::new();
        output.push(b"ERROR: disk full");
        let patterns = [Pattern::regex("ERR[A-Z]*").unwrap(), literal("ERROR: disk")];
        let found = output
            .expect_any(&patterns, Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(found.index, 0);
        assert_eq!(found.matched, "ERROR");
    }

    #[test]
    fn test_describe_patterns() {
        assert_eq!(describe_patterns(&[literal("a")]), "'a'");
        assert_eq!(
            describe_patterns(&[literal("a"), literal("b")]),
            "any of 'a', 'b'"
        );
    }

    #[tokio::test]
    async fn test_mark_all_read_hides_seen_output() {
        let output = SessionOutput::new();
        output.push(b"MARKER");
        output.mark_all_read();
        assert!(
            output
                .expect(&literal("MARKER"), Duration::from_millis(10))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_expect_wakes_on_push() {
        let output = std::sync::Arc::new(SessionOutput::new());
        let writer = std::sync::Arc::clone(&output);
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            writer.push(b"late ");
            writer.push(b"ARRIVAL");
        });
        let found = output
            .expect(&literal("ARRIVAL"), Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(found.before, "late ");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_expect_fails_fast_on_eof() {
        let output = SessionOutput::new();
        output.push(b"bye");
        output.close();
        let started = Instant::now();
        assert_eq!(
            output
                .expect(&literal("never"), Duration::from_secs(5))
                .await,
            Err(WaitError::Eof)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn test_buffer_is_bounded() {
        let output = SessionOutput::new();
        output.push(&vec![b'x'; MAX_BUFFER_BYTES]);
        output.push(b"TAIL");
        let state = output.state();
        assert_eq!(state.buffer.len(), MAX_BUFFER_BYTES);
        assert!(state.buffer.ends_with(b"TAIL"));
        assert_eq!(state.read_pos, state.base);
    }

    #[tokio::test]
    async fn test_wait_stable() {
        let output = SessionOutput::new();
        output
            .wait_stable(Duration::from_millis(20), Duration::from_secs(1))
            .await
            .unwrap();

        let output = std::sync::Arc::new(output);
        let writer = std::sync::Arc::clone(&output);
        let task = tokio::spawn(async move {
            for _ in 0..20 {
                writer.push(b"tick");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        assert_eq!(
            output
                .wait_stable(Duration::from_millis(100), Duration::from_millis(150))
                .await,
            Err(WaitError::Timeout(Duration::from_millis(150)))
        );
        task.await.unwrap();
    }
}
