#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Integration tests for exit status: `tui_wait_exit`, `exit_status` in session listings, and
//! the exit event in recordings.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rmcp::handler::server::wrapper::Parameters;
use shadowpty::pty_manager::{ExitStatus, PtyConfig, PtyManager};
use shadowpty::server::{ShadowPtyServer, TuiListSessionsParams, TuiWaitExitParams};

const ROWS: u16 = 43;
const COLS: u16 = 155;

async fn start_sh(manager: &PtyManager, script: &str, record_path: Option<&str>) {
    let args = vec!["-c".to_string(), script.to_string()];
    manager
        .start_app(&PtyConfig::new("sh", &args, ROWS, COLS).with_record_path(record_path))
        .await
        .expect("start sh");
}

fn cast_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("shadowpty_{name}_{}.cast", std::process::id()))
}

/// The `"x"` events in an asciicast file, as their data strings.
fn exit_events(path: &PathBuf) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("read cast")
        .lines()
        .skip(1)
        .filter_map(|line| serde_json::from_str::<(f64, String, String)>(line).ok())
        .filter(|(_, kind, _)| kind == "x")
        .map(|(_, _, data)| data)
        .collect()
}

#[tokio::test]
async fn test_wait_exit_reports_code_and_unread_output() {
    let manager = PtyManager::new();
    start_sh(&manager, "printf 'BYE\\n'; exit 3", None).await;

    let exit = manager
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("wait exit");
    assert_eq!(exit.status, ExitStatus::Code(3));
    assert!(exit.output.contains("BYE"), "{exit:?}");

    // The output was returned, so it counts as read
    let again = manager
        .wait_exit(Duration::from_secs(1))
        .await
        .expect("wait exit again");
    assert_eq!(again.status, ExitStatus::Code(3));
    assert!(again.output.is_empty(), "{again:?}");

    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_wait_exit_reports_signal() {
    let manager = PtyManager::new();
    start_sh(&manager, "kill -KILL $$", None).await;

    let exit = manager
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("wait exit");
    assert_eq!(exit.status, ExitStatus::Signal(9));
    assert_eq!(exit.status.to_string(), "killed by signal 9 (SIGKILL)");

    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_wait_exit_times_out_while_running() {
    let manager = PtyManager::new();
    start_sh(&manager, "printf 'STILL_HERE\\n'; sleep 30", None).await;

    let started = Instant::now();
    let err = manager
        .wait_exit(Duration::from_millis(300))
        .await
        .expect_err("still running");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(format!("{err:#}").contains("still running"), "{err:#}");

    let sessions = manager.list_sessions().await;
    assert_eq!(sessions[0].exit_status, None);

    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_recording_ends_with_real_exit_code() {
    let path = cast_path("exit_code");
    let manager = PtyManager::new();
    start_sh(&manager, "printf 'OUT\\n'; exit 7", path.to_str()).await;

    manager
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("wait exit");
    manager.stop_app().await.expect("stop");

    assert_eq!(exit_events(&path), vec!["7".to_string()]);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_recording_of_ended_session_logs_the_kill() {
    let path = cast_path("killed");
    let manager = PtyManager::new();
    start_sh(&manager, "sleep 30", path.to_str()).await;
    manager.stop_app().await.expect("stop");

    // The reader thread writes the exit event once the killed child is gone
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = exit_events(&path);
    while events.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        events = exit_events(&path);
    }
    // SIGKILL (9) recorded as 128 + 9, as shells report it
    assert_eq!(events, vec!["137".to_string()]);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn test_wait_exit_and_list_sessions_tools() {
    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());
    start_sh(&manager, "printf 'DONE\\n'; exit 2", None).await;

    let waited = server
        .tui_wait_exit(Parameters(TuiWaitExitParams::default()))
        .await
        .expect("tool call ok");
    assert!(!waited.is_error.unwrap_or(false));
    let text = serde_json::to_string(&waited.content).expect("serialize");
    assert!(text.contains("exited with code 2"), "{text}");
    assert!(text.contains("DONE"), "{text}");

    let listed = server
        .tui_list_sessions(Parameters(TuiListSessionsParams::default()))
        .await
        .expect("tool call ok");
    let content = serde_json::to_value(&listed.content).expect("serialize");
    let sessions: serde_json::Value =
        serde_json::from_str(content[0]["text"].as_str().expect("text")).expect("json");
    assert_eq!(
        sessions[0]["exit_status"],
        serde_json::json!({ "exit_code": 2 }),
        "{sessions}"
    );

    let missing = server
        .tui_wait_exit(Parameters(TuiWaitExitParams {
            session_id: Some("nope".to_string()),
            ..TuiWaitExitParams::default()
        }))
        .await
        .expect("tool call ok");
    assert!(missing.is_error.unwrap_or(false));

    manager.stop_app().await.expect("stop");
}
