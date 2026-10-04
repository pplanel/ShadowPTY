#![allow(clippy::expect_used, clippy::unwrap_used)]

//! End-to-end tests for asking the person how to capture sessions: a real MCP client, connected
//! over an in-memory pipe, answers (or can't answer) the form `tui_start` sends. Each test runs
//! with both lifecycles: the legacy `initialize` handshake, where the server sends the form during
//! the call, and `server/discover` (protocol 2026-07-28), where `tui_start` returns the form as an
//! input request and the client retries the call with the answer.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
    ElicitRequestParams, ElicitResult, ElicitationAction, Implementation, InputRequiredResult,
    ProtocolVersion,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt, ErrorData, RoleClient, ServiceExt,
};
use serde_json::{Value, json};
use shadowpty::pty_manager::PtyManager;
use shadowpty::server::ShadowPtyServer;
use tokio::sync::Notify;

/// How the person answers the form.
#[derive(Clone)]
enum Reply {
    /// The client can't show forms.
    NoForms,
    Accept(Value),
    Decline,
    /// The person closes the form.
    Cancel,
}

/// A client that answers every form with `reply`. With `hold`, it waits to be released before
/// answering, as a person taking their time would.
#[derive(Clone)]
struct Person {
    reply: Reply,
    asked: Arc<AtomicUsize>,
    hold: Option<Arc<Notify>>,
}

impl Person {
    fn new(reply: Reply) -> Self {
        Self {
            reply,
            asked: Arc::new(AtomicUsize::new(0)),
            hold: None,
        }
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

impl ClientHandler for Person {
    fn get_info(&self) -> ClientConfig {
        let capabilities = if matches!(self.reply, Reply::NoForms) {
            ClientCapabilities::default()
        } else {
            ClientCapabilities::builder().enable_elicitation().build()
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
        if let Some(hold) = &self.hold {
            hold.notified().await;
        }
        Ok(match &self.reply {
            Reply::Accept(answer) => {
                ElicitResult::new(ElicitationAction::Accept).with_content(answer.clone())
            }
            Reply::Decline | Reply::NoForms => ElicitResult::new(ElicitationAction::Decline),
            Reply::Cancel => ElicitResult::new(ElicitationAction::Cancel),
        })
    }
}

const fn initialize() -> ClientLifecycleMode {
    ClientLifecycleMode::Initialize
}

fn discover() -> ClientLifecycleMode {
    ClientLifecycleMode::Discover {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
    }
}

fn lifecycles() -> [ClientLifecycleMode; 2] {
    [initialize(), discover()]
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

/// `tui_start` arguments for a long-running session, plus `extra` fields.
fn start_params(session_id: &str, extra: &Value) -> CallToolRequestParams {
    let mut args =
        json!({"command": "sh", "args": ["-c", "echo hi; sleep 30"], "session_id": session_id});
    if let (Some(args), Some(extra)) = (args.as_object_mut(), extra.as_object()) {
        args.extend(extra.clone());
    }
    CallToolRequestParams::new("tui_start").with_arguments(args.as_object().unwrap().clone())
}

fn text_of(result: &CallToolResult) -> String {
    let content = serde_json::to_value(&result.content).unwrap();
    let text = content[0]["text"].as_str().unwrap().to_string();
    assert!(!result.is_error.unwrap_or(false), "{text}");
    text
}

async fn start(client: &RunningService<RoleClient, Person>, session_id: &str) -> String {
    let result = client
        .call_tool(start_params(session_id, &json!({})))
        .await
        .expect("call tui_start");
    text_of(&result)
}

/// One `tools/call` round, without answering any form the server returns.
async fn start_once(
    client: &RunningService<RoleClient, Person>,
    params: CallToolRequestParams,
) -> CallToolResponse {
    client.call_tool_once(params).await.expect("call tui_start")
}

/// The tool result, when the server didn't ask for input.
fn complete(response: CallToolResponse) -> Option<CallToolResult> {
    match response {
        CallToolResponse::Complete(result) => Some(result),
        _ => None,
    }
}

/// The input request, when the server returned the form.
fn input_required(response: CallToolResponse) -> Option<InputRequiredResult> {
    match response {
        CallToolResponse::InputRequired(result) => Some(result),
        _ => None,
    }
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("shadowpty_capture_{}_{name}", std::process::id()))
}

const HINT: &str = "Ask the person whether they want";

#[tokio::test]
async fn test_person_is_asked_once_and_answer_applies_to_later_sessions() {
    for (n, lifecycle) in lifecycles().into_iter().enumerate() {
        let dir = temp(&format!("asked{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        let record = dir.join("app.cast");
        let person = Person::new(Reply::Accept(
            json!({"record": true, "record_path": record, "live": false}),
        ));
        let (client, manager) = connect(person.clone(), lifecycle).await;

        let first = start(&client, "one").await;
        assert!(
            first.contains(&format!("recording to '{}'", record.display())),
            "{first}"
        );
        assert!(!first.contains(HINT), "{first}");

        let second = start(&client, "two").await;
        let second_record = dir.join("app-2.cast");
        assert!(
            second.contains(&format!("recording to '{}'", second_record.display())),
            "{second}"
        );
        assert_eq!(person.asked(), 1, "asked once per server");

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
        let person = Person::new(Reply::NoForms);
        let (client, manager) = connect(person.clone(), lifecycle).await;

        let first = start(&client, "one").await;
        assert!(first.contains(HINT), "{first}");
        assert!(!first.contains("recording to"), "{first}");
        let second = start(&client, "two").await;
        assert!(!second.contains(HINT), "{second}");
        assert_eq!(person.asked(), 0);

        manager.stop_session("one").await.unwrap();
        manager.stop_session("two").await.unwrap();
        let _ = client.cancel().await;
    }
}

#[tokio::test]
async fn test_declined_captures_nothing_and_gives_no_hint() {
    for lifecycle in lifecycles() {
        let person = Person::new(Reply::Decline);
        let (client, manager) = connect(person.clone(), lifecycle).await;

        for session in ["one", "two"] {
            let text = start(&client, session).await;
            assert!(!text.contains("recording to"), "{text}");
            assert!(!text.contains("reporting to"), "{text}");
            assert!(!text.contains("watch live"), "{text}");
            assert!(!text.contains(HINT), "{text}");
        }
        assert_eq!(person.asked(), 1, "a declined form isn't shown again");

        manager.stop_session("one").await.unwrap();
        manager.stop_session("two").await.unwrap();
        let _ = client.cancel().await;
    }
}

#[tokio::test]
async fn test_closed_form_gives_a_hint_once() {
    for lifecycle in lifecycles() {
        let person = Person::new(Reply::Cancel);
        let (client, manager) = connect(person.clone(), lifecycle).await;

        let first = start(&client, "one").await;
        assert!(first.contains(HINT), "{first}");
        assert!(!first.contains("recording to"), "{first}");
        let second = start(&client, "two").await;
        assert!(!second.contains(HINT), "{second}");
        assert_eq!(person.asked(), 1, "a closed form isn't shown again");

        manager.stop_session("one").await.unwrap();
        manager.stop_session("two").await.unwrap();
        let _ = client.cancel().await;
    }
}

#[tokio::test]
async fn test_retry_without_an_answer_does_not_send_the_form_again() {
    let person = Person::new(Reply::Accept(json!({"record": true})));
    let (client, manager) = connect(person.clone(), discover()).await;

    let form = input_required(start_once(&client, start_params("one", &json!({}))).await)
        .expect("the capture form");
    let state = form.request_state.clone().expect("request state");

    // A client that sends the form back while it is out doesn't get a second one
    let other = complete(start_once(&client, start_params("other", &json!({}))).await)
        .expect("the form was sent twice");
    let other = text_of(&other);
    assert!(
        !other.contains("recording to") && !other.contains(HINT),
        "{other}"
    );

    // Coming back with the request state but no answer means the form was closed
    let mut retry = start_params("one", &json!({}));
    retry.request_state = Some(state);
    let first = complete(start_once(&client, retry).await)
        .expect("the form was sent again after the retry");
    let first = text_of(&first);
    assert!(first.contains(HINT), "{first}");
    assert!(!first.contains("recording to"), "{first}");

    let later = start(&client, "two").await;
    assert!(!later.contains(HINT), "{later}");
    assert_eq!(person.asked(), 0, "the form was never shown");

    for session in ["one", "other", "two"] {
        manager.stop_session(session).await.unwrap();
    }
    let _ = client.cancel().await;
}

#[tokio::test]
async fn test_explicit_start_is_not_blocked_while_the_person_answers() {
    let dir = temp("held");
    std::fs::create_dir_all(&dir).unwrap();
    let record = dir.join("app.cast");
    let hold = Arc::new(Notify::new());
    let person = Person {
        hold: Some(hold.clone()),
        ..Person::new(Reply::Accept(
            json!({"record": true, "record_path": record, "live": false}),
        ))
    };
    let (client, manager) = connect(person.clone(), initialize()).await;

    let peer = client.peer().clone();
    let asking = tokio::spawn(async move {
        peer.call_tool_once(start_params("one", &json!({})))
            .await
            .expect("call tui_start")
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while person.asked() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the person is asked");

    // A start that needs the answer waits for it instead of asking again
    let peer = client.peer().clone();
    let waiting = tokio::spawn(async move {
        peer.call_tool_once(start_params("two", &json!({})))
            .await
            .expect("call tui_start")
    });

    // A start that names its own capture doesn't wait for the person
    let explicit = tokio::time::timeout(
        Duration::from_secs(10),
        client.call_tool(start_params("explicit", &json!({"live": false}))),
    )
    .await
    .expect("explicit start blocked by the pending form")
    .expect("call tui_start");
    let explicit = text_of(&explicit);
    assert!(!explicit.contains("recording to"), "{explicit}");
    assert!(
        !waiting.is_finished(),
        "start without capture didn't wait for the answer"
    );

    hold.notify_one();
    let first = complete(asking.await.unwrap()).expect("first start");
    let second = complete(waiting.await.unwrap()).expect("second start");
    let first = text_of(&first);
    let second = text_of(&second);
    assert!(
        first.contains(&format!("recording to '{}'", record.display())),
        "{first}"
    );
    assert!(
        second.contains(&format!(
            "recording to '{}'",
            dir.join("app-2.cast").display()
        )),
        "{second}"
    );
    assert_eq!(person.asked(), 1, "asked once");

    for session in ["one", "two", "explicit"] {
        manager.stop_session(session).await.unwrap();
    }
    let _ = client.cancel().await;
    let _ = std::fs::remove_dir_all(&dir);
}
