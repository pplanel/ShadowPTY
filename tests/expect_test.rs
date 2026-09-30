#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Integration tests for the waiting tools: expect, paste, wait-stable and run-script.

use std::time::{Duration, Instant};

use shadowpty::output::Pattern;
use shadowpty::pty_manager::{ExpectTarget, Expectation, PtyConfig, PtyManager, Script};

fn expectation(pattern: &str, target: ExpectTarget, timeout_ms: u64) -> Expectation {
    Expectation {
        patterns: vec![Pattern::literal(pattern).unwrap()],
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
            patterns: vec![Pattern::regex(r"STATUS_CODE_\d{3}_OK").unwrap()],
            target: ExpectTarget::Stream,
            timeout: Duration::from_secs(3),
        })
        .await
        .expect("regex match");
    assert_eq!(matched.matched, "STATUS_CODE_200_OK");
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
    assert_eq!(matched.matched, "RELEASE");
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
            pattern: Some("([".to_string()),
            patterns: None,
            syntax: None,
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
            syntax: None,
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

fn patterns(sources: &[&str], target: ExpectTarget, timeout_ms: u64) -> Expectation {
    Expectation {
        patterns: sources
            .iter()
            .map(|source| Pattern::literal(source).unwrap())
            .collect(),
        target,
        timeout: Duration::from_millis(timeout_ms),
    }
}

#[tokio::test]
async fn test_expect_first_of_several_patterns_in_stream() {
    let manager = PtyManager::new();
    start_sh(
        &manager,
        "sleep 0.2; printf 'Connecting...\\nPermission denied\\n'; sleep 5",
    )
    .await;

    let found = manager
        .expect(&patterns(
            &["Password:", "Permission denied", "Welcome"],
            ExpectTarget::Stream,
            5_000,
        ))
        .await
        .expect("one of the patterns");
    assert_eq!(found.index, 1);
    assert_eq!(found.matched, "Permission denied");
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_expect_first_of_several_patterns_on_screen() {
    let manager = PtyManager::new();
    start_sh(
        &manager,
        "printf 'Status: \\033[32mREADY\\033[0m\\n'; sleep 5",
    )
    .await;

    let found = manager
        .expect(&patterns(&["FAILED", "READY"], ExpectTarget::Screen, 5_000))
        .await
        .expect("one of the patterns");
    assert_eq!(found.index, 1);
    assert_eq!(found.matched, "READY");

    let err = manager
        .expect(&patterns(&["FAILED", "CRASHED"], ExpectTarget::Screen, 200))
        .await
        .expect_err("neither is on screen");
    assert!(
        format!("{err:#}").contains("any of 'FAILED', 'CRASHED'"),
        "{err:#}"
    );
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_expect_tool_with_several_patterns() {
    use rmcp::handler::server::wrapper::Parameters;
    use shadowpty::server::{ShadowPtyServer, TuiExpectParams};

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());
    start_sh(&manager, "printf 'Build failed: 2 errors\\n'; sleep 5").await;

    let matched = server
        .tui_expect(Parameters(TuiExpectParams {
            patterns: Some(vec![
                "Build succeeded".to_string(),
                "Build failed".to_string(),
            ]),
            timeout_ms: Some(5_000),
            ..TuiExpectParams::default()
        }))
        .await
        .expect("tool call ok");
    assert!(!matched.is_error.unwrap_or(false));
    let text = serde_json::to_string(&matched.content).expect("serialize");
    assert!(
        text.contains("Matched pattern 2 of 2 ('Build failed')"),
        "{text}"
    );

    for invalid in [
        TuiExpectParams::default(),
        TuiExpectParams {
            patterns: Some(Vec::new()),
            ..TuiExpectParams::default()
        },
        TuiExpectParams {
            pattern: Some("a".to_string()),
            patterns: Some(vec!["b".to_string()]),
            ..TuiExpectParams::default()
        },
    ] {
        let result = server
            .tui_expect(Parameters(invalid))
            .await
            .expect("tool call ok");
        assert!(result.is_error.unwrap_or(false));
    }
    manager.stop_app().await.expect("stop");
}

/// Starts `sh -c script` at the default test size (43×155).
async fn start_sh_43x155(manager: &PtyManager, script: &str) {
    let args = vec!["-c".to_string(), script.to_string()];
    manager
        .start_app(&PtyConfig::new("sh", &args, 43, 155))
        .await
        .expect("start sh");
}

fn literals(sources: &[&str]) -> Vec<Pattern> {
    sources
        .iter()
        .map(|source| Pattern::literal(source).unwrap())
        .collect()
}

#[tokio::test]
async fn test_wait_gone_returns_when_spinner_clears() {
    let manager = PtyManager::new();
    start_sh_43x155(
        &manager,
        "printf 'Loading...'; sleep 0.4; printf '\\r\\033[KDone\\n'; sleep 5",
    )
    .await;
    manager
        .expect(&expectation("Loading", ExpectTarget::Screen, 5_000))
        .await
        .expect("spinner shown");

    let elapsed = manager
        .wait_gone(&literals(&["Loading"]), Duration::from_secs(5))
        .await
        .expect("spinner cleared");
    assert!(elapsed >= Duration::from_millis(100), "{elapsed:?}");
    let screen = manager.read_screen().await.expect("read");
    assert!(
        screen.contains("Done") && !screen.contains("Loading"),
        "{screen}"
    );
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_wait_gone_is_immediate_when_text_is_absent() {
    let manager = PtyManager::new();
    start_sh_43x155(&manager, "printf 'Ready\\n'; sleep 5").await;
    manager
        .expect(&expectation("Ready", ExpectTarget::Screen, 5_000))
        .await
        .expect("ready");

    let elapsed = manager
        .wait_gone(
            &literals(&["Loading", "Please wait"]),
            Duration::from_secs(5),
        )
        .await
        .expect("nothing to wait for");
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_wait_gone_times_out_and_shows_screen() {
    let manager = PtyManager::new();
    start_sh_43x155(&manager, "printf 'Loading forever'; sleep 5").await;
    manager
        .expect(&expectation("Loading", ExpectTarget::Screen, 5_000))
        .await
        .expect("shown");

    let err = manager
        .wait_gone(&literals(&["Loading"]), Duration::from_millis(300))
        .await
        .expect_err("never clears");
    let message = format!("{err:#}");
    assert!(message.contains("'Loading' still on screen"), "{message}");
    assert!(message.contains("Loading forever"), "{message}");
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_wait_gone_fails_fast_when_process_exits_with_text() {
    let manager = PtyManager::new();
    start_sh_43x155(&manager, "printf 'Loading'; sleep 0.2").await;
    manager
        .expect(&expectation("Loading", ExpectTarget::Screen, 5_000))
        .await
        .expect("shown");

    let started = Instant::now();
    let err = manager
        .wait_gone(&literals(&["Loading"]), Duration::from_secs(10))
        .await
        .expect_err("can no longer clear");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert!(format!("{err:#}").contains("exited"), "{err:#}");
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_wait_gone_tool() {
    use rmcp::handler::server::wrapper::Parameters;
    use shadowpty::server::{ShadowPtyServer, TuiWaitGoneParams};

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());
    start_sh_43x155(
        &manager,
        "printf 'Syncing...'; sleep 0.3; printf '\\r\\033[KSynced\\n'; sleep 5",
    )
    .await;
    manager
        .expect(&expectation("Syncing", ExpectTarget::Screen, 5_000))
        .await
        .expect("shown");

    let gone = server
        .tui_wait_gone(Parameters(TuiWaitGoneParams {
            patterns: Some(vec!["Syncing".to_string(), "Loading".to_string()]),
            timeout_ms: Some(5_000),
            ..TuiWaitGoneParams::default()
        }))
        .await
        .expect("tool call ok");
    assert!(!gone.is_error.unwrap_or(false));
    let text = serde_json::to_string(&gone.content).expect("serialize");
    assert!(
        text.contains("2 patterns are no longer on screen"),
        "{text}"
    );

    let invalid = server
        .tui_wait_gone(Parameters(TuiWaitGoneParams::default()))
        .await
        .expect("tool call ok");
    assert!(invalid.is_error.unwrap_or(false));
    manager.stop_app().await.expect("stop");
}

#[tokio::test]
async fn test_glob_patterns_in_tools() {
    use rmcp::handler::server::wrapper::Parameters;
    use shadowpty::server::{
        PatternSyntax, ShadowPtyServer, TuiExpectParams, TuiRunScriptParams, TuiWaitGoneParams,
    };

    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());
    start_interactive_sh(&manager).await;

    // Stream expect: `*` stops at the end of the line
    manager
        .send_input("printf 'Build finished: 3 warnings\\nnext line\\n'<ENTER>")
        .await
        .expect("send");
    let matched = server
        .tui_expect(Parameters(TuiExpectParams {
            pattern: Some("Build*: ? warnings".to_string()),
            syntax: Some(PatternSyntax::Glob),
            timeout_ms: Some(5_000),
            ..TuiExpectParams::default()
        }))
        .await
        .expect("tool call ok");
    let text = serde_json::to_string(&matched.content).expect("serialize");
    assert!(
        !matched.is_error.unwrap_or(false) && text.contains("Build finished: 3 warnings"),
        "{text}"
    );

    // Glob prompt for run_script
    let script = server
        .tui_run_script(Parameters(TuiRunScriptParams {
            commands: vec!["echo glob-ok".to_string()],
            prompt_pattern: Some("[A-Z]*> ".to_string()),
            syntax: Some(PatternSyntax::Glob),
            is_regex: None,
            timeout_ms: Some(5_000),
            session_id: None,
        }))
        .await
        .expect("tool call ok");
    let text = serde_json::to_string(&script.content).expect("serialize");
    assert!(
        !script.is_error.unwrap_or(false) && text.contains("glob-ok"),
        "{text}"
    );

    // Glob in wait_gone: nothing like "Loading 42%" is on screen
    let gone = server
        .tui_wait_gone(Parameters(TuiWaitGoneParams {
            pattern: Some("Loading [0-9]*%".to_string()),
            syntax: Some(PatternSyntax::Glob),
            timeout_ms: Some(1_000),
            ..TuiWaitGoneParams::default()
        }))
        .await
        .expect("tool call ok");
    assert!(!gone.is_error.unwrap_or(false));

    // `syntax` and `is_regex` can't contradict each other
    let conflict = server
        .tui_expect(Parameters(TuiExpectParams {
            pattern: Some("x".to_string()),
            syntax: Some(PatternSyntax::Glob),
            is_regex: Some(true),
            timeout_ms: Some(100),
            ..TuiExpectParams::default()
        }))
        .await
        .expect("tool call ok");
    let text = serde_json::to_string(&conflict.content).expect("serialize");
    assert!(
        conflict.is_error.unwrap_or(false) && text.contains("disagree"),
        "{text}"
    );

    manager.stop_app().await.expect("stop");
}
