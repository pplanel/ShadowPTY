#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;
use termcp::pty_manager::{PtyConfig, PtyManager};

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
    manager
        .send_input("exit<ENTER>")
        .await
        .expect("send exit");
}
