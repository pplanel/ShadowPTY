#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Integration tests for the live viewer: `tui_start` with `live: true`, then plain HTTP over a
//! raw `TcpStream` against the page, the security checks and the event stream.

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use serde_json::Value;
use shadowpty::live::LiveServer;
use shadowpty::output::Pattern;
use shadowpty::pty_manager::{ExpectTarget, Expectation, PtyConfig, PtyManager};
use shadowpty::server::{ShadowPtyServer, TuiEndParams, TuiInputParams, TuiStartParams};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const ROWS: u16 = 43;
const COLS: u16 = 155;
const WAIT: Duration = Duration::from_secs(10);

fn text_of(result: &CallToolResult) -> String {
    let content = serde_json::to_value(&result.content).expect("serialize");
    content[0]["text"].as_str().expect("text").to_string()
}

/// A viewer link, split into what a raw HTTP request needs.
struct LiveUrl {
    port: u16,
    /// `/s/<id>` (still encoded).
    path: String,
    token: String,
}

impl LiveUrl {
    fn parse(text: &str) -> Self {
        let start = text.find("http://127.0.0.1:").expect("live URL in reply");
        let url = text[start..].split_whitespace().next().unwrap();
        let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
        let (port, target) = rest.split_once('/').unwrap();
        let (path, query) = target.split_once("?t=").unwrap();
        Self {
            port: port.parse().unwrap(),
            path: format!("/{path}"),
            token: query.to_string(),
        }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

/// Starts `sh -c script` as a live session and returns its viewer link.
async fn start_live(server: &ShadowPtyServer, session_id: &str, script: &str) -> LiveUrl {
    let started = server
        .tui_start(Parameters(TuiStartParams {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            rows: Some(ROWS),
            cols: Some(COLS),
            session_id: Some(session_id.to_string()),
            live: Some(true),
            ..TuiStartParams::default()
        }))
        .await
        .expect("tool call ok");
    let text = text_of(&started);
    assert!(!started.is_error.unwrap_or(false), "{text}");
    assert!(text.contains(", watch live at http://127.0.0.1:"), "{text}");
    LiveUrl::parse(&text)
}

async fn wait_on_screen(manager: &PtyManager, session_id: &str, text: &str) {
    let expectation = Expectation::new(Pattern::literal(text).unwrap(), ExpectTarget::Screen, WAIT);
    manager
        .expect_session(session_id, &expectation)
        .await
        .expect("text on screen");
}

/// Sends one GET and returns the status code and the whole response.
async fn get(port: u16, target: &str, host: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let request = format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(WAIT, stream.read_to_string(&mut response))
        .await
        .expect("response in time")
        .unwrap();
    let code = response[9..12].parse().unwrap();
    (code, response)
}

/// A server-sent event stream read over a raw socket.
struct EventStream {
    reader: BufReader<TcpStream>,
}

impl EventStream {
    async fn open(url: &LiveUrl) -> Self {
        let mut stream = TcpStream::connect(("127.0.0.1", url.port)).await.unwrap();
        let request = format!(
            "GET {}/events?t={} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n\r\n",
            url.path,
            url.token,
            url.host()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("HTTP/1.1 200"), "{line}");
        let mut head = String::new();
        while line != "\r\n" {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            head.push_str(&line);
        }
        assert!(head.contains("Content-Type: text/event-stream"), "{head}");
        Self { reader }
    }

    /// The next event's name and data (data lines joined with `\n`).
    async fn next(&mut self) -> (String, String) {
        let mut event = String::new();
        let mut data: Vec<String> = Vec::new();
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await.unwrap();
            assert!(n > 0, "event stream closed");
            let line = line.trim_end_matches('\n');
            if line.is_empty() {
                if !event.is_empty() {
                    return (event, data.join("\n"));
                }
                continue;
            }
            if let Some(name) = line.strip_prefix("event: ") {
                event = name.to_string();
            } else if let Some(value) = line.strip_prefix("data: ") {
                data.push(value.to_string());
            }
        }
    }

    /// Reads events until `event` arrives with data that satisfies `accept`, and returns it.
    async fn until(&mut self, event: &str, accept: impl Fn(&Value) -> bool + Send + Sync) -> Value {
        let found = tokio::time::timeout(WAIT, async {
            loop {
                let (name, data) = self.next().await;
                if name == event {
                    let value: Value = serde_json::from_str(&data).unwrap();
                    if accept(&value) {
                        return value;
                    }
                }
            }
        })
        .await;
        assert!(found.is_ok(), "no matching '{event}' event within {WAIT:?}");
        found.unwrap()
    }

    /// Reads events until a `frame` satisfying `frame_ok` and a `status` satisfying `status_ok`
    /// have both arrived, in either order: frames are spaced out, status changes aren't.
    async fn until_frame_and_status(
        &mut self,
        frame_ok: impl Fn(&Value) -> bool + Send + Sync,
        status_ok: impl Fn(&Value) -> bool + Send + Sync,
    ) -> (Value, Value) {
        let mut frame = None;
        let mut status = None;
        let finished = tokio::time::timeout(WAIT, async {
            while frame.is_none() || status.is_none() {
                let (name, data) = self.next().await;
                let value: Value = serde_json::from_str(&data).unwrap();
                if name == "frame" && frame.is_none() && frame_ok(&value) {
                    frame = Some(value);
                } else if name == "status" && status.is_none() && status_ok(&value) {
                    status = Some(value);
                }
            }
        })
        .await;
        assert!(
            finished.is_ok(),
            "within {WAIT:?}: frame found: {}, status found: {status:?}",
            frame.is_some()
        );
        (frame.unwrap(), status.unwrap())
    }
}

fn svg_contains(frame: &Value, text: &str) -> bool {
    frame["svg"].as_str().is_some_and(|svg| svg.contains(text))
}

#[tokio::test]
async fn test_live_page_and_access_checks() {
    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());
    let url = start_live(&server, "live-page", "echo PAGE_READY; sleep 30").await;
    let host = url.host();
    let token = &url.token;
    assert_eq!(token.len(), 32, "128-bit hex token");

    let (code, page) = get(url.port, &format!("{}?t={token}", url.path), &host).await;
    assert_eq!(code, 200, "{page}");
    assert!(page.contains("Content-Type: text/html"), "{page}");
    assert!(page.contains("EventSource"), "{page}");
    assert!(page.contains("Referrer-Policy: no-referrer"), "{page}");

    let (code, index) = get(url.port, &format!("/?t={token}"), "localhost:0").await;
    assert_eq!(code, 421, "wrong port in Host: {index}");
    let (code, index) = get(
        url.port,
        &format!("/?t={token}"),
        &format!("localhost:{}", url.port),
    )
    .await;
    assert_eq!(code, 200, "{index}");
    assert!(index.contains("live-page"), "{index}");

    // No token, a wrong token, a wrong Host
    let (code, _) = get(url.port, &url.path, &host).await;
    assert_eq!(code, 403);
    let wrong = "0".repeat(32);
    let (code, _) = get(url.port, &format!("{}?t={wrong}", url.path), &host).await;
    assert_eq!(code, 403);
    let (code, _) = get(url.port, &format!("{}/events?t={wrong}", url.path), &host).await;
    assert_eq!(code, 403);
    let (code, _) = get(
        url.port,
        &format!("{}?t={token}", url.path),
        &format!("rebind.example:{}", url.port),
    )
    .await;
    assert!(code == 403 || code == 421, "{code}");

    let (code, _) = get(url.port, &format!("/s/unknown/events?t={token}"), &host).await;
    assert_eq!(code, 404);

    manager.stop_session("live-page").await.expect("stop");
}

#[tokio::test]
async fn test_live_stream_follows_screen_and_exit() {
    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());
    let url = start_live(
        &server,
        "live-stream",
        "echo LIVE_READY; read line; echo \"GOT_$line\"; exit 7",
    )
    .await;
    wait_on_screen(&manager, "live-stream", "LIVE_READY").await;

    let mut events = EventStream::open(&url).await;
    let (name, data) = events.next().await;
    assert_eq!(name, "status", "{data}");
    let status: Value = serde_json::from_str(&data).unwrap();
    assert_eq!(status["session_id"], "live-stream");
    assert_eq!(status["state"], "running");
    assert!(
        status["command"]
            .as_str()
            .unwrap()
            .starts_with("sh -c echo LIVE_READY")
    );

    let frame = events
        .until("frame", |frame| svg_contains(frame, "LIVE_READY"))
        .await;
    assert_eq!(
        (frame["rows"].as_u64(), frame["cols"].as_u64()),
        (Some(43), Some(155))
    );
    let first_seq = frame["seq"].as_u64().unwrap();

    server
        .tui_input(Parameters(TuiInputParams {
            keys: "hello<ENTER>".to_string(),
            session_id: Some("live-stream".to_string()),
        }))
        .await
        .expect("tool call ok");
    let (frame, status) = events
        .until_frame_and_status(
            |frame| svg_contains(frame, "GOT_hello"),
            |status| status["state"] == "exited",
        )
        .await;
    assert!(frame["seq"].as_u64().unwrap() > first_seq);
    assert_eq!(status["exit_status"], serde_json::json!({ "exit_code": 7 }));
    assert!(status["ended_ms"].as_u64().is_some());

    manager.stop_session("live-stream").await.expect("stop");
}

#[tokio::test]
async fn test_output_burst_is_coalesced_under_the_fps_cap() {
    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());
    let url = start_live(
        &server,
        "live-burst",
        "echo BURST_READY; read go; i=0; while [ $i -lt 3000 ]; do echo \"line $i\"; i=$((i+1)); done; echo BURST_DONE; sleep 30",
    )
    .await;
    wait_on_screen(&manager, "live-burst", "BURST_READY").await;
    let mut events = EventStream::open(&url).await;
    events
        .until("frame", |frame| svg_contains(frame, "BURST_READY"))
        .await;

    let started = std::time::Instant::now();
    manager
        .send_input_session("live-burst", "go<ENTER>")
        .await
        .expect("input");
    let mut frames = 0u32;
    tokio::time::timeout(WAIT, async {
        loop {
            let (name, data) = events.next().await;
            if name == "frame" {
                frames += 1;
                if data.contains("BURST_DONE") {
                    break;
                }
            }
        }
    })
    .await
    .expect("burst finished on screen");
    let elapsed = started.elapsed();

    // Thousands of chunks, but at most one frame per interval (plus one already due)
    let cap = u32::try_from(elapsed.as_millis() / shadowpty::live::FRAME_INTERVAL.as_millis())
        .unwrap()
        + 2;
    assert!(frames <= cap, "{frames} frames in {elapsed:?} (cap {cap})");

    manager.stop_session("live-burst").await.expect("stop");
}

#[tokio::test]
async fn test_ended_session_keeps_its_last_frame() {
    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());
    let url = start_live(&server, "live-end", "echo FINAL_SCREEN; sleep 30").await;
    wait_on_screen(&manager, "live-end", "FINAL_SCREEN").await;

    let ended = server
        .tui_end(Parameters(TuiEndParams {
            session_id: Some("live-end".to_string()),
        }))
        .await
        .expect("tool call ok");
    assert!(!ended.is_error.unwrap_or(false));

    // A viewer who arrives after the end still sees the final screen, and that it ended
    let mut events = EventStream::open(&url).await;
    events
        .until_frame_and_status(
            |frame| svg_contains(frame, "FINAL_SCREEN"),
            |status| status["state"] == "closed" && status["exit_status"].is_object(),
        )
        .await;

    // Reusing the id without `live` takes the old session off the viewer
    let reused = server
        .tui_start(Parameters(TuiStartParams {
            command: "cat".to_string(),
            session_id: Some("live-end".to_string()),
            ..TuiStartParams::default()
        }))
        .await
        .expect("tool call ok");
    assert!(!text_of(&reused).contains("watch live"));
    let (code, _) = get(
        url.port,
        &format!("{}/events?t={}", url.path, url.token),
        &url.host(),
    )
    .await;
    assert_eq!(code, 404);

    manager.stop_session("live-end").await.expect("stop");
}

#[tokio::test]
async fn test_report_events_reach_viewers_in_order() {
    let manager = PtyManager::new();
    let args = vec!["-c".to_string(), "sleep 30".to_string()];
    manager
        .start_session("live-report", &PtyConfig::new("sh", &args, ROWS, COLS))
        .await
        .expect("start");
    let live = LiveServer::start().await.expect("live server");
    let link = live
        .watch(&manager, "live-report", "sh -c 'sleep 30'")
        .await
        .expect("watch");
    let url = LiveUrl::parse(&link);
    let sink = live.report_sink("live-report").expect("sink");

    let fixture: Vec<Value> = include_str!("fixtures/live_report_events.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let (early, late) = fixture.split_at(4);

    // Events sent before a viewer connects are replayed to it, later ones are streamed
    for event in early {
        sink.push(event);
    }
    let mut events = EventStream::open(&url).await;
    for event in late {
        sink.push(event);
    }
    for expected in &fixture {
        let received = events.until("report", |_| true).await;
        assert_eq!(&received, expected);
    }

    manager.stop_session("live-report").await.expect("stop");
}
