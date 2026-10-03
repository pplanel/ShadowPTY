#![allow(clippy::expect_used, clippy::unwrap_used)]

//! End-to-end tests for asking the person how to capture sessions: a real MCP client, connected
//! over an in-memory pipe, answers (or can't answer) the form `tui_start` sends. Each test runs
//! with both lifecycles: the legacy `initialize` handshake, where the server sends the form during
//! the call, and `server/discover` (protocol 2026-07-28), where `tui_start` returns the form as an
//! input request and the client retries the call with the answer.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, ElicitRequestParams,
    ElicitResult, ElicitationAction, Implementation, ProtocolVersion,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt, ErrorData, RoleClient, ServiceExt,
};
use serde_json::{Value, json};
use shadowpty::pty_manager::PtyManager;
use shadowpty::server::ShadowPtyServer;

/// A client that answers every form with `answer`, or can't show forms when it's `None`.
#[derive(Clone)]
struct Person {
    answer: Option<Value>,
    asked: Arc<AtomicUsize>,
}

impl ClientHandler for Person {
    fn get_info(&self) -> ClientConfig {
        let capabilities = if self.answer.is_some() {
            ClientCapabilities::builder().enable_elicitation().build()
        } else {
            ClientCapabilities::default()
        };
        ClientConfig::new(capabilities, Implementation::new("person", "0"))
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let ElicitRequestParams::FormElicitationParams {
            requested_schema, ..
        } = request
        else {
            return Ok(ElicitResult::new(ElicitationAction::Decline));
        };
        let schema = serde_json::to_value(&requested_schema).unwrap();
        for field in [
            "record",
            "record_path",
            "report",
            "report_path",
            "live",
            "open_browser",
        ] {
            assert!(
                schema["properties"].get(field).is_some(),
                "{field}: {schema}"
            );
        }
        let answer = self.answer.clone().unwrap();
        Ok(ElicitResult::new(ElicitationAction::Accept).with_content(answer))
    }
}

fn lifecycles() -> [ClientLifecycleMode; 2] {
    [
        ClientLifecycleMode::Initialize,
        ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
    ]
}

async fn connect(
    person: Person,
    lifecycle: ClientLifecycleMode,
) -> (RunningService<RoleClient, Person>, PtyManager) {
    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone());
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let service = server.serve(server_io).await.expect("serve");
        let _ = service.waiting().await;
    });
    let client = person
        .serve_with_lifecycle(client_io, lifecycle)
        .await
        .expect("connect");
    (client, manager)
}

async fn start(client: &RunningService<RoleClient, Person>, session_id: &str) -> String {
    let args =
        json!({"command": "sh", "args": ["-c", "echo hi; sleep 30"], "session_id": session_id});
    let result: CallToolResult = client
        .call_tool(
            CallToolRequestParams::new("tui_start")
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .expect("call tui_start");
    let content = serde_json::to_value(&result.content).unwrap();
    let text = content[0]["text"].as_str().unwrap().to_string();
    assert!(!result.is_error.unwrap_or(false), "{text}");
    text
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("shadowpty_capture_{}_{name}", std::process::id()))
}

#[tokio::test]
async fn test_person_is_asked_once_and_answer_applies_to_later_sessions() {
    for (n, lifecycle) in lifecycles().into_iter().enumerate() {
        let dir = temp(&format!("asked{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        let record = dir.join("app.cast");
        let asked = Arc::new(AtomicUsize::new(0));
        let person = Person {
            answer: Some(json!({"record": true, "record_path": record, "live": false})),
            asked: asked.clone(),
        };
        let (client, manager) = connect(person, lifecycle).await;

        let first = start(&client, "one").await;
        assert!(
            first.contains(&format!("recording to '{}'", record.display())),
            "{first}"
        );
        assert!(!first.contains("Ask the person"), "{first}");

        let second = start(&client, "two").await;
        let second_record = dir.join("app-2.cast");
        assert!(
            second.contains(&format!("recording to '{}'", second_record.display())),
            "{second}"
        );
        assert_eq!(asked.load(Ordering::SeqCst), 1, "asked once per server");

        manager.stop_session("one").await.unwrap();
        manager.stop_session("two").await.unwrap();
        assert!(record.exists() && second_record.exists());
        let _ = client.cancel().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[tokio::test]
async fn test_client_without_forms_gets_a_hint_once() {
    for lifecycle in lifecycles() {
        let asked = Arc::new(AtomicUsize::new(0));
        let person = Person {
            answer: None,
            asked: asked.clone(),
        };
        let (client, manager) = connect(person, lifecycle).await;

        let first = start(&client, "one").await;
        assert!(
            first.contains("Ask the person whether they want"),
            "{first}"
        );
        assert!(!first.contains("recording to"), "{first}");
        let second = start(&client, "two").await;
        assert!(!second.contains("Ask the person"), "{second}");
        assert_eq!(asked.load(Ordering::SeqCst), 0);

        manager.stop_session("one").await.unwrap();
        manager.stop_session("two").await.unwrap();
        let _ = client.cancel().await;
    }
}
