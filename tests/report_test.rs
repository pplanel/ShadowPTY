#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

//! Integration tests for the JSON Lines session report (`report_path`) and the report events
//! every session broadcasts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use serde_json::{Value, json};
use shadowpty::output::Pattern;
use shadowpty::pty_manager::{
    ExpectTarget, Expectation, PtyConfig, PtyManager, ReportEvent, ReportTotals, Script,
};
use shadowpty::report::ReportEntry;
use shadowpty::server::{
    ShadowPtyServer, TuiEndParams, TuiExpectParams, TuiScreenshotParams, TuiStartParams,
};
use tokio::sync::broadcast;

const ROWS: u16 = 43;
const COLS: u16 = 155;

fn report_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "shadowpty_report_{name}_{}.jsonl",
        std::process::id()
    ))
}

/// Every line of the report, each parsed as a JSON object with a `type` and an `at_ms`.
fn read_report(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("read report")
        .lines()
        .map(|line| {
            let entry: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("invalid report line {line:?}: {e}"));
            assert!(entry["type"].is_string(), "{entry}");
            assert!(entry["at_ms"].is_u64(), "{entry}");
            entry
        })
        .collect()
}

fn of_type<'a>(entries: &'a [Value], kind: &str) -> Vec<&'a Value> {
    entries.iter().filter(|e| e["type"] == kind).collect()
}

fn expectation(pattern: &str, target: ExpectTarget, timeout_ms: u64) -> Expectation {
    Expectation {
        patterns: vec![Pattern::literal(pattern).unwrap()],
        target,
        timeout: Duration::from_millis(timeout_ms),
    }
}

async fn start_sh(manager: &PtyManager, script: &str, report: Option<&Path>) {
    let args = vec!["-c".to_string(), script.to_string()];
    let config = PtyConfig::new("sh", &args, ROWS, COLS)
        .with_report_path(report.map(|path| path.to_str().expect("utf-8 path")));
    manager.start_app(&config).await.expect("start sh");
}

/// Starts an interactive `sh` with a distinctive prompt.
async fn start_interactive_sh(manager: &PtyManager, report: Option<&Path>) {
    start_sh(manager, "export PS1='READY> '; exec sh -i", report).await;
}

/// Runs every kind of check once, in an interactive shell; all pass but one expect.
async fn run_every_kind_of_check(manager: &PtyManager) {
    manager
        .expect(&expectation("READY> ", ExpectTarget::Stream, 5_000))
        .await
        .expect("prompt");
    manager
        .send_input("printf 'A%sB\\n' X<ENTER>")
        .await
        .expect("input");
    manager
        .expect(&expectation("AXB", ExpectTarget::Stream, 5_000))
        .await
        .expect("stream match");
    manager
        .expect(&expectation("AXB", ExpectTarget::Screen, 5_000))
        .await
        .expect("screen match");
    manager
        .expect(&expectation("NEVER_PRINTED", ExpectTarget::Stream, 200))
        .await
        .expect_err("no match");
    manager.send_paste("true\n").await.expect("paste");
    manager.resize(40, 150).await.expect("resize");
    manager
        .wait_stable(Duration::from_millis(100), Duration::from_secs(3))
        .await
        .expect("stable");
    manager
        .wait_gone(
            &[Pattern::literal("NOT_ON_SCREEN").unwrap()],
            Duration::from_secs(1),
        )
        .await
        .expect("gone");
    let commands = ["echo SCRIPT_OK".to_string()];
    let outcome = manager
        .run_script(&Script {
            commands: &commands,
            prompt: Pattern::literal("READY> ").unwrap(),
            timeout_per_command: Duration::from_secs(5),
        })
        .await
        .expect("script");
    assert_eq!(outcome.error, None);
}

fn assert_start_and_inputs(entries: &[Value]) {
    let start = &entries[0];
    assert_eq!(start["type"], "start");
    assert_eq!(start["session_id"], "default");
    assert_eq!(start["command"], "sh");
    assert_eq!(start["args"][0], "-c");
    assert_eq!(
        (start["rows"].as_u64(), start["cols"].as_u64()),
        (Some(43), Some(155))
    );
    assert!(start["pid"].is_u64(), "{start}");
    assert_eq!(start["record_path"], Value::Null);
    assert!(start["timestamp"].as_u64().unwrap() > 1_700_000_000);
    assert_eq!(start["version"], 1);

    let input = of_type(entries, "input");
    assert_eq!(input.len(), 1, "{entries:?}");
    assert_eq!(input[0]["keys"], "printf 'A%sB\\n' X<ENTER>");
    assert_eq!(input[0]["bytes"], 18);
    assert_eq!(
        of_type(entries, "paste")[0]["text"],
        "true\n",
        "{entries:?}"
    );
    assert_eq!(of_type(entries, "resize")[0]["rows"], 40);
    assert_eq!(of_type(entries, "resize")[0]["cols"], 150);
}

fn assert_checks(entries: &[Value]) {
    let expects = of_type(entries, "expect");
    assert_eq!(expects.len(), 4, "{expects:?}");
    let stream = expects[1];
    assert_eq!(stream["target"], "stream");
    assert_eq!(stream["syntax"], "literal");
    assert_eq!(stream["patterns"], json!(["AXB"]));
    assert_eq!(stream["timeout_ms"], 5000);
    assert_eq!(stream["passed"], true);
    assert_eq!(stream["pattern_index"], 0);
    assert_eq!(stream["matched"], "AXB");
    assert_eq!(stream["row"], Value::Null);
    assert_eq!(stream["error"], Value::Null);
    let screen = expects[2];
    assert_eq!(screen["target"], "screen");
    assert_eq!(screen["passed"], true);
    assert!(screen["row"].as_u64().unwrap() >= 1, "{screen}");
    let failed = expects[3];
    assert_eq!(failed["passed"], false);
    assert_eq!(failed["pattern_index"], Value::Null);
    assert!(failed["elapsed_ms"].as_u64().unwrap() >= 200, "{failed}");
    let error = failed["error"].as_str().unwrap();
    assert!(error.contains("'NEVER_PRINTED' not found"), "{error}");
    // A check's line is written when it ends
    assert!(failed["at_ms"].as_u64() >= failed["elapsed_ms"].as_u64());

    let stable = of_type(entries, "wait_stable")[0];
    assert_eq!(stable["quiet_period_ms"], 100);
    assert_eq!(stable["timeout_ms"], 3000);
    assert_eq!(stable["passed"], true);
    let gone = of_type(entries, "wait_gone")[0];
    assert_eq!(gone["patterns"], json!(["NOT_ON_SCREEN"]));
    assert_eq!(gone["passed"], true);
    let script = of_type(entries, "run_script")[0];
    assert_eq!(script["commands"], json!(["echo SCRIPT_OK"]));
    assert_eq!(script["prompt"], "READY> ");
    assert_eq!(script["passed"], true);
    assert_eq!(script["completed"], 1);
}

#[tokio::test]
async fn test_report_logs_inputs_checks_exit_and_summary() {
    let path = report_path("checks");
    let manager = PtyManager::new();
    start_interactive_sh(&manager, Some(&path)).await;
    run_every_kind_of_check(&manager).await;

    // Valid line by line while the session is still running, with no summary yet
    let entries = read_report(&path);
    assert!(of_type(&entries, "summary").is_empty());
    assert_start_and_inputs(&entries);
    assert_checks(&entries);
    let last_check_at = of_type(&entries, "run_script")[0]["at_ms"].as_u64();

    let stopped = manager.stop_app().await.expect("stop");
    assert_eq!(stopped.report_path.as_deref(), path.to_str());
    assert_eq!(
        stopped.totals,
        ReportTotals {
            checks: 7,
            passed: 6,
            failed: 1,
        }
    );

    // Complete once the session is stopped: the kill, then the summary
    let entries = read_report(&path);
    let [.., exit, summary] = entries.as_slice() else {
        panic!("report too short: {entries:?}");
    };
    assert_eq!(exit["type"], "exit");
    assert_eq!(exit["exit_status"], json!({ "signal": 9 }));
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["checks"], 7);
    assert_eq!(summary["passed"], 6);
    assert_eq!(summary["failed"], 1);
    assert_eq!(summary["exit_status"], json!({ "signal": 9 }));
    assert!(summary["duration_ms"].as_u64() >= last_check_at);
    assert_eq!(of_type(&entries, "exit").len(), 1);
    assert_eq!(of_type(&entries, "summary").len(), 1);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_report_logs_failed_waits_and_script() {
    let path = report_path("failures");
    let manager = PtyManager::new();
    start_sh(&manager, "printf 'LOADING\\n'; sleep 30", Some(&path)).await;

    manager
        .expect(&expectation("LOADING", ExpectTarget::Screen, 5_000))
        .await
        .expect("loading shown");
    manager
        .wait_gone(
            &[Pattern::regex("LOAD(ING)?").unwrap()],
            Duration::from_millis(200),
        )
        .await
        .expect_err("never clears");
    manager
        .wait_exit(Duration::from_millis(200))
        .await
        .expect_err("still running");
    let commands = ["first".to_string(), "second".to_string()];
    let outcome = manager
        .run_script(&Script {
            commands: &commands,
            prompt: Pattern::glob("PROMPT*>").unwrap(),
            timeout_per_command: Duration::from_millis(300),
        })
        .await
        .expect("script ran");
    assert!(outcome.error.is_some());

    let stopped = manager.stop_app().await.expect("stop");
    assert_eq!(
        stopped.totals,
        ReportTotals {
            checks: 4,
            passed: 1,
            failed: 3,
        }
    );

    let entries = read_report(&path);
    let gone = of_type(&entries, "wait_gone")[0];
    assert_eq!(gone["syntax"], "regex");
    assert_eq!(gone["passed"], false);
    assert!(
        gone["error"].as_str().unwrap().contains("still on screen"),
        "{gone}"
    );
    let exit_wait = of_type(&entries, "wait_exit")[0];
    assert_eq!(exit_wait["passed"], false);
    assert_eq!(exit_wait["timeout_ms"], 200);
    assert_eq!(exit_wait["exit_status"], Value::Null);
    assert!(
        exit_wait["error"]
            .as_str()
            .unwrap()
            .contains("still running"),
        "{exit_wait}"
    );
    let script = of_type(&entries, "run_script")[0];
    assert_eq!(script["syntax"], "glob");
    assert_eq!(script["prompt"], "PROMPT*>");
    assert_eq!(script["timeout_ms"], 300);
    assert_eq!(script["passed"], false);
    assert_eq!(script["completed"], 0);
    assert!(
        script["error"]
            .as_str()
            .unwrap()
            .starts_with("command 1 ('first')"),
        "{script}"
    );

    let summary = entries.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(
        (&summary["checks"], &summary["passed"], &summary["failed"]),
        (&json!(4), &json!(1), &json!(3))
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_report_logs_failed_wait_stable() {
    let path = report_path("unstable");
    let manager = PtyManager::new();
    start_sh(
        &manager,
        "while :; do printf .; sleep 0.01; done",
        Some(&path),
    )
    .await;

    manager
        .wait_stable(Duration::from_millis(200), Duration::from_millis(400))
        .await
        .expect_err("never quiet");
    manager.stop_app().await.expect("stop");

    let entries = read_report(&path);
    let stable = of_type(&entries, "wait_stable")[0];
    assert_eq!(stable["passed"], false);
    assert_eq!(stable["quiet_period_ms"], 200);
    assert_eq!(stable["timeout_ms"], 400);
    assert!(
        stable["error"].as_str().unwrap().contains("timed out"),
        "{stable}"
    );
    assert_eq!(entries.last().unwrap()["failed"], 1);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_report_of_process_that_exits_on_its_own() {
    let path = report_path("natural_exit");
    let manager = PtyManager::new();
    start_sh(&manager, "printf 'BYE\\n'; exit 3", Some(&path)).await;

    let exit = manager
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("wait exit");
    assert_eq!(exit.status.to_string(), "exited with code 3");

    // The exit is logged as soon as the process is gone, before the session ends; a nonzero
    // code doesn't fail `wait_exit`
    let entries = read_report(&path);
    let kinds: Vec<&str> = entries
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["start", "exit", "wait_exit"]);
    assert_eq!(entries[1]["exit_status"], json!({ "exit_code": 3 }));
    assert_eq!(entries[2]["passed"], true);
    assert_eq!(entries[2]["exit_status"], json!({ "exit_code": 3 }));
    assert_eq!(entries[2]["error"], Value::Null);

    manager.stop_app().await.expect("stop");
    let entries = read_report(&path);
    assert_eq!(of_type(&entries, "exit").len(), 1, "{entries:?}");
    let summary = entries.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["checks"], 1);
    assert_eq!(summary["passed"], 1);
    assert_eq!(summary["failed"], 0);
    assert_eq!(summary["exit_status"], json!({ "exit_code": 3 }));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_report_tools_reply_and_screenshots() {
    let path = report_path("tools");
    let svg_path =
        std::env::temp_dir().join(format!("shadowpty_report_shot_{}.svg", std::process::id()));
    let path_str = path.to_str().unwrap().to_string();
    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());

    let started = server
        .tui_start(Parameters(TuiStartParams {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "printf 'TOOLS_READY\\n'; sleep 30".to_string(),
            ],
            rows: Some(ROWS),
            cols: Some(COLS),
            record_path: None,
            report_path: Some(path_str.clone()),
            session_id: Some("reported".to_string()),
        }))
        .await
        .expect("tool call ok");
    let text = serde_json::to_string(&started.content).unwrap();
    assert!(
        text.contains(&format!("reporting to '{path_str}'")),
        "{text}"
    );

    let expected = server
        .tui_expect(Parameters(TuiExpectParams {
            pattern: Some("TOOLS_READY".to_string()),
            screen_mode: Some(true),
            session_id: Some("reported".to_string()),
            ..TuiExpectParams::default()
        }))
        .await
        .expect("tool call ok");
    assert!(!expected.is_error.unwrap_or(false));

    // An SVG saved to a file, then a PNG returned inline
    for (format, output_path) in [
        ("svg", Some(svg_path.to_str().unwrap().to_string())),
        ("png", None),
    ] {
        let shot = server
            .tui_take_screenshot(Parameters(TuiScreenshotParams {
                session_id: Some("reported".to_string()),
                format: Some(format.to_string()),
                output_path,
                ..TuiScreenshotParams::default()
            }))
            .await
            .expect("tool call ok");
        assert!(!shot.is_error.unwrap_or(false));
    }

    let ended = server
        .tui_end(Parameters(TuiEndParams {
            session_id: Some("reported".to_string()),
        }))
        .await
        .expect("tool call ok");
    let text = serde_json::to_string(&ended.content).unwrap();
    assert!(
        text.contains(&format!("Report '{path_str}': 1 check, 1 passed, 0 failed")),
        "{text}"
    );

    let entries = read_report(&path);
    let shots = of_type(&entries, "screenshot");
    assert_eq!(shots.len(), 2, "{entries:?}");
    assert_eq!(shots[0]["format"], "svg");
    assert_eq!(shots[0]["path"], svg_path.to_str().unwrap());
    assert_eq!(
        shots[0]["bytes"].as_u64(),
        std::fs::metadata(&svg_path).ok().map(|m| m.len())
    );
    assert_eq!(shots[1]["format"], "png");
    assert_eq!(shots[1]["path"], Value::Null);
    assert!(shots[1]["bytes"].as_u64().unwrap() > 0);
    assert_eq!(entries[0]["session_id"], "reported");
    assert_eq!(entries.last().unwrap()["type"], "summary");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&svg_path);
}

#[tokio::test]
async fn test_tool_replies_without_a_report() {
    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());
    let started = server
        .tui_start(Parameters(TuiStartParams {
            command: "cat".to_string(),
            args: Vec::new(),
            rows: Some(ROWS),
            cols: Some(COLS),
            record_path: None,
            report_path: None,
            session_id: None,
        }))
        .await
        .expect("tool call ok");
    let text = serde_json::to_string(&started.content).unwrap();
    assert!(!text.contains("reporting"), "{text}");

    let ended = server
        .tui_end(Parameters(TuiEndParams::default()))
        .await
        .expect("tool call ok");
    let text = serde_json::to_string(&ended.content).unwrap();
    assert!(text.contains("Terminated session 'default'"), "{text}");
    assert!(!text.contains("Report"), "{text}");
}

#[tokio::test]
async fn test_bad_report_path_fails_to_start() {
    let manager = PtyManager::new();
    let args = vec!["-c".to_string(), "sleep 30".to_string()];
    let config = PtyConfig::new("sh", &args, ROWS, COLS)
        .with_report_path(Some("/nonexistent-dir/shadowpty/report.jsonl"));
    let err = manager.start_app(&config).await.expect_err("bad path");
    assert!(
        format!("{err:#}").contains("failed to create report file"),
        "{err:#}"
    );
    assert!(!manager.is_active().await);
}

async fn next_event(events: &mut broadcast::Receiver<ReportEvent>) -> ReportEntry {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("event in time")
        .expect("event")
        .entry
}

#[tokio::test]
async fn test_subscribers_get_events_without_a_report_file() {
    let manager = PtyManager::new();
    start_interactive_sh(&manager, None).await;
    assert!(manager.subscribe_report_session("nope").await.is_err());
    let mut events = manager
        .subscribe_report_session("default")
        .await
        .expect("subscribe");

    manager.send_input("echo HI<ENTER>").await.expect("input");
    assert_eq!(
        next_event(&mut events).await,
        ReportEntry::Input {
            keys: "echo HI<ENTER>".to_string(),
            bytes: 8,
        }
    );

    manager
        .expect(&expectation("READY> ", ExpectTarget::Stream, 5_000))
        .await
        .expect("prompt");
    let ReportEntry::Expect {
        passed, matched, ..
    } = next_event(&mut events).await
    else {
        panic!("expected an expect entry");
    };
    assert!(passed);
    assert_eq!(matched.as_deref(), Some("READY> "));

    let stopped = manager.stop_app().await.expect("stop");
    assert_eq!(stopped.report_path, None);
    assert_eq!(stopped.totals.checks, 1);

    // A subscriber also gets the end of the session
    assert!(matches!(
        next_event(&mut events).await,
        ReportEntry::Exit { .. }
    ));
    assert!(matches!(
        next_event(&mut events).await,
        ReportEntry::Summary {
            checks: 1,
            passed: 1,
            failed: 0,
            ..
        }
    ));
}
