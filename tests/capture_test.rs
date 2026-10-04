#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

//! End-to-end tests for how sessions are captured when the agent leaves it to the person: a
//! real MCP client, connected over an in-memory pipe, answers (or doesn't) the capture form.
//!
//! `tui_start` never waits for the person. Until they decide, a session records and writes its
//! report to the default files. With the legacy `initialize` lifecycle the form goes out in the
//! background as the session starts; with `server/discover` (protocol 2026-07-28) `tui_end`
//! returns it as an input request and the client retries the call with the answer.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
    ElicitRequestParams, ElicitResult, ElicitationAction, Implementation, ProtocolVersion,
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
    /// The fields of the last form shown.
    fields: Arc<std::sync::Mutex<Vec<String>>>,
    hold: Option<Arc<Notify>>,
}

impl Person {
    fn new(reply: Reply) -> Self {
        Self {
            reply,
            asked: Arc::new(AtomicUsize::new(0)),
            fields: Arc::default(),
            hold: None,
        }
    }

    fn holding(reply: Reply, hold: &Arc<Notify>) -> Self {
        Self {
            hold: Some(Arc::clone(hold)),
            ..Self::new(reply)
        }
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }

    fn offered(&self, field: &str) -> bool {
        self.fields.lock().unwrap().iter().any(|f| f == field)
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
        let fields: Vec<String> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for field in ["record", "record_path", "report", "report_path"] {
            assert!(fields.iter().any(|f| f == field), "{field}: {schema}");
        }
        *self.fields.lock().unwrap() = fields;
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

type Client = RunningService<RoleClient, Person>;

/// A server whose default files go to `dir`, with `timeout` to answer a background form.
async fn connect(
    person: Person,
    lifecycle: ClientLifecycleMode,
    dir: &Path,
    timeout: Duration,
) -> (Client, PtyManager) {
    let manager = PtyManager::new();
    let server = ShadowPtyServer::new(manager.clone()).with_capture(dir.to_path_buf(), timeout);
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

/// A fresh directory for the default files.
fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shadowpty_capture_{}_{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn path(dir: &Path, file: &str) -> String {
    dir.join(file).to_string_lossy().into_owned()
}

/// `tui_start` arguments for a long-running `sh` session, plus `extra` fields.
fn start_params(session_id: &str, extra: &Value) -> CallToolRequestParams {
    let mut args =
        json!({"command": "sh", "args": ["-c", "echo hi; sleep 30"], "session_id": session_id});
    if let (Some(args), Some(extra)) = (args.as_object_mut(), extra.as_object()) {
        args.extend(extra.clone());
    }
    CallToolRequestParams::new("tui_start").with_arguments(args.as_object().unwrap().clone())
}

fn end_params(session_id: &str) -> CallToolRequestParams {
    CallToolRequestParams::new("tui_end").with_arguments(
        json!({"session_id": session_id})
            .as_object()
            .unwrap()
            .clone(),
    )
}

fn text_of(result: &CallToolResult) -> String {
    let content = serde_json::to_value(&result.content).unwrap();
    let text = content[0]["text"].as_str().unwrap().to_string();
    assert!(!result.is_error.unwrap_or(false), "{text}");
    text
}

/// Starts a session; fails if `tui_start` waits on the person.
async fn start(client: &Client, session_id: &str) -> String {
    start_with(client, start_params(session_id, &json!({}))).await
}

async fn start_with(client: &Client, params: CallToolRequestParams) -> String {
    let result = tokio::time::timeout(Duration::from_secs(10), client.call_tool(params))
        .await
        .expect("tui_start waited on the person")
        .expect("call tui_start");
    text_of(&result)
}

/// Ends a session, answering any form the server returns.
async fn end(client: &Client, session_id: &str) -> String {
    let result = client
        .call_tool(end_params(session_id))
        .await
        .expect("call tui_end");
    text_of(&result)
}

/// One `tools/call` round, without answering any form the server returns.
async fn call_once(client: &Client, params: CallToolRequestParams) -> CallToolResponse {
    client.call_tool_once(params).await.expect("call")
}

fn exists(file: &str) -> bool {
    Path::new(file).exists()
}

/// The agent is told it may offer the person more.
const HINT: &str = "Ask the person whether they want";

#[tokio::test]
async fn test_background_form_does_not_block_start_and_its_answer_moves_the_files() {
    let dir = temp("background");
    let kept = path(&dir, "kept.cast");
    let hold = Arc::new(Notify::new());
    let person = Person::holding(
        Reply::Accept(json!({"record": true, "record_path": kept, "report": false})),
        &hold,
    );
    let (client, _manager) =
        connect(person.clone(), initialize(), &dir, Duration::from_secs(30)).await;

    // The session starts while the person still has the form open
    let started = start(&client, "one").await;
    assert!(
        started.contains(&format!("recording to '{}'", path(&dir, "sh.cast"))),
        "{started}"
    );
    assert!(started.contains("is being asked"), "{started}");
    tokio::time::timeout(Duration::from_secs(10), async {
        while person.asked() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the person is asked in the background");
    assert!(
        person.offered("live"),
        "the start form offers watching live"
    );

    hold.notify_one();
    let ended = end(&client, "one").await;
    assert!(
        ended.contains(&format!("Recording saved to '{kept}'")),
        "{ended}"
    );
    assert!(ended.contains("Report deleted"), "{ended}");
    assert!(exists(&kept));
    assert!(!exists(&path(&dir, "sh.cast")));
    assert!(!exists(&path(&dir, "sh.report.jsonl")));

    // Later sessions take the answer from the start, without asking again
    let later = start(&client, "two").await;
    assert!(
        later.contains(&format!("recording to '{}'", path(&dir, "kept-2.cast"))),
        "{later}"
    );
    assert!(!later.contains("reporting to"), "{later}");
    assert_eq!(person.asked(), 1);
    end(&client, "two").await;

    let _ = client.cancel().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn test_unanswered_background_form_keeps_the_defaults() {
    let dir = temp("timeout");
    let hold = Arc::new(Notify::new());
    let person = Person::holding(Reply::Accept(json!({"record": false})), &hold);
    let (client, _manager) = connect(
        person.clone(),
        initialize(),
        &dir,
        Duration::from_millis(500),
    )
    .await;

    start(&client, "one").await;
    // tui_end waits for the form's timeout, then keeps both files
    let ended = end(&client, "one").await;
    let record = path(&dir, "sh.cast");
    let report = path(&dir, "sh.report.jsonl");
    assert!(
        ended.contains(&format!("Recording saved to '{record}'")),
        "{ended}"
    );
    assert!(ended.contains(&format!("Report '{report}'")), "{ended}");
    assert!(exists(&record) && exists(&report));

    // The defaults now apply from the start, and the agent hears once that it may offer more
    let later = start(&client, "two").await;
    assert!(
        later.contains(&format!("recording to '{}'", path(&dir, "sh-2.cast"))),
        "{later}"
    );
    assert!(later.contains(HINT), "{later}");
    let third = start(&client, "three").await;
    assert!(!third.contains(HINT), "{third}");
    end(&client, "two").await;
    end(&client, "three").await;
    assert_eq!(person.asked(), 1);

    hold.notify_waiters();
    let _ = client.cancel().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn test_form_at_end_on_2026_07_28() {
    let dir = temp("at_end");
    let kept = path(&dir, "kept.cast");
    let person = Person::new(Reply::Accept(
        json!({"record": true, "record_path": kept, "report": false}),
    ));
    let (client, _manager) =
        connect(person.clone(), discover(), &dir, Duration::from_secs(30)).await;

    let started = start(&client, "one").await;
    assert!(
        started.contains(&format!("recording to '{}'", path(&dir, "sh.cast"))),
        "{started}"
    );
    assert!(started.contains("When this session ends"), "{started}");
    assert_eq!(person.asked(), 0, "not asked as the session starts");

    let ended = end(&client, "one").await;
    assert_eq!(person.asked(), 1);
    assert!(!person.offered("live"), "too late to watch live");
    assert!(
        ended.contains(&format!("Recording saved to '{kept}'")),
        "{ended}"
    );
    assert!(ended.contains("Report deleted"), "{ended}");
    assert!(exists(&kept) && !exists(&path(&dir, "sh.report.jsonl")));

    let later = start(&client, "two").await;
    assert!(
        later.contains(&format!("recording to '{}'", path(&dir, "kept-2.cast"))),
        "{later}"
    );
    end(&client, "two").await;
    assert_eq!(person.asked(), 1, "asked once per server");

    let _ = client.cancel().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn test_retry_without_an_answer_keeps_the_defaults() {
    let dir = temp("retry");
    let person = Person::new(Reply::Accept(json!({"record": false})));
    let (client, _manager) =
        connect(person.clone(), discover(), &dir, Duration::from_secs(30)).await;

    start(&client, "one").await;
    start(&client, "other").await;
    let CallToolResponse::InputRequired(form) = call_once(&client, end_params("one")).await else {
        panic!("tui_end didn't return the capture form");
    };
    let state = form.request_state.clone().expect("request state");

    // Another session ending while the form is out keeps its defaults, without a second form
    let CallToolResponse::Complete(other) = call_once(&client, end_params("other")).await else {
        panic!("the form was sent twice");
    };
    let other = text_of(&other);
    assert!(
        other.contains(&format!("Recording saved to '{}'", path(&dir, "sh-2.cast"))),
        "{other}"
    );

    // Coming back with the request state but no answer means the form was closed
    let mut retry = end_params("one");
    retry.request_state = Some(state);
    let CallToolResponse::Complete(first) = call_once(&client, retry).await else {
        panic!("the form was sent again after the retry");
    };
    let first = text_of(&first);
    assert!(
        first.contains(&format!("Recording saved to '{}'", path(&dir, "sh.cast"))),
        "{first}"
    );
    assert_eq!(person.asked(), 0, "the form was never shown");

    let _ = client.cancel().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn test_declined_or_closed_form_keeps_the_defaults() {
    for (n, (reply, hint)) in [(Reply::Decline, false), (Reply::Cancel, true)]
        .into_iter()
        .enumerate()
    {
        for (m, lifecycle) in lifecycles().into_iter().enumerate() {
            let dir = temp(&format!("no_answer{n}{m}"));
            let person = Person::new(reply.clone());
            let (client, _manager) =
                connect(person.clone(), lifecycle, &dir, Duration::from_secs(30)).await;

            start(&client, "one").await;
            let ended = end(&client, "one").await;
            let record = path(&dir, "sh.cast");
            assert!(
                ended.contains(&format!("Recording saved to '{record}'")),
                "{ended}"
            );
            assert!(exists(&record) && exists(&path(&dir, "sh.report.jsonl")));

            // Declining isn't nagged about; a closed form gets the hint once
            let later = start(&client, "two").await;
            assert!(
                later.contains(&format!("recording to '{}'", path(&dir, "sh-2.cast"))),
                "{later}"
            );
            assert_eq!(later.contains(HINT), hint, "{later}");
            end(&client, "two").await;
            assert_eq!(person.asked(), 1);

            let _ = client.cancel().await;
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[tokio::test]
async fn test_client_without_forms_keeps_the_defaults_and_gets_a_hint_once() {
    for (n, lifecycle) in lifecycles().into_iter().enumerate() {
        let dir = temp(&format!("no_forms{n}"));
        let person = Person::new(Reply::NoForms);
        let (client, _manager) =
            connect(person.clone(), lifecycle, &dir, Duration::from_secs(30)).await;

        let first = start(&client, "one").await;
        assert!(
            first.contains(&format!("recording to '{}'", path(&dir, "sh.cast"))),
            "{first}"
        );
        assert!(first.contains(HINT), "{first}");
        let second = start(&client, "two").await;
        assert!(!second.contains(HINT), "{second}");
        end(&client, "one").await;
        end(&client, "two").await;
        assert!(exists(&path(&dir, "sh.cast")) && exists(&path(&dir, "sh-2.cast")));
        assert_eq!(person.asked(), 0);

        let _ = client.cancel().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[tokio::test]
async fn test_explicit_start_captures_only_what_it_asks_for() {
    for (n, lifecycle) in lifecycles().into_iter().enumerate() {
        let dir = temp(&format!("explicit{n}"));
        let person = Person::new(Reply::Accept(json!({"record": true})));
        let (client, _manager) =
            connect(person.clone(), lifecycle, &dir, Duration::from_secs(30)).await;

        let started = start_with(&client, start_params("one", &json!({"live": false}))).await;
        assert!(!started.contains("recording to"), "{started}");
        let ended = end(&client, "one").await;
        assert!(!ended.contains("Recording"), "{ended}");
        assert_eq!(person.asked(), 0);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "no files");

        let _ = client.cancel().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
