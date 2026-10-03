//! Turns a session's screen into frames for its viewers, and tracks its status.
//!
//! One producer task per live session watches the session's revision counter (bumped by the
//! reader on every chunk and by `notify_screen_changed`) and its exit status. When the screen
//! changed it takes a `Screen` snapshot under the terminal lock and renders it to SVG outside
//! the lock, on a blocking thread, at most `MAX_FPS` times a second. Frames go into a `watch`
//! channel, so a burst of output becomes one frame and a slow viewer simply gets the latest one;
//! nothing ever waits on a viewer.

use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

use super::{LiveEntry, SessionState, unix_ms};
use crate::screenshot::{Theme, render_svg};
use crate::session::{ExitStatus, TuiSession};

/// Most frames per second sent to viewers.
///
/// A frame of a busy 43×155 screen costs about 0.7 ms to capture and render (`cargo bench --
/// render`: `capture` + `screenshot_svg`), so this is about 1% of a core while the app redraws
/// continuously.
pub const MAX_FPS: u64 = 15;

/// Shortest time between two frames while someone is watching.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(1000 / MAX_FPS);

/// Shortest time between two frames while nobody is watching. Frames are still rendered, so a
/// viewer who joins later, or after the session ended, sees a recent screen right away.
pub const IDLE_FRAME_INTERVAL: Duration = Duration::from_secs(1);

/// Remembers when the last frame was rendered, to space frames out.
#[derive(Debug, Default, Clone, Copy)]
pub struct FrameClock {
    last: Option<Instant>,
}

impl FrameClock {
    /// How long to wait at `now` before the next frame may be rendered.
    #[must_use]
    pub fn delay(&self, now: Instant, interval: Duration) -> Duration {
        self.last.map_or(Duration::ZERO, |last| {
            (last + interval).saturating_duration_since(now)
        })
    }

    /// Records that a frame was rendered at `now`.
    pub const fn mark(&mut self, now: Instant) {
        self.last = Some(now);
    }
}

/// A rendered frame as sent in a `frame` event.
#[derive(Debug, serde::Serialize)]
struct FrameMessage<'a> {
    seq: u64,
    rows: u16,
    cols: u16,
    svg: &'a str,
}

/// A rendered screen, before it's numbered and serialized.
struct Rendered {
    rows: u16,
    cols: u16,
    svg: String,
}

/// Snapshots the session's screen and renders it, on a blocking thread. `None` once the session
/// is gone.
async fn render(session: Weak<TuiSession>) -> Option<Rendered> {
    tokio::task::spawn_blocking(move || {
        let session = session.upgrade()?;
        let screen = session.snapshot().ok();
        // If `tui_end` ran meanwhile this is the last handle, and dropping it tears the session
        // down; that's fine here, on a blocking thread
        drop(session);
        let screen = screen?;
        Some(Rendered {
            rows: screen.rows,
            cols: screen.cols,
            svg: render_svg(&screen, &Theme::default()),
        })
    })
    .await
    .ok()
    .flatten()
}

/// What the producer watches. It only holds a weak handle on the session, so it never keeps a
/// session alive after `tui_end`.
pub struct FrameSource {
    pub session: Weak<TuiSession>,
    /// The session's revision counter; closed once the session is gone.
    pub changes: watch::Receiver<u64>,
    pub exit: watch::Receiver<Option<ExitStatus>>,
}

/// The last frame published for an entry, to number frames and skip unchanged ones.
#[derive(Debug, Default)]
pub struct LastFrame {
    seq: u64,
    svg: String,
}

/// Publishes a rendered screen to the entry's viewers, unless it's the same as the last one.
fn publish(entry: &LiveEntry, rendered: Rendered) {
    let mut last = super::lock(&entry.last_frame);
    if rendered.svg == last.svg {
        return;
    }
    let message = FrameMessage {
        seq: last.seq + 1,
        rows: rendered.rows,
        cols: rendered.cols,
        svg: &rendered.svg,
    };
    match serde_json::to_string(&message) {
        Ok(json) => {
            entry.frame.send_replace(Some(Arc::from(json)));
            last.seq += 1;
            last.svg = rendered.svg;
        }
        Err(e) => tracing::warn!("failed to serialize live frame: {e}"),
    }
    drop(last);
}

/// Renders the session's screen now and publishes it if it changed.
pub async fn render_and_publish(entry: &LiveEntry, session: Weak<TuiSession>) {
    if let Some(rendered) = render(session).await {
        publish(entry, rendered);
    }
}

/// The producer's state between frames.
struct Producer {
    entry: Arc<LiveEntry>,
    session: Weak<TuiSession>,
    clock: FrameClock,
}

impl Producer {
    /// Renders the screen and publishes it if it changed.
    async fn frame(&mut self) {
        self.clock.mark(Instant::now());
        render_and_publish(&self.entry, self.session.clone()).await;
    }

    /// Records the exit status, if the process has exited.
    fn exit_changed(&self, status: Option<ExitStatus>) {
        let Some(status) = status else {
            return;
        };
        self.entry.status.send_if_modified(|current| {
            if current.exit_status.is_some() {
                return false;
            }
            current.exit_status = Some(status);
            if current.state == SessionState::Running {
                current.state = SessionState::Exited;
            }
            current.ended_ms = Some(unix_ms());
            true
        });
    }

    /// Records that the session was ended with `tui_end` (or replaced).
    fn session_closed(&self) {
        self.entry.status.send_if_modified(|current| {
            let changed = current.state != SessionState::Closed;
            current.state = SessionState::Closed;
            current.ended_ms.get_or_insert_with(unix_ms);
            changed
        });
    }
}

/// Publishes frames and status for one live session until the session is gone or the entry is
/// closed (the id was reused, or the server is shutting down).
pub async fn run_producer(entry: Arc<LiveEntry>, source: FrameSource) {
    let FrameSource {
        session,
        mut changes,
        mut exit,
    } = source;
    let mut closed = entry.closed.subscribe();
    let mut producer = Producer {
        entry: Arc::clone(&entry),
        session,
        clock: FrameClock::default(),
    };
    producer.exit_changed(*exit.borrow_and_update());

    let mut dirty = true;
    let mut session_open = true;
    let mut exit_open = true;
    while !*closed.borrow_and_update() {
        if dirty {
            let interval = if entry.frame.receiver_count() > 0 {
                FRAME_INTERVAL
            } else {
                IDLE_FRAME_INTERVAL
            };
            let delay = producer.clock.delay(Instant::now(), interval);
            if delay.is_zero() {
                dirty = false;
                // Seen before the snapshot, so a change during the render isn't lost
                changes.borrow_and_update();
                producer.frame().await;
            } else {
                // A viewer joining shortens the interval, so re-check when one does
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = entry.viewer_joined.notified() => {}
                    _ = closed.changed() => {}
                }
                continue;
            }
        }
        if !session_open && !exit_open {
            break;
        }
        tokio::select! {
            changed = changes.changed(), if session_open => {
                if changed.is_err() {
                    session_open = false;
                    // The PTY can close just before the exit is reported; include it if known
                    producer.exit_changed(*exit.borrow_and_update());
                    producer.session_closed();
                }
                dirty = true;
            }
            changed = exit.changed(), if exit_open => {
                exit_open = changed.is_ok();
                producer.exit_changed(*exit.borrow_and_update());
            }
            () = entry.viewer_joined.notified() => {}
            _ = closed.changed() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_frame_is_immediate() {
        let clock = FrameClock::default();
        assert_eq!(clock.delay(Instant::now(), FRAME_INTERVAL), Duration::ZERO);
    }

    #[test]
    fn test_frames_are_spaced_by_the_interval() {
        let start = Instant::now();
        let mut clock = FrameClock::default();
        clock.mark(start);
        assert_eq!(clock.delay(start, FRAME_INTERVAL), FRAME_INTERVAL);
        assert_eq!(
            clock.delay(start + Duration::from_millis(50), FRAME_INTERVAL),
            Duration::from_millis(16)
        );
        assert_eq!(
            clock.delay(start + FRAME_INTERVAL, FRAME_INTERVAL),
            Duration::ZERO
        );
        assert_eq!(
            clock.delay(start + Duration::from_secs(5), FRAME_INTERVAL),
            Duration::ZERO
        );
    }

    #[test]
    fn test_fps_cap() {
        assert_eq!(FRAME_INTERVAL, Duration::from_millis(66));
        assert!(IDLE_FRAME_INTERVAL > FRAME_INTERVAL);
    }
}
