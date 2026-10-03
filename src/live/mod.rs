//! Live web viewer: lets a person watch a session in a browser while the agent drives it.
//!
//! `tui_start` with `live: true` starts (once per MCP server) a small HTTP server bound to
//! 127.0.0.1 on an ephemeral port and returns a link to the session's page. The page receives
//! server-sent events:
//!
//! - `status`: session id, command, `running` / `exited` / `closed`, exit status, start and end
//!   times (see [`Status`]);
//! - `frame`: the screen as SVG, rendered server-side from the session's one emulator, so the
//!   person sees exactly what the agent reads (see [`frames`]);
//! - `report`: one session-report event (start, input, signal, check, screenshot, exit, summary)
//!   as JSON.
//!
//! Security: loopback only, a random 128-bit token required in every URL (`?t=`), the `Host`
//! header must be `127.0.0.1:PORT` or `localhost:PORT` (against DNS rebinding), and the viewer is
//! read-only: nothing the browser sends reaches the session.
//!
//! Nothing here writes to stdout, which is the MCP channel.

mod frames;
mod http;

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::Read as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore, broadcast, watch};
use tokio::task::JoinHandle;

pub use frames::{FRAME_INTERVAL, MAX_FPS};

use crate::pty_manager::PtyManager;
use crate::session::ExitStatus;
use frames::{FrameSource, render_and_publish, run_producer};
use http::{Request, Route};

/// The viewer page, one self-contained file.
const PAGE_HTML: &str = include_str!("../../assets/live/index.html");

/// Report events kept per session for viewers who connect later.
const REPORT_HISTORY: usize = 2000;

/// Report events buffered per viewer; a viewer that falls further behind reconnects and gets
/// the history again.
const REPORT_CHANNEL: usize = 256;

/// Most connections served at once.
const MAX_CONNECTIONS: usize = 64;

/// How long a client has to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a write to a viewer may take before it's dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often an idle event stream gets a comment, so proxies and browsers keep it open and a
/// closed tab is noticed.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Milliseconds since the Unix epoch, for the page's clock.
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A random 128-bit token as hex, from the kernel's CSPRNG.
fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut urandom| urandom.read_exact(&mut bytes))
        .context("failed to read /dev/urandom for the live viewer token")?;
    Ok(bytes.iter().fold(String::with_capacity(32), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    }))
}

/// Where a live session is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// The process is running.
    Running,
    /// The process exited; the session is still open.
    Exited,
    /// The session was ended with `tui_end` (or replaced); the last frame stays visible.
    Closed,
}

/// The data of a `status` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub session_id: String,
    /// The command line as started.
    pub command: String,
    pub state: SessionState,
    /// `null` while running, then `{"exit_code": N}`, `{"signal": N}` or `"unknown"`.
    pub exit_status: Option<ExitStatus>,
    /// When the session started, in milliseconds since the Unix epoch.
    pub started_ms: u64,
    /// When the process exited or the session was closed, in milliseconds since the Unix epoch.
    pub ended_ms: Option<u64>,
}

/// Everything viewers of one session see. Outlives the session, so an ended session keeps its
/// last frame and events until the id is reused or the server stops.
pub struct LiveEntry {
    status: watch::Sender<Status>,
    /// The latest `frame` event data (JSON), `None` until the first render.
    frame: watch::Sender<Option<Arc<str>>>,
    last_frame: Mutex<frames::LastFrame>,
    reports: broadcast::Sender<Arc<str>>,
    history: Mutex<VecDeque<Arc<str>>>,
    /// Wakes the producer when a viewer connects, so it renders a pending frame right away.
    viewer_joined: Notify,
    /// Set when the id is reused or the server stops: streams end and browsers reconnect.
    closed: watch::Sender<bool>,
}

impl LiveEntry {
    fn new(status: Status) -> Self {
        Self {
            status: watch::Sender::new(status),
            frame: watch::Sender::new(None),
            last_frame: Mutex::new(frames::LastFrame::default()),
            reports: broadcast::Sender::new(REPORT_CHANNEL),
            history: Mutex::new(VecDeque::new()),
            viewer_joined: Notify::new(),
            closed: watch::Sender::new(false),
        }
    }

    /// Subscribes to new report events and returns the ones sent so far. Both happen under the
    /// history lock, so no event is missed or seen twice.
    fn subscribe_reports(&self) -> (broadcast::Receiver<Arc<str>>, Vec<Arc<str>>) {
        let history = lock(&self.history);
        let receiver = self.reports.subscribe();
        let past = history.iter().cloned().collect();
        drop(history);
        (receiver, past)
    }

    fn close(&self) {
        self.closed.send_replace(true);
    }
}

/// Sends report events to a session's viewers.
#[derive(Clone)]
pub struct ReportSink {
    entry: Arc<LiveEntry>,
}

impl ReportSink {
    /// Sends one report event (e.g. a `ReportEvent`) to current and future viewers.
    pub fn push<T: Serialize + ?Sized>(&self, event: &T) {
        match serde_json::to_string(event) {
            Ok(json) => self.push_json(Arc::from(json)),
            Err(e) => tracing::warn!("failed to serialize report event for the live viewer: {e}"),
        }
    }

    fn push_json(&self, json: Arc<str>) {
        let mut history = lock(&self.entry.history);
        if history.len() == REPORT_HISTORY {
            history.pop_front();
        }
        history.push_back(Arc::clone(&json));
        // No receivers is fine: nobody is watching right now
        let _ = self.entry.reports.send(json);
        drop(history);
    }
}

/// Forwards the session's report events (start, inputs, checks, screenshots, exit, summary) to
/// its viewers as `report` events: the ones so far, then each new one until the session's report
/// is gone.
async fn forward_report_events(
    manager: &PtyManager,
    session_id: &str,
    sink: ReportSink,
) -> Result<()> {
    let subscription = manager.subscribe_report_session(session_id).await?;
    for event in &subscription.history {
        sink.push(event);
    }
    let mut events = subscription.live;
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => sink.push(&event),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!("live viewer skipped {skipped} report events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    Ok(())
}

/// State shared by the listener and every connection.
struct Hub {
    entries: Mutex<HashMap<String, Arc<LiveEntry>>>,
    token: String,
    port: u16,
}

impl Hub {
    fn entry(&self, session_id: &str) -> Option<Arc<LiveEntry>> {
        lock(&self.entries).get(session_id).cloned()
    }
}

/// The live viewer's HTTP server, shared by all sessions of one MCP server.
pub struct LiveServer {
    hub: Arc<Hub>,
    accept: JoinHandle<()>,
}

impl LiveServer {
    /// Binds 127.0.0.1 on an ephemeral port and starts serving.
    pub async fn start() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("failed to bind the live viewer to 127.0.0.1")?;
        let port = listener.local_addr()?.port();
        let hub = Arc::new(Hub {
            entries: Mutex::new(HashMap::new()),
            token: random_token()?,
            port,
        });
        let accept = tokio::spawn(accept_loop(listener, Arc::clone(&hub)));
        tracing::info!("live viewer listening on 127.0.0.1:{port}");
        Ok(Self { hub, accept })
    }

    /// The address the server listens on.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.hub.port))
    }

    /// Link to the page listing the live sessions.
    #[must_use]
    pub fn index_url(&self) -> String {
        format!("http://127.0.0.1:{}/?t={}", self.hub.port, self.hub.token)
    }

    /// Link to a session's viewer page.
    #[must_use]
    pub fn session_url(&self, session_id: &str) -> String {
        format!(
            "http://127.0.0.1:{}/s/{}?t={}",
            self.hub.port,
            http::percent_encode(session_id),
            self.hub.token
        )
    }

    /// Starts streaming a running session (replacing what was shown under its id) and returns
    /// the link to its page. `command` is the command line shown on the page.
    pub async fn watch(
        &self,
        manager: &PtyManager,
        session_id: &str,
        command: &str,
    ) -> Result<String> {
        let session = manager.get_session(session_id).await?;
        let entry = Arc::new(LiveEntry::new(Status {
            session_id: session_id.to_string(),
            command: command.to_string(),
            state: SessionState::Running,
            exit_status: None,
            started_ms: unix_ms(),
            ended_ms: None,
        }));
        let source = FrameSource {
            session: Arc::downgrade(&session),
            changes: session.watch_changes(),
            exit: session.watch_exit(),
        };
        drop(session);

        let previous = lock(&self.hub.entries).insert(session_id.to_string(), Arc::clone(&entry));
        if let Some(previous) = previous {
            previous.close();
        }
        tokio::spawn(run_producer(Arc::clone(&entry), source));
        forward_report_events(manager, session_id, ReportSink { entry }).await?;
        Ok(self.session_url(session_id))
    }

    /// Renders a live session's screen right away. Called before `tui_end` tears the session
    /// down, so the page keeps its final screen even if no frame was due yet.
    pub async fn capture_final_frame(&self, manager: &PtyManager, session_id: &str) {
        let Some(entry) = self.hub.entry(session_id) else {
            return;
        };
        if let Ok(session) = manager.get_session(session_id).await {
            render_and_publish(&entry, Arc::downgrade(&session)).await;
        }
    }

    /// Stops showing a session, e.g. when its id is reused without `live`. Its viewers are
    /// told the session is gone.
    pub fn forget(&self, session_id: &str) {
        let removed = lock(&self.hub.entries).remove(session_id);
        if let Some(entry) = removed {
            entry.close();
        }
    }

    /// Where report events for a live session go, or `None` if it isn't live.
    #[must_use]
    pub fn report_sink(&self, session_id: &str) -> Option<ReportSink> {
        self.hub.entry(session_id).map(|entry| ReportSink { entry })
    }
}

impl Drop for LiveServer {
    fn drop(&mut self) {
        self.accept.abort();
        for entry in lock(&self.hub.entries).values() {
            entry.close();
        }
    }
}

async fn accept_loop(listener: TcpListener, hub: Arc<Hub>) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::debug!("live viewer accept failed: {e}");
                continue;
            }
        };
        // Over the limit, refuse by closing the connection
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            continue;
        };
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(&hub, stream).await {
                tracing::debug!("live viewer connection ended: {e}");
            }
            drop(permit);
        });
    }
}

/// What to answer a request with.
enum Reply {
    Response(Vec<u8>),
    Events(Arc<LiveEntry>),
}

/// Checks and routes a request. The order matters: nothing about the sessions is revealed
/// before the `Host` header and the token are checked.
fn decide(hub: &Hub, request: &Request) -> Reply {
    if !http::host_allowed(request.host.as_deref(), hub.port) {
        return Reply::Response(http::text_response(
            http::MISDIRECTED,
            "Unexpected Host header",
        ));
    }
    let token = http::query_param(&request.query, "t").unwrap_or_default();
    if !http::tokens_match(&token, &hub.token) {
        return Reply::Response(http::text_response(
            http::FORBIDDEN,
            "Missing or wrong token: use the link from tui_start",
        ));
    }
    if request.method != "GET" {
        return Reply::Response(http::text_response(
            http::METHOD_NOT_ALLOWED,
            "The live viewer is read-only",
        ));
    }
    match http::route(&request.path) {
        Route::Index => Reply::Response(http::response(
            http::OK,
            "text/html; charset=utf-8",
            index_page(hub).as_bytes(),
        )),
        Route::Page(_) => Reply::Response(http::response(
            http::OK,
            "text/html; charset=utf-8",
            PAGE_HTML.as_bytes(),
        )),
        Route::Events(session_id) => hub.entry(&session_id).map_or_else(
            || {
                Reply::Response(http::text_response(
                    http::NOT_FOUND,
                    "No live session with this id",
                ))
            },
            Reply::Events,
        ),
        Route::NotFound => Reply::Response(http::text_response(http::NOT_FOUND, "Not found")),
    }
}

/// The list of live sessions, with links.
fn index_page(hub: &Hub) -> String {
    let mut statuses: Vec<Status> = lock(&hub.entries)
        .values()
        .map(|entry| entry.status.borrow().clone())
        .collect();
    statuses.sort_by(|a, b| a.session_id.cmp(&b.session_id));

    let mut rows = String::new();
    for status in &statuses {
        let state = match (status.state, status.exit_status) {
            (SessionState::Running, _) => "running".to_string(),
            (_, Some(exit)) => exit.to_string(),
            (SessionState::Exited, None) => "exited".to_string(),
            (SessionState::Closed, None) => "closed".to_string(),
        };
        let _ = write!(
            rows,
            "<li><a href=\"/s/{}?t={}\">{}</a> <code>{}</code> <span>{}</span></li>",
            http::percent_encode(&status.session_id),
            hub.token,
            http::escape_html(&status.session_id),
            http::escape_html(&status.command),
            http::escape_html(&state),
        );
    }
    if rows.is_empty() {
        rows.push_str("<li>No live sessions yet. Start one with <code>tui_start</code> and <code>live: true</code>.</li>");
    }
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<meta http-equiv=\"refresh\" content=\"5\"><link rel=\"icon\" href=\"data:,\">\
<title>ShadowPTY live sessions</title><style>\
:root{{color-scheme:light dark;--bg:#fafaf9;--fg:#1c1917;--muted:#78716c;--link:#1d4ed8}}\
@media (prefers-color-scheme:dark){{:root{{--bg:#1c1917;--fg:#e7e5e4;--muted:#a8a29e;--link:#93c5fd}}}}\
body{{margin:0;padding:24px 16px;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif}}\
main{{max-width:760px;margin:0 auto}}h1{{font-size:20px}}ul{{padding-left:20px}}li{{margin:6px 0}}\
a{{color:var(--link);font-weight:600}}code,span{{color:var(--muted)}}code{{font-size:13px}}\
</style></head><body><main><h1>ShadowPTY live sessions</h1><ul>{rows}</ul></main></body></html>"
    )
}

/// Writes `bytes`, giving up on a viewer that doesn't read.
async fn send(stream: &mut (impl AsyncWriteExt + Unpin), bytes: &[u8]) -> std::io::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(bytes))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "write timed out"))?
}

/// Reads the request head, up to `http::MAX_HEAD_BYTES`.
async fn read_head(stream: &mut TcpStream) -> std::io::Result<Result<Request, http::ParseError>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(len) = http::head_len(&buf) {
            return Ok(http::parse_head(&buf[..len]));
        }
        if buf.len() > http::MAX_HEAD_BYTES {
            return Ok(Err(http::ParseError::TooLarge));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn handle_connection(hub: &Hub, mut stream: TcpStream) -> std::io::Result<()> {
    let head = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "no request"))??;
    let reply = match head {
        Ok(request) => decide(hub, &request),
        Err(http::ParseError::TooLarge) => Reply::Response(http::text_response(
            http::HEAD_TOO_LARGE,
            "Request head too large",
        )),
        Err(http::ParseError::Malformed) => {
            Reply::Response(http::text_response(http::BAD_REQUEST, "Malformed request"))
        }
    };
    match reply {
        Reply::Response(bytes) => {
            send(&mut stream, &bytes).await?;
            stream.shutdown().await
        }
        Reply::Events(entry) => stream_events(stream, &entry).await,
    }
}

/// Serves one viewer's event stream: the current status, past report events and the latest
/// frame, then every change until the viewer leaves or the entry is closed.
async fn stream_events(mut stream: TcpStream, entry: &LiveEntry) -> std::io::Result<()> {
    let mut status = entry.status.subscribe();
    let mut frame = entry.frame.subscribe();
    let mut closed = entry.closed.subscribe();
    let (mut reports, history) = entry.subscribe_reports();
    entry.viewer_joined.notify_one();

    let (mut reader, mut writer) = stream.split();
    let mut start = http::event_stream_head();
    let status_json = serde_json::to_string(&*status.borrow_and_update())?;
    start.push_str(&http::sse_event("status", &status_json));
    for event in &history {
        start.push_str(&http::sse_event("report", event));
    }
    let current_frame = frame.borrow_and_update().clone();
    if let Some(data) = current_frame {
        start.push_str(&http::sse_event("frame", &data));
    }
    send(&mut writer, start.as_bytes()).await?;

    let mut keepalive = tokio::time::interval_at(
        tokio::time::Instant::now() + KEEPALIVE_INTERVAL,
        KEEPALIVE_INTERVAL,
    );
    let mut scratch = [0u8; 256];
    while !*closed.borrow_and_update() {
        let message = tokio::select! {
            _ = frame.changed() => {
                let data = frame.borrow_and_update().clone();
                data.map(|data| http::sse_event("frame", &data))
            }
            _ = status.changed() => {
                let json = serde_json::to_string(&*status.borrow_and_update())?;
                Some(http::sse_event("status", &json))
            }
            event = reports.recv() => match event {
                Ok(event) => Some(http::sse_event("report", &event)),
                // Too far behind, or gone: end the stream; the browser reconnects and replays
                // the history
                Err(_) => return Ok(()),
            },
            _ = closed.changed() => None,
            _ = keepalive.tick() => Some(": keepalive\n\n".to_string()),
            // The viewer never sends anything after its request; a read ends when it leaves
            _ = reader.read(&mut scratch) => return Ok(()),
        };
        if let Some(message) = message {
            send(&mut writer, message.as_bytes()).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn hub() -> Hub {
        Hub {
            entries: Mutex::new(HashMap::new()),
            token: "0123456789abcdef0123456789abcdef".to_string(),
            port: 4321,
        }
    }

    fn request(method: &str, target: &str, host: &str) -> Request {
        let head = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\n\r\n");
        http::parse_head(head.as_bytes()).unwrap()
    }

    fn status_of(reply: &Reply) -> u16 {
        match reply {
            Reply::Response(bytes) => String::from_utf8_lossy(&bytes[9..12]).parse().unwrap(),
            Reply::Events(_) => 200,
        }
    }

    fn status(session_id: &str) -> Status {
        Status {
            session_id: session_id.to_string(),
            command: "sh -c 'echo <hi>'".to_string(),
            state: SessionState::Running,
            exit_status: None,
            started_ms: 1,
            ended_ms: None,
        }
    }

    #[test]
    fn test_random_token_is_128_bit_hex() {
        let token = random_token().unwrap();
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(token, random_token().unwrap());
    }

    #[test]
    fn test_decide_checks_host_then_token_then_method() {
        let hub = hub();
        let t = "t=0123456789abcdef0123456789abcdef";
        let ok_host = "127.0.0.1:4321";
        assert_eq!(
            status_of(&decide(&hub, &request("GET", &format!("/?{t}"), ok_host))),
            200
        );
        assert_eq!(
            status_of(&decide(
                &hub,
                &request("GET", &format!("/?{t}"), "localhost:4321")
            )),
            200
        );
        assert_eq!(
            status_of(&decide(
                &hub,
                &request("GET", &format!("/?{t}"), "evil.test:4321")
            )),
            421
        );
        assert_eq!(status_of(&decide(&hub, &request("GET", "/", ok_host))), 403);
        assert_eq!(
            status_of(&decide(&hub, &request("GET", "/?t=0123", ok_host))),
            403
        );
        assert_eq!(
            status_of(&decide(&hub, &request("POST", &format!("/?{t}"), ok_host))),
            405
        );
        assert_eq!(
            status_of(&decide(
                &hub,
                &request("GET", &format!("/s/x/events?{t}"), ok_host)
            )),
            404
        );
        assert_eq!(
            status_of(&decide(
                &hub,
                &request("GET", &format!("/nope?{t}"), ok_host)
            )),
            404
        );
        // The page itself is served for any id; the stream says whether the session exists
        assert_eq!(
            status_of(&decide(
                &hub,
                &request("GET", &format!("/s/x?{t}"), ok_host)
            )),
            200
        );

        lock(&hub.entries).insert("x".into(), Arc::new(LiveEntry::new(status("x"))));
        assert!(matches!(
            decide(&hub, &request("GET", &format!("/s/x/events?{t}"), ok_host)),
            Reply::Events(_)
        ));
    }

    #[test]
    fn test_index_lists_sessions_escaped() {
        let hub = hub();
        lock(&hub.entries).insert("a b".into(), Arc::new(LiveEntry::new(status("a b"))));
        let page = index_page(&hub);
        assert!(
            page.contains("href=\"/s/a%20b?t=0123456789abcdef0123456789abcdef\""),
            "{page}"
        );
        assert!(page.contains("sh -c &#39;echo &lt;hi&gt;&#39;"), "{page}");
        assert!(page.contains("running"), "{page}");
    }

    #[test]
    fn test_report_history_is_bounded_and_replayed() {
        let entry = Arc::new(LiveEntry::new(status("x")));
        let sink = ReportSink {
            entry: Arc::clone(&entry),
        };
        for i in 0..REPORT_HISTORY + 5 {
            sink.push(&serde_json::json!({ "type": "input", "at_ms": i }));
        }
        let (mut receiver, history) = entry.subscribe_reports();
        assert_eq!(history.len(), REPORT_HISTORY);
        assert!(history[0].contains("\"at_ms\":5"), "{}", history[0]);

        sink.push(&serde_json::json!({ "type": "exit" }));
        assert_eq!(&*receiver.try_recv().unwrap(), "{\"type\":\"exit\"}");
    }

    #[test]
    fn test_status_serializes_exit_status() {
        let mut status = status("x");
        assert!(
            serde_json::to_string(&status)
                .unwrap()
                .contains("\"state\":\"running\",\"exit_status\":null")
        );
        status.state = SessionState::Exited;
        status.exit_status = Some(ExitStatus::Code(3));
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            json.contains("\"state\":\"exited\",\"exit_status\":{\"exit_code\":3}"),
            "{json}"
        );
    }
}
