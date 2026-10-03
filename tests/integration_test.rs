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
    let stopped = manager.stop_app().await.expect("stop_app should succeed");
    assert_eq!(stopped.info.pid, Some(pid));

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
    use rmcp::handler::server::wrapper::Parameters;
    use shadowpty::server::{ShadowPtyServer, TuiEndParams};

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());

    // Calling tui_end with no active session returns an error CallToolResult
    let result = server
        .tui_end(Parameters(TuiEndParams::default()))
        .await
        .expect("tool call ok");
    assert!(result.is_error.unwrap_or(false));

    // Start a session
    let args = vec!["30".to_string()];
    let config = PtyConfig::new("sleep", &args, 24, 80);
    let info = manager.start_app(&config).await.expect("start sleep");
    let pid = info.pid.expect("valid pid");

    // Process should be running
    assert!(is_process_running(pid));

    // Call tui_end
    let result = server
        .tui_end(Parameters(TuiEndParams::default()))
        .await
        .expect("tool call ok");
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

/// Polls a session's screen until it contains `needle` or ~2s pass.
async fn wait_for_screen(manager: &PtyManager, session_id: &str, needle: &str) -> String {
    let mut screen = String::new();
    for _ in 0..40 {
        screen = manager
            .read_screen_session(session_id)
            .await
            .expect("read screen");
        if screen.contains(needle) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    screen
}

#[tokio::test]
async fn test_multi_session_isolation() {
    let manager = PtyManager::new();
    let cfg_a = PtyConfig::new("cat", &[], 24, 80);
    let cfg_b = PtyConfig::new("cat", &[], 12, 40);

    let info_a = manager
        .start_session("sess-a", &cfg_a)
        .await
        .expect("start a");
    let info_b = manager
        .start_session("sess-b", &cfg_b)
        .await
        .expect("start b");
    assert_eq!(info_a.session_id, "sess-a");
    assert_eq!(info_b.session_id, "sess-b");
    assert_ne!(info_a.pid, info_b.pid);

    manager
        .send_input_session("sess-a", "DATA_FOR_SESSION_A<ENTER>")
        .await
        .expect("send a");
    manager
        .send_input_session("sess-b", "DATA_FOR_SESSION_B<ENTER>")
        .await
        .expect("send b");

    let screen_a = wait_for_screen(&manager, "sess-a", "DATA_FOR_SESSION_A").await;
    let screen_b = wait_for_screen(&manager, "sess-b", "DATA_FOR_SESSION_B").await;
    assert!(screen_a.contains("DATA_FOR_SESSION_A"), "a: {screen_a}");
    assert!(!screen_a.contains("DATA_FOR_SESSION_B"), "a: {screen_a}");
    assert!(screen_b.contains("DATA_FOR_SESSION_B"), "b: {screen_b}");
    assert!(!screen_b.contains("DATA_FOR_SESSION_A"), "b: {screen_b}");

    // Operations on an unknown session fail without touching the others
    assert!(manager.send_input_session("nope", "x").await.is_err());
    assert!(manager.read_screen_session("nope").await.is_err());
    assert!(manager.resize_session("nope", 10, 10).await.is_err());

    let pid_a = info_a.pid.expect("pid a");
    manager.stop_session("sess-a").await.expect("stop a");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!is_process_running(pid_a), "session a should be reaped");
    assert!(manager.is_session_active("sess-b").await);
    assert!(is_process_running(info_b.pid.expect("pid b")));

    manager.stop_session("sess-b").await.expect("stop b");
}

#[tokio::test]
async fn test_start_same_session_id_replaces_and_reaps_previous() {
    let manager = PtyManager::new();
    let args = vec!["30".to_string()];
    let config = PtyConfig::new("sleep", &args, 24, 80);

    let first = manager
        .start_session("dup", &config)
        .await
        .expect("start first");
    let second = manager
        .start_session("dup", &config)
        .await
        .expect("start second");
    let first_pid = first.pid.expect("first pid");
    let second_pid = second.pid.expect("second pid");
    assert_ne!(first_pid, second_pid);

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !is_process_running(first_pid),
        "replaced session's process should be killed and reaped"
    );
    assert!(is_process_running(second_pid));
    assert_eq!(manager.list_sessions().await.len(), 1);

    manager.stop_session("dup").await.expect("stop");
}

#[tokio::test]
async fn test_session_tools_route_by_session_id() {
    use rmcp::handler::server::wrapper::Parameters;
    use shadowpty::server::{
        ShadowPtyServer, TuiEndParams, TuiListSessionsParams, TuiReadParams, TuiStartParams,
    };

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());

    let started = server
        .tui_start(Parameters(TuiStartParams {
            command: "cat".to_string(),
            args: Vec::new(),
            rows: Some(10),
            cols: Some(40),
            record_path: None,
            report_path: None,
            session_id: Some("tool-sess".to_string()),
            live: None,
        }))
        .await
        .expect("tool call ok");
    assert!(!started.is_error.unwrap_or(false));
    assert!(manager.is_session_active("tool-sess").await);
    assert!(!manager.is_active().await, "default session must not start");

    let listed = server
        .tui_list_sessions(Parameters(TuiListSessionsParams::default()))
        .await
        .expect("tool call ok");
    let listed_json = serde_json::to_string(&listed.content).expect("serialize");
    assert!(listed_json.contains("tool-sess"), "{listed_json}");

    // The default session doesn't exist, so reading it is an error
    let read_default = server
        .tui_read(Parameters(TuiReadParams::default()))
        .await
        .expect("tool call ok");
    assert!(read_default.is_error.unwrap_or(false));

    let read = server
        .tui_read(Parameters(TuiReadParams {
            session_id: Some("tool-sess".to_string()),
        }))
        .await
        .expect("tool call ok");
    assert!(!read.is_error.unwrap_or(false));

    let ended = server
        .tui_end(Parameters(TuiEndParams {
            session_id: Some("tool-sess".to_string()),
        }))
        .await
        .expect("tool call ok");
    assert!(!ended.is_error.unwrap_or(false));
    assert!(manager.list_sessions().await.is_empty());
}

#[tokio::test]
async fn test_take_screenshot_png_inline_default() {
    use base64::prelude::*;
    use rmcp::handler::server::wrapper::Parameters;
    use shadowpty::server::{ShadowPtyServer, TuiScreenshotParams};

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());

    let args = vec!["-c".to_string(), "echo 'INLINE_PNG_TEST'".to_string()];
    let config = PtyConfig::new("sh", &args, 24, 80);
    manager.start_app(&config).await.expect("start sh");

    tokio::time::sleep(Duration::from_millis(150)).await;

    // Default screenshot must be PNG and return inline MCP Image block
    let res = server
        .tui_take_screenshot(Parameters(TuiScreenshotParams::default()))
        .await
        .expect("screenshot tool call");
    assert!(!res.is_error.unwrap_or(false));
    assert_eq!(res.content.len(), 1);

    let img = res.content[0]
        .as_image()
        .expect("expected Image content block");
    assert_eq!(img.mime_type, "image/png");

    let png_bytes = BASE64_STANDARD.decode(&img.data).expect("valid base64");
    assert_eq!(
        &png_bytes[..8],
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
    );

    let cursor = std::io::Cursor::new(&png_bytes);
    let decoder = png::Decoder::new(cursor);
    let reader = decoder.read_info().expect("read png info");
    assert_eq!(reader.info().width, 80 * 9);
    assert_eq!(reader.info().height, 24 * 18);
}

#[tokio::test]
async fn test_take_screenshot_svg_inline() {
    use rmcp::handler::server::wrapper::Parameters;
    use shadowpty::server::{ShadowPtyServer, TuiScreenshotParams};

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());

    let args = vec![
        "-c".to_string(),
        "echo 'INLINE_SCREENSHOT_TEST'".to_string(),
    ];
    let config = PtyConfig::new("sh", &args, 24, 80);
    manager.start_app(&config).await.expect("start sh");

    tokio::time::sleep(Duration::from_millis(150)).await;

    let res = server
        .tui_take_screenshot(Parameters(TuiScreenshotParams {
            format: Some("svg".to_string()),
            ..Default::default()
        }))
        .await
        .expect("screenshot tool call");
    assert!(!res.is_error.unwrap_or(false));

    let content_json = serde_json::to_string(&res.content).expect("serialize");
    assert!(content_json.contains("<svg"));
    assert!(content_json.contains("INLINE_SCREENSHOT_TEST"));
}

#[tokio::test]
async fn test_take_screenshot_png_to_output_path() {
    use rmcp::handler::server::wrapper::Parameters;
    use shadowpty::server::{ShadowPtyServer, TuiScreenshotParams};

    let temp_dir = std::env::temp_dir();
    let file_path = temp_dir.join(format!("shadowpty_shot_{}.png", std::process::id()));
    let output_path = file_path.to_string_lossy().to_string();

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());

    let args = vec!["-c".to_string(), "echo 'FILE_PNG_TEST'".to_string()];
    let config = PtyConfig::new("sh", &args, 24, 80);
    manager.start_app(&config).await.expect("start sh");

    tokio::time::sleep(Duration::from_millis(150)).await;

    let res = server
        .tui_take_screenshot(Parameters(TuiScreenshotParams {
            output_path: Some(output_path.clone()),
            ..Default::default()
        }))
        .await
        .expect("screenshot tool call");
    assert!(!res.is_error.unwrap_or(false));

    let msg = serde_json::to_string(&res.content).expect("serialize");
    assert!(msg.contains("Saved PNG screenshot"));
    assert!(msg.contains("80x24 cells"));

    // Verify file content is valid PNG
    let bytes = std::fs::read(&file_path).expect("read written file");
    assert_eq!(
        &bytes[..8],
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
    );

    let _ = std::fs::remove_file(&file_path);
}

#[tokio::test]
async fn test_take_screenshot_svg_to_output_path() {
    use rmcp::handler::server::wrapper::Parameters;
    use shadowpty::server::{ShadowPtyServer, TuiScreenshotParams};

    let temp_dir = std::env::temp_dir();
    let file_path = temp_dir.join(format!("shadowpty_shot_{}.svg", std::process::id()));
    let output_path = file_path.to_string_lossy().to_string();

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());

    let args = vec!["-c".to_string(), "echo 'FILE_SCREENSHOT_TEST'".to_string()];
    let config = PtyConfig::new("sh", &args, 24, 80);
    manager.start_app(&config).await.expect("start sh");

    tokio::time::sleep(Duration::from_millis(150)).await;

    let res = server
        .tui_take_screenshot(Parameters(TuiScreenshotParams {
            format: Some("svg".to_string()),
            output_path: Some(output_path.clone()),
            ..Default::default()
        }))
        .await
        .expect("screenshot tool call");
    assert!(!res.is_error.unwrap_or(false));

    let msg = serde_json::to_string(&res.content).expect("serialize");
    assert!(msg.contains("Saved SVG screenshot"));
    assert!(msg.contains("80x24 cells"));

    // Verify file content
    let content = std::fs::read_to_string(&file_path).expect("read written file");
    assert!(content.contains("<svg"));
    assert!(content.contains("FILE_SCREENSHOT_TEST"));

    let _ = std::fs::remove_file(&file_path);
}

#[tokio::test]
async fn test_take_screenshot_path_validation() {
    use rmcp::handler::server::wrapper::Parameters;
    use shadowpty::server::{ShadowPtyServer, TuiScreenshotParams};

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());

    let args = vec!["30".to_string()];
    let config = PtyConfig::new("sleep", &args, 24, 80);
    manager.start_app(&config).await.expect("start sleep");

    // Relative path should fail
    let rel_res = server
        .tui_take_screenshot(Parameters(TuiScreenshotParams {
            output_path: Some("relative/path.svg".to_string()),
            ..Default::default()
        }))
        .await
        .expect("tool call");
    assert!(rel_res.is_error.unwrap_or(false));
    let err_msg = serde_json::to_string(&rel_res.content).expect("serialize");
    assert!(err_msg.contains("must be an absolute path"));

    // Non-existent directory should fail
    let bad_dir_res = server
        .tui_take_screenshot(Parameters(TuiScreenshotParams {
            output_path: Some("/nonexistent_dir_93817/shot.svg".to_string()),
            ..Default::default()
        }))
        .await
        .expect("tool call");
    assert!(bad_dir_res.is_error.unwrap_or(false));
    let err_msg = serde_json::to_string(&bad_dir_res.content).expect("serialize");
    assert!(err_msg.contains("Parent directory does not exist"));

    // Unknown session should fail
    let unknown_sess = server
        .tui_take_screenshot(Parameters(TuiScreenshotParams {
            session_id: Some("unknown-session".to_string()),
            ..Default::default()
        }))
        .await
        .expect("tool call");
    assert!(unknown_sess.is_error.unwrap_or(false));
}
