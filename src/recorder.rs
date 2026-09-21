//! asciicast v3 session recorder for ShadowPTY.
//!
//! Specification: <https://docs.asciinema.org/manual/asciicast/v3/>
//!
//! Format details:
//! - Line 1: JSON header object (`version: 3`, `term: {cols, rows, type}`, `timestamp`, `command`)
//! - Subsequent lines: JSON array `[interval, event_type, data]` where interval is elapsed seconds
//!   since the previous event (delta).
//! - Event types:
//!   - `"o"`: terminal output
//!   - `"i"`: terminal input
//!   - `"r"`: terminal resize (`"{cols}x{rows}"`)
//!   - `"x"`: exit status (`"{code}"`)

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

#[derive(Serialize)]
struct CastHeaderTerm<'a> {
    cols: u16,
    rows: u16,
    #[serde(rename = "type")]
    term_type: &'a str,
}

#[derive(Serialize)]
struct CastHeader<'a> {
    version: u32,
    term: CastHeaderTerm<'a>,
    timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<&'a str>,
}

/// Asciicast v3 recorder handle.
pub struct AsciicastRecorder {
    writer: BufWriter<File>,
    last_event_time: Instant,
}

impl AsciicastRecorder {
    /// Creates and initializes a new asciicast v3 file with the header.
    pub fn create<P: AsRef<Path>>(
        path: P,
        cols: u16,
        rows: u16,
        command: Option<&str>,
    ) -> Result<Self> {
        let file = File::create(path.as_ref()).with_context(|| {
            format!(
                "failed to create asciicast file at {}",
                path.as_ref().display()
            )
        })?;
        let mut writer = BufWriter::new(file);

        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let header = CastHeader {
            version: 3,
            term: CastHeaderTerm {
                cols,
                rows,
                term_type: "xterm-256color",
            },
            timestamp: now_unix,
            command,
        };

        serde_json::to_writer(&mut writer, &header)
            .context("failed to serialize asciicast header")?;
        writer
            .write_all(b"\n")
            .context("failed to write header newline")?;
        writer.flush().context("failed to flush asciicast header")?;

        Ok(Self {
            writer,
            last_event_time: Instant::now(),
        })
    }

    /// Records an event with relative delta timing since previous event.
    fn record_event(&mut self, event_type: &str, data: &str) -> Result<()> {
        let now = Instant::now();
        let delta = now.duration_since(self.last_event_time).as_secs_f64();
        self.last_event_time = now;

        // Round to 3 decimal places (milliseconds) as per asciinema convention
        let rounded_delta = (delta * 1000.0).round() / 1000.0;

        let event = (rounded_delta, event_type, data);
        serde_json::to_writer(&mut self.writer, &event)
            .context("failed to serialize asciicast event")?;
        self.writer
            .write_all(b"\n")
            .context("failed to write event newline")?;
        self.writer.flush().context("failed to flush event")?;

        Ok(())
    }

    /// Records terminal output bytes (`"o"` event).
    pub fn record_output(&mut self, bytes: &[u8]) -> Result<()> {
        let text = String::from_utf8_lossy(bytes);
        self.record_event("o", &text)
    }

    /// Records terminal input bytes (`"i"` event).
    pub fn record_input(&mut self, bytes: &[u8]) -> Result<()> {
        let text = String::from_utf8_lossy(bytes);
        self.record_event("i", &text)
    }

    /// Records terminal resize (`"r"` event with `"{cols}x{rows}"`).
    pub fn record_resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let data = format!("{cols}x{rows}");
        self.record_event("r", &data)
    }

    /// Records session exit (`"x"` event with exit status string).
    pub fn record_exit(&mut self, exit_code: i32) -> Result<()> {
        let data = exit_code.to_string();
        self.record_event("x", &data)
    }
}

/// Thread-safe shared reference to an optional asciicast recorder.
pub type SharedRecorder = Arc<Mutex<Option<AsciicastRecorder>>>;
