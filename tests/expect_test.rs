#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Integration tests for the waiting tools: expect, paste, wait-stable and run-script.

use std::time::{Duration, Instant};

use shadowpty::output::Pattern;
use shadowpty::pty_manager::{ExpectTarget, Expectation, PtyConfig, PtyManager, Script};

fn expectation(pattern: &str, target: ExpectTarget, timeout_ms: u64) -> Expectation {
    Expectation {
        pattern: Pattern::literal(pattern).unwrap(),
        target,
        timeout: Duration::from_millis(timeout_ms),
    }
}

fn stream(pattern: &str, timeout_ms: u64) -> Expectation {
    expectation(pattern, ExpectTarget::Stream, timeout_ms)
}

async fn start_sh(manager: &PtyManager, script: &str) {
    let args = vec!["-c".to_string(), script.to_string()];
    manager
        .start_app(&PtyConfig::new("sh", &args, 24, 80))
        .await
        .expect("start sh");
}

/// Starts an interactive `sh` with a distinctive prompt and waits for the first prompt.
async fn start_interactive_sh(manager: &PtyManager) {
    start_sh(manager, "export PS1='READY> '; exec sh -i").await;
    manager
        .expect(&stream("READY> ", 5_000))
        .await
        .expect("initial prompt");
}

#[tokio::test]
async fn test_expect_regex_matching() {
    let manager = PtyManager::new();
    start_sh(&manager, "printf 'STATUS_CODE_200_OK\\n'; sleep 5").await;

    let matched = manager
        .expect(&Expectation {
            pattern: Pattern::regex(r"STATUS_CODE_\d{3}_OK").unwrap(),
            target: ExpectTarget::Stream,
            timeout: Duration::from_secs(3),
        })
        .await
        .expect("regex match");
    assert_eq!(matched, "STATUS_CODE_200_OK");
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_expect_ignores_output_already_read() {
    let manager = PtyManager::new();
    manager
        .start_app(&PtyConfig::new("cat", &[], 24, 80))
        .await
        .expect("start cat");

    manager.send_input("MARKER<ENTER>").await.expect("send");
    manager
        .wait_stable(Duration::from_millis(150), Duration::from_secs(3))
        .await
        .expect("stable");
    let screen = manager.read_screen().await.expect("read");
    assert!(screen.contains("MARKER"), "{screen}");

    // The agent has already seen MARKER on screen, so it must not match again
    assert!(manager.expect(&stream("MARKER", 300)).await.is_err());

    // New output still matches
    manager
        .send_input("MARKER<ENTER>")
        .await
        .expect("send again");
    manager
        .expect(&stream("MARKER", 3_000))
        .await
        .expect("new marker");
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_expect_consumes_each_match_once() {
    let manager = PtyManager::new();
    start_sh(&manager, "printf 'ONE-TICK ONE-TICK '; sleep 5").await;

    manager.expect(&stream("TICK", 3_000)).await.expect("first");
    manager
        .expect(&stream("TICK", 3_000))
        .await
        .expect("second");
    assert!(manager.expect(&stream("TICK", 300)).await.is_err());
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_screen_mode_matches_rendered_text() {
    let manager = PtyManager::new();
    // Overwriting the first character renders "JELLO", which never appears in the raw output
    start_sh(&manager, "printf 'HELLO\\rJ'; sleep 5").await;

    manager
        .expect(&expectation("JELLO", ExpectTarget::Screen, 3_000))
        .await
        .expect("screen match");
    assert!(manager.expect(&stream("JELLO", 300)).await.is_err());
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_expect_fails_fast_when_process_exits() {
    let manager = PtyManager::new();
    start_sh(&manager, "echo bye").await;

    let started = Instant::now();
    let err = manager
        .expect(&stream("never printed", 10_000))
        .await
        .expect_err("process exits without printing the pattern");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
    let message = format!("{err:#}");
    assert!(message.contains("exited"), "{message}");
    assert!(message.contains("bye"), "{message}");
}

#[tokio::test]
async fn test_pending_expect_does_not_block_other_calls() {
    let manager = PtyManager::new();
    manager
        .start_app(&PtyConfig::new("cat", &[], 24, 80))
        .await
        .expect("start cat");

    let waiting = manager.clone();
    let pending = tokio::spawn(async move { waiting.expect(&stream("RELEASE", 5_000)).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let started = Instant::now();
    manager.send_input("HELLO<ENTER>").await.expect("send");
    manager.read_screen().await.expect("read");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "input and read were blocked for {:?}",
        started.elapsed()
    );

    manager.send_input("RELEASE<ENTER>").await.expect("release");
    let matched = pending.await.expect("join").expect("pending expect");
    assert_eq!(matched, "RELEASE");
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_bracketed_paste() {
    let manager = PtyManager::new();
    start_interactive_sh(&manager).await;

    manager
        .send_paste("echo 'LINE_ONE'\necho 'LINE_TWO'\n")
        .await
        .expect("paste");
    manager
        .expect(&expectation("LINE_TWO", ExpectTarget::Screen, 3_000))
        .await
        .expect("line two");

    let err = manager
        .send_paste("evil\x1b[201~rm -rf /")
        .await
        .expect_err("end marker must be rejected");
    assert!(format!("{err:#}").contains("end marker"));
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_wait_stable() {
    let manager = PtyManager::new();
    start_sh(
        &manager,
        "for i in 1 2 3; do echo tick $i; sleep 0.1; done; sleep 5",
    )
    .await;
    manager
        .wait_stable(Duration::from_millis(400), Duration::from_secs(3))
        .await
        .expect("output settles after the loop");
    let screen = manager.read_screen().await.expect("read");
    assert!(screen.contains("tick 3"), "{screen}");
    manager.stop_app().await.expect("stop");

    start_sh(&manager, "while :; do echo x; sleep 0.05; done").await;
    assert!(
        manager
            .wait_stable(Duration::from_millis(300), Duration::from_millis(800))
            .await
            .is_err(),
        "continuous output never settles"
    );
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_run_script_skips_echo_and_prior_output() {
    let manager = PtyManager::new();
    start_interactive_sh(&manager).await;

    let commands = vec![
        // The echo of this command contains the prompt text; it must not end the wait
        ": 'READY> '".to_string(),
        "echo second".to_string(),
        "echo $((1+2))".to_string(),
    ];
    let outcome = manager
        .run_script(&Script {
            commands: &commands,
            prompt: Pattern::literal("READY> ").unwrap(),
            timeout_per_command: Duration::from_secs(5),
        })
        .await
        .expect("run script");

    assert_eq!(outcome.error, None);
    let outputs: Vec<&str> = outcome.steps.iter().map(|s| s.output.as_str()).collect();
    assert_eq!(outputs, ["", "second", "3"]);
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_run_script_stops_when_prompt_is_missing() {
    let manager = PtyManager::new();
    start_interactive_sh(&manager).await;

    let commands = vec!["echo hi".to_string(), "echo never-run".to_string()];
    let outcome = manager
        .run_script(&Script {
            commands: &commands,
            prompt: Pattern::literal("NOT-THE-PROMPT").unwrap(),
            timeout_per_command: Duration::from_millis(500),
        })
        .await
        .expect("run script");

    assert!(outcome.steps.is_empty());
    let error = outcome.error.expect("script stops");
    assert!(error.starts_with("command 1 ('echo hi')"), "{error}");
    assert!(
        error.contains("hi"),
        "error should include unread output: {error}"
    );
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_expect_and_script_tools() {
    use rmcp::handler::server::wrapper::Parameters;
    use shadowpty::server::{ShadowPtyServer, TuiExpectParams, TuiRunScriptParams};

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());
    start_interactive_sh(&manager).await;

    let invalid = server
        .tui_expect(Parameters(TuiExpectParams {
            pattern: "([".to_string(),
            is_regex: Some(true),
            screen_mode: None,
            timeout_ms: Some(100),
            session_id: None,
        }))
        .await
        .expect("tool call ok");
    assert!(invalid.is_error.unwrap_or(false));

    let script = server
        .tui_run_script(Parameters(TuiRunScriptParams {
            commands: vec!["echo tool-output".to_string()],
            prompt_pattern: Some("READY> ".to_string()),
            is_regex: None,
            timeout_ms: Some(5_000),
            session_id: None,
        }))
        .await
        .expect("tool call ok");
    assert!(!script.is_error.unwrap_or(false));
    let text = serde_json::to_string(&script.content).expect("serialize");
    assert!(text.contains("tool-output"), "{text}");
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_unterminated_sync_frame_is_flushed() {
    let manager = PtyManager::new();
    // Opens a synchronized update (DECSET 2026) and never closes it, like an app that crashes
    // mid-frame
    let args = vec![
        "-c".to_string(),
        r"printf '\033[?2026hMID_FRAME'; sleep 5".to_string(),
    ];
    manager
        .start_app(&PtyConfig::new("sh", &args, 43, 155))
        .await
        .expect("start sh");

    let started = Instant::now();
    manager
        .expect(&expectation("MID_FRAME", ExpectTarget::Screen, 2_000))
        .await
        .expect("frame flushed to the screen");
    assert!(started.elapsed() < Duration::from_secs(1));
    manager.stop_app().await.expect("stop");
}
