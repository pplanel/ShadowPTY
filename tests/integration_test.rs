#![allow(clippy::expect_used, clippy::unwrap_used)]

use shadowpty::pty_manager::{PtyConfig, PtyManager};
use std::time::Duration;

#[tokio::test]
async fn test_interactive_pty_session() {
    let manager = PtyManager::new();

    // Spawn a shell (sh)
    let args = vec!["-s".to_string()];
    let config = PtyConfig::new("sh", &args, 24, 80);
    let info = manager.start_app(&config).await.expect("start sh");
    assert!(info.pid.is_some());

    // Send command: echo "TEST_INTERACTIVE_OUTPUT"<ENTER>
    manager
        .send_input("echo \"TEST_INTERACTIVE_OUTPUT\"<ENTER>")
        .await
        .expect("send echo command");

    // Wait a short time for output to arrive in vt100 parser
    tokio::time::sleep(Duration::from_millis(200)).await;

    let screen = manager.read_screen().await.expect("read screen");
    assert!(
        screen.contains("TEST_INTERACTIVE_OUTPUT"),
        "Screen did not contain expected text. Got: {screen}"
    );

    // Test resize
    let (rows, cols) = manager.resize(30, 100).await.expect("resize");
    assert_eq!(rows, 30);
    assert_eq!(cols, 100);

    // Send exit
    manager.send_input("exit<ENTER>").await.expect("send exit");
}

#[tokio::test]
async fn test_asciicast_v3_recording() {
    let temp_dir = std::env::temp_dir();
    let cast_path = temp_dir.join(format!("shadowpty_test_{}.cast", std::process::id()));
    let cast_path_str = cast_path.to_string_lossy().to_string();

    let manager = PtyManager::new();
    let args = vec!["-s".to_string()];
    let config = PtyConfig::new("sh", &args, 24, 80).with_record_path(Some(&cast_path_str));

    let info = manager
        .start_app(&config)
        .await
        .expect("start sh with recording");
    assert!(info.pid.is_some());

    // Send input
    manager
        .send_input("echo \"RECORDED_OUTPUT\"<ENTER>")
        .await
        .expect("send input");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Resize
    let _ = manager.resize(30, 100).await.expect("resize");

    // Exit
    manager.send_input("exit<ENTER>").await.expect("exit");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Drop manager / session to trigger exit code recording
    drop(manager);

    // Read recorded .cast file
    let content = std::fs::read_to_string(&cast_path).expect("read cast file");
    let lines: Vec<&str> = content.lines().collect();
    assert!(
        lines.len() >= 4,
        "Expected at least header + events, got: {lines:?}"
    );

    // Line 1: Header validation
    let header_json: serde_json::Value = serde_json::from_str(lines[0]).expect("valid json header");
    assert_eq!(header_json["version"], 3);
    assert_eq!(header_json["term"]["cols"], 80);
    assert_eq!(header_json["term"]["rows"], 24);
    assert_eq!(header_json["term"]["type"], "xterm-256color");
    assert_eq!(header_json["command"], "sh");

    // Check presence of event codes: 'i', 'o', 'r', 'x'
    let mut has_input = false;
    let mut has_output = false;
    let mut has_resize = false;
    let mut has_exit = false;

    for line in &lines[1..] {
        let event_json: serde_json::Value = serde_json::from_str(line).expect("valid event json");
        assert!(event_json.is_array());
        let arr = event_json.as_array().unwrap();
        assert_eq!(arr.len(), 3);

        // [interval, type, data]
        assert!(arr[0].is_f64() || arr[0].is_number());
        let ev_type = arr[1].as_str().unwrap();
        let ev_data = arr[2].as_str().unwrap();

        match ev_type {
            "i" => {
                has_input = true;
                if ev_data.contains("RECORDED_OUTPUT") {
                    // Confirmed input was recorded
                }
            }
            "o" => {
                has_output = true;
            }
            "r" => {
                has_resize = true;
                assert_eq!(ev_data, "100x30");
            }
            "x" => {
                has_exit = true;
            }
            _ => {}
        }
    }

    assert!(has_input, "Missing 'i' input event");
    assert!(has_output, "Missing 'o' output event");
    assert!(has_resize, "Missing 'r' resize event");
    assert!(has_exit, "Missing 'x' exit event");

    let _ = std::fs::remove_file(&cast_path);
}

fn is_process_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[tokio::test]
async fn test_stop_app_terminates_and_reaps_process() {
    let manager = PtyManager::new();

    // Spawn a long-running process (sleep 30)
    let args = vec!["30".to_string()];
    let config = PtyConfig::new("sleep", &args, 24, 80);
    let info = manager.start_app(&config).await.expect("start sleep");
    let pid = info.pid.expect("valid pid");

    // Process should be active in PtyManager and running in the OS
    assert!(manager.is_active().await);
    assert!(is_process_running(pid), "Process {pid} should be running");

    // Terminate the session
    let stopped_info = manager.stop_app().await.expect("stop_app should succeed");
    assert_eq!(stopped_info.pid, Some(pid));

    // Session is no longer active in manager
    assert!(!manager.is_active().await);

    // Give OS a moment to finish reap
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Process must be killed AND reaped (not a zombie!)
    assert!(
        !is_process_running(pid),
        "Process {pid} should be terminated and reaped from process table"
    );

    // Subsequent call to stop_app should return error
    let err = manager.stop_app().await;
    assert!(
        err.is_err(),
        "Calling stop_app with no active session should fail"
    );
}

#[tokio::test]
async fn test_tui_end_tool() {
    use shadowpty::server::ShadowPtyServer;

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());

    // Calling tui_end with no active session returns an error CallToolResult
    let result = server.tui_end().await.expect("tool call ok");
    assert!(result.is_error.unwrap_or(false));

    // Start a session
    let args = vec!["30".to_string()];
    let config = PtyConfig::new("sleep", &args, 24, 80);
    let info = manager.start_app(&config).await.expect("start sleep");
    let pid = info.pid.expect("valid pid");

    // Process should be running
    assert!(is_process_running(pid));

    // Call tui_end
    let result = server.tui_end().await.expect("tool call ok");
    assert!(!result.is_error.unwrap_or(false));

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !is_process_running(pid),
        "Process should be reaped after tui_end"
    );
}

#[tokio::test]
async fn test_stop_app_terminates_descendant_child_processes() {
    let manager = PtyManager::new();

    // Spawn sh running a background sleep command
    let unique_sleep = "sleep 9471";
    let args = vec!["-c".to_string(), format!("{unique_sleep} & wait")];
    let config = PtyConfig::new("sh", &args, 24, 80);
    let info = manager
        .start_app(&config)
        .await
        .expect("start sh with background sleep");
    let pid = info.pid.expect("valid pid");

    // Wait for the background sleep process to start
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify background sleep process is running
    let pgrep = std::process::Command::new("pgrep")
        .args(["-f", unique_sleep])
        .output()
        .expect("pgrep");
    assert!(pgrep.status.success(), "Background sleep should be running");

    // Terminate session
    manager.stop_app().await.expect("stop_app ok");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Both leader PID and descendant sleep process must be killed
    assert!(!is_process_running(pid), "Session leader should be killed");
    let pgrep_after = std::process::Command::new("pgrep")
        .args(["-f", unique_sleep])
        .output()
        .expect("pgrep");
    assert!(
        !pgrep_after.status.success(),
        "Background sleep should also be terminated"
    );
}
