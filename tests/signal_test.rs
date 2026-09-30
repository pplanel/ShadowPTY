#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Integration tests for `tui_signal`: signals reach the right processes and leave the session
//! open.

use std::path::PathBuf;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use shadowpty::output::Pattern;
use shadowpty::pty_manager::{
    ExitStatus, ExpectTarget, Expectation, PtyConfig, PtyManager, SignalTarget,
};
use shadowpty::server::{ShadowPtyServer, SignalTargetParam, TuiSignalParams};
use shadowpty::signals::Signal;

const ROWS: u16 = 43;
const COLS: u16 = 155;

fn stream(pattern: &str, timeout_ms: u64) -> Expectation {
    Expectation::new(
        Pattern::literal(pattern).unwrap(),
        ExpectTarget::Stream,
        Duration::from_millis(timeout_ms),
    )
}

async fn start_sh(manager: &PtyManager, script: &str, record_path: Option<&str>) {
    let args = vec!["-c".to_string(), script.to_string()];
    manager
        .start_app(&PtyConfig::new("sh", &args, ROWS, COLS).with_record_path(record_path))
        .await
        .expect("start sh");
}

/// A loop that reports each signal it traps and keeps running.
const TRAPPING_LOOP: &str = "trap 'echo GOT_INT' INT; trap 'echo GOT_HUP' HUP; \
     echo READY; while :; do sleep 0.1; done";

#[tokio::test]
async fn test_trapped_signal_leaves_process_running() {
    let manager = PtyManager::new();
    start_sh(&manager, TRAPPING_LOOP, None).await;
    manager
        .expect(&stream("READY", 5_000))
        .await
        .expect("ready");

    let delivery = manager.signal(Signal::INT).await.expect("signal");
    assert_eq!(delivery.target, SignalTarget::Foreground);
    manager
        .expect(&stream("GOT_INT", 5_000))
        .await
        .expect("trap ran");

    // Still running, and still answers signals
    assert_eq!(manager.list_sessions().await[0].exit_status, None);
    manager
        .signal_session("default", Signal::HUP, SignalTarget::Process)
        .await
        .expect("signal process");
    manager
        .expect(&stream("GOT_HUP", 5_000))
        .await
        .expect("second trap ran");

    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_term_ends_the_process() {
    let manager = PtyManager::new();
    start_sh(&manager, "echo READY; exec sleep 30", None).await;
    manager
        .expect(&stream("READY", 5_000))
        .await
        .expect("ready");

    manager.signal(Signal::TERM).await.expect("signal");
    let exit = manager
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("exited");
    assert_eq!(exit.status, ExitStatus::Signal(Signal::TERM.as_raw()));

    let err = manager
        .signal(Signal::TERM)
        .await
        .expect_err("already exited");
    assert!(
        format!("{err:#}").contains("no longer running (killed by signal"),
        "{err:#}"
    );

    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_crash_signal_is_reported_by_name() {
    let manager = PtyManager::new();
    start_sh(&manager, "echo READY; exec sleep 30", None).await;
    manager
        .expect(&stream("READY", 5_000))
        .await
        .expect("ready");

    manager.signal(Signal::TRAP).await.expect("signal");
    let exit = manager
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("exited");
    assert_eq!(exit.status, ExitStatus::Signal(Signal::TRAP.as_raw()));
    assert!(
        exit.status.to_string().ends_with("(SIGTRAP)"),
        "{}",
        exit.status
    );

    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_stop_and_cont_pause_and_resume_output() {
    let manager = PtyManager::new();
    start_sh(
        &manager,
        "i=0; while :; do i=$((i+1)); echo TICK$i; sleep 0.05; done",
        None,
    )
    .await;
    manager
        .expect(&stream("TICK3", 5_000))
        .await
        .expect("ticking");

    manager.signal(Signal::STOP).await.expect("stop signal");
    manager
        .wait_stable(Duration::from_millis(400), Duration::from_secs(3))
        .await
        .expect("output pauses");
    // A stopped process is still alive (macOS reports stops to the exit watcher too)
    assert_eq!(manager.list_sessions().await[0].exit_status, None);
    manager.read_screen().await.expect("read marks output seen");

    manager.signal(Signal::CONT).await.expect("cont signal");
    manager
        .expect(&stream("TICK", 3_000))
        .await
        .expect("output resumes");

    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_foreground_signal_reaches_the_job_not_the_shell() {
    let manager = PtyManager::new();
    start_sh(&manager, "export PS1='READY> '; exec sh -i", None).await;
    manager
        .expect(&stream("READY> ", 5_000))
        .await
        .expect("prompt");

    manager
        .send_input("sleep 30<ENTER>")
        .await
        .expect("start job");
    manager
        .wait_stable(Duration::from_millis(300), Duration::from_secs(3))
        .await
        .expect("job running");

    // Interrupts `sleep` (the foreground job); the shell survives and takes the next command
    manager.signal(Signal::INT).await.expect("signal");
    manager
        .send_input("echo ALIVE_$((40+2))<ENTER>")
        .await
        .expect("input");
    manager
        .expect(&stream("ALIVE_42", 5_000))
        .await
        .expect("shell answers right away");
    assert_eq!(manager.list_sessions().await[0].exit_status, None);

    manager.stop_app().await.expect("stop");
}

fn cast_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("shadowpty_{name}_{}.cast", std::process::id()))
}

#[tokio::test]
async fn test_signal_tool_and_recording_marker() {
    let path = cast_path("signal_marker");
    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());
    start_sh(&manager, TRAPPING_LOOP, path.to_str()).await;
    manager
        .expect(&stream("READY", 5_000))
        .await
        .expect("ready");

    let sent = server
        .tui_signal(Parameters(TuiSignalParams {
            signal: "sigint".to_string(),
            target: None,
            session_id: None,
        }))
        .await
        .expect("tool call ok");
    assert!(!sent.is_error.unwrap_or(false));
    let text = serde_json::to_string(&sent.content).expect("serialize");
    assert!(
        text.contains("Sent SIGINT to the foreground process group"),
        "{text}"
    );
    manager
        .expect(&stream("GOT_INT", 5_000))
        .await
        .expect("trap ran");

    let sent = server
        .tui_signal(Parameters(TuiSignalParams {
            signal: "HUP".to_string(),
            target: Some(SignalTargetParam::Process),
            session_id: None,
        }))
        .await
        .expect("tool call ok");
    let text = serde_json::to_string(&sent.content).expect("serialize");
    assert!(text.contains("Sent SIGHUP to process "), "{text}");

    let unknown = server
        .tui_signal(Parameters(TuiSignalParams {
            signal: "SIGFOO".to_string(),
            target: None,
            session_id: None,
        }))
        .await
        .expect("tool call ok");
    assert!(unknown.is_error.unwrap_or(false));
    let text = serde_json::to_string(&unknown.content).expect("serialize");
    assert!(text.contains("unknown signal 'SIGFOO'"), "{text}");

    manager.stop_app().await.expect("stop");

    let markers: Vec<String> = std::fs::read_to_string(&path)
        .expect("read cast")
        .lines()
        .skip(1)
        .filter_map(|line| serde_json::from_str::<(f64, String, String)>(line).ok())
        .filter(|(_, kind, _)| kind == "m")
        .map(|(_, _, label)| label)
        .collect();
    assert_eq!(markers, ["SIGINT", "SIGHUP"]);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_signals_are_in_the_session_report() {
    let path = std::env::temp_dir().join(format!(
        "shadowpty_signal_report_{}.jsonl",
        std::process::id()
    ));
    let manager = PtyManager::new();
    let args = vec!["-c".to_string(), "echo READY; exec sleep 30".to_string()];
    manager
        .start_app(&PtyConfig::new("sh", &args, ROWS, COLS).with_report_path(path.to_str()))
        .await
        .expect("start sh");
    manager
        .expect(&stream("READY", 5_000))
        .await
        .expect("ready");

    let delivery = manager.signal(Signal::TERM).await.expect("signal");
    manager
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("exited");
    manager.stop_app().await.expect("stop");

    let entries: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .expect("read report")
        .lines()
        .map(|line| serde_json::from_str(line).expect("json line"))
        .collect();
    let kinds: Vec<&str> = entries
        .iter()
        .map(|entry| entry["type"].as_str().expect("type"))
        .collect();
    // `exit` is written by the reader when the process dies, so it can come before or after
    // the `wait_exit` check that saw it
    assert_eq!(kinds[..3], ["start", "expect", "signal"], "{kinds:?}");
    let mut rest = kinds[3..5].to_vec();
    rest.sort_unstable();
    assert_eq!(rest, ["exit", "wait_exit"], "{kinds:?}");
    assert_eq!(kinds[5..], ["summary"], "{kinds:?}");
    assert_eq!(entries[2]["signal"], "SIGTERM");
    assert_eq!(entries[2]["target"], "foreground");
    assert_eq!(entries[2]["id"], delivery.id);
    // Sending a signal isn't a check
    assert_eq!(entries[5]["checks"], 2);
    let _ = std::fs::remove_file(&path);
}
