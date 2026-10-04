//! A scripted MCP client: plays the client's role (Claude Code, an older client, …) against a
//! ShadowPTY server, deterministically and without a model, and checks what comes back.
//!
//! ```sh
//! cargo build --release --example mcp_script
//! target/release/examples/mcp_script mcp-tests/auto-test/scripts/06-a-accept-other-files.json
//! ```
//!
//! It spawns the server, connects as the script's client, runs the steps in order, prints a
//! PASS/FAIL line per assertion and ends with `mcp_script: N passed, M failed`. The exit code is 0
//! only if every assertion passed (1 if one failed, 2 if the script couldn't run).
//!
//! # Script
//!
//! A JSON file. Before it's read, `${REPO}` becomes the repository root and `${DIR}` the
//! script's own directory.
//!
//! ```json
//! {
//!   "description": "What this script checks",
//!   "server": {
//!     "command": ["${REPO}/target/release/shadowpty"],
//!     "cwd": "${DIR}/../script-out/example",
//!     "env": {"RUST_LOG": "info"},
//!     "stderr_log": "${DIR}/../script-out/example/server-stderr.log"
//!   },
//!   "client": {"name": "mcp-script", "version": "0", "protocol": "2026-07-28", "forms": true, "tasks": false},
//!   "clean": ["${DIR}/../script-out/example"],
//!   "answers": [{"accept": {"record": true}}, "decline", {"cancel": true, "hold_ms": 2000}],
//!   "steps": [ … ]
//! }
//! ```
//!
//! - `server`: what to spawn (default: the release build of this repository), in which working
//!   directory (created if missing; default files of the capture form land there), with which
//!   environment, and where its stderr goes (appended; discarded if unset).
//! - `client`: the identity and capabilities the client declares. `protocol` is `"2026-07-28"`
//!   (the `server/discover` lifecycle, as Claude Code) or `"legacy"` (the `initialize`
//!   handshake). `forms` declares form elicitation; `tasks` declares the MCP Tasks extension.
//! - `clean`: files or directories removed before the server starts.
//! - `answers`: how the person answers each form, in order: `"accept"` (with the form's
//!   defaults), `"decline"`, `"cancel"`, or `{"accept": {…content…}}`, `{"decline": true}`,
//!   `{"cancel": true}`, each with an optional `"hold_ms"` to answer late. A form beyond the
//!   list is cancelled and reported.
//!
//! # Steps
//!
//! - `{"call": "tui_start", "args": {…}}`: calls the tool, answering any form the server returns
//!   (protocol 2026-07-28) or sends (older protocols).
//! - `{"call_once": "tui_end", "args": {…}, "request_state": "previous"}`: one round only; a form
//!   is not answered. `"request_state": "previous"` sends back the state of the last form
//!   returned, without an answer.
//! - Call assertions: `expect` (substrings of the result text), `expect_not`, `error` (the tool
//!   result is an error; default false), `within_ms` (the call returned in time),
//!   `expect_input_required` (`call_once` only), `expect_form` (forms shown while the call ran:
//!   `{"shown": bool, "fields": […], "no_fields": […], "message": "…"}`, checked on the last one).
//! - `{"forms": {"count": N, "timeout_ms": 5000, "fields": […], "no_fields": […], "message": "…"}}`:
//!   waits until N forms were shown in total, then checks the last one.
//! - `{"files": {"<path>": true, "<other>": false}}`: which files exist.
//! - `{"log": {"contains": […], "count": {"pattern": "…", "equals": 1}, "timeout_ms": 3000}}`: the
//!   server's stderr log, waiting up to `timeout_ms` for the lines to land.
//! - `{"remove": [paths]}`: removes files or directories.
//!
//! Text in a step may carry a `"label"` that's printed with it.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
    ElicitRequestParams, ElicitResult, ElicitationAction, Implementation, ProtocolVersion,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, ClientLifecycleMode, ClientServiceExt, ErrorData, RoleClient};
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

/// The repository this example was built from.
const REPO: &str = env!("CARGO_MANIFEST_DIR");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Script {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    server: Server,
    #[serde(default)]
    client: Client,
    #[serde(default)]
    clean: Vec<String>,
    #[serde(default)]
    answers: Vec<AnswerSpec>,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Server {
    #[serde(default = "default_command")]
    command: Vec<String>,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    stderr_log: Option<String>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            command: default_command(),
            cwd: None,
            env: BTreeMap::new(),
            stderr_log: None,
        }
    }
}

fn default_command() -> Vec<String> {
    vec![format!("{REPO}/target/release/shadowpty")]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Client {
    #[serde(default = "default_name")]
    name: String,
    #[serde(default = "default_version")]
    version: String,
    #[serde(default = "default_protocol")]
    protocol: String,
    #[serde(default = "yes")]
    forms: bool,
    #[serde(default)]
    tasks: bool,
}

impl Default for Client {
    fn default() -> Self {
        Self {
            name: default_name(),
            version: default_version(),
            protocol: default_protocol(),
            forms: true,
            tasks: false,
        }
    }
}

fn default_name() -> String {
    "mcp-script".to_string()
}

fn default_version() -> String {
    "0".to_string()
}

fn default_protocol() -> String {
    "2026-07-28".to_string()
}

const fn yes() -> bool {
    true
}

/// How the person answers one form, as written in the script.
#[derive(Deserialize)]
#[serde(untagged)]
enum AnswerSpec {
    Word(String),
    Full(FullAnswer),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FullAnswer {
    accept: Option<Value>,
    #[serde(default)]
    decline: bool,
    #[serde(default)]
    cancel: bool,
    #[serde(default)]
    hold_ms: u64,
}

/// How the person answers one form.
#[derive(Debug, Clone)]
struct Answer {
    action: ElicitationAction,
    content: Option<Value>,
    hold: Duration,
}

impl TryFrom<AnswerSpec> for Answer {
    type Error = anyhow::Error;

    fn try_from(spec: AnswerSpec) -> Result<Self> {
        let full = match spec {
            AnswerSpec::Word(word) => match word.as_str() {
                "accept" => FullAnswer {
                    accept: Some(Value::Object(Map::new())),
                    decline: false,
                    cancel: false,
                    hold_ms: 0,
                },
                "decline" => FullAnswer {
                    accept: None,
                    decline: true,
                    cancel: false,
                    hold_ms: 0,
                },
                "cancel" => FullAnswer {
                    accept: None,
                    decline: false,
                    cancel: true,
                    hold_ms: 0,
                },
                other => bail!("unknown answer {other:?}: use accept, decline or cancel"),
            },
            AnswerSpec::Full(full) => full,
        };
        let action = match (&full.accept, full.decline, full.cancel) {
            (Some(_), false, false) => ElicitationAction::Accept,
            (None, true, false) => ElicitationAction::Decline,
            (None, false, true) => ElicitationAction::Cancel,
            _ => bail!("an answer needs exactly one of accept, decline or cancel"),
        };
        Ok(Self {
            action,
            content: full.accept,
            hold: Duration::from_millis(full.hold_ms),
        })
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Step {
    Files(FilesStep),
    Log(LogStep),
    Forms(FormsStep),
    Remove(RemoveStep),
    Call(Box<CallStep>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesStep {
    files: BTreeMap<String, bool>,
    label: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogStep {
    log: LogCheck,
    label: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogCheck {
    #[serde(default)]
    contains: Vec<String>,
    count: Option<CountCheck>,
    #[serde(default = "default_log_timeout")]
    timeout_ms: u64,
}

const fn default_log_timeout() -> u64 {
    3000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CountCheck {
    pattern: String,
    equals: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FormsStep {
    forms: FormsCheck,
    label: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FormsCheck {
    count: usize,
    #[serde(default = "default_forms_timeout")]
    timeout_ms: u64,
    #[serde(flatten)]
    last: FormCheck,
}

const fn default_forms_timeout() -> u64 {
    5000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveStep {
    remove: Vec<String>,
    label: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallStep {
    call: Option<String>,
    call_once: Option<String>,
    #[serde(default)]
    args: Map<String, Value>,
    request_state: Option<String>,
    label: Option<String>,
    #[serde(default)]
    expect: Vec<String>,
    #[serde(default)]
    expect_not: Vec<String>,
    #[serde(default)]
    error: bool,
    within_ms: Option<u64>,
    expect_input_required: Option<bool>,
    expect_form: Option<FormCheck>,
}

/// Checks on a form that was shown.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FormCheck {
    shown: Option<bool>,
    #[serde(default)]
    fields: Vec<String>,
    #[serde(default)]
    no_fields: Vec<String>,
    message: Option<String>,
}

/// A form the server showed the person.
#[derive(Debug, Clone)]
struct Form {
    message: String,
    fields: Vec<String>,
}

/// The client: answers forms from the script's queue and remembers each one.
#[derive(Clone)]
struct Person {
    info: ClientConfig,
    answers: Arc<Mutex<VecDeque<Answer>>>,
    forms: Arc<Mutex<Vec<Form>>>,
}

impl ClientHandler for Person {
    fn get_info(&self) -> ClientConfig {
        self.info.clone()
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        let form = match &request {
            ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => Form {
                message: message.clone(),
                fields: serde_json::to_value(requested_schema)
                    .ok()
                    .and_then(|schema| {
                        schema["properties"]
                            .as_object()
                            .map(|fields| fields.keys().cloned().collect())
                    })
                    .unwrap_or_default(),
            },
            _ => Form {
                message: "(a URL elicitation)".to_string(),
                fields: Vec::new(),
            },
        };
        println!(
            "     form: {:?} with fields {:?}",
            form.message, form.fields
        );
        self.forms.lock().await.push(form);
        let answer = self.answers.lock().await.pop_front();
        let Some(answer) = answer else {
            println!("     form: no answer left in the script, closing it");
            return Ok(ElicitResult::new(ElicitationAction::Cancel));
        };
        if !answer.hold.is_zero() {
            tokio::time::sleep(answer.hold).await;
        }
        println!("     form: answered {:?}", answer.action);
        let accept = answer.action == ElicitationAction::Accept;
        let result = ElicitResult::new(answer.action);
        Ok(match answer.content {
            Some(content) if accept => result.with_content(content),
            _ => result,
        })
    }
}

/// Assertion counts.
#[derive(Default)]
struct Tally {
    passed: usize,
    failed: usize,
}

impl Tally {
    fn check(&mut self, ok: bool, what: impl Display) {
        if ok {
            self.passed += 1;
            println!("     PASS {what}");
        } else {
            self.failed += 1;
            println!("     FAIL {what}");
        }
    }
}

/// Runs the steps against a connected server.
struct Runner {
    client: RunningService<RoleClient, Person>,
    forms: Arc<Mutex<Vec<Form>>>,
    stderr_log: Option<PathBuf>,
    /// The request state of the last form returned by a `call_once`.
    last_state: Option<String>,
    tally: Tally,
}

impl Runner {
    async fn run(&mut self, number: usize, step: Step) -> Result<()> {
        match step {
            Step::Call(call) => self.call(number, *call).await,
            Step::Files(files) => {
                header(number, "files", files.label.as_deref());
                for (path, exists) in files.files {
                    let found = Path::new(&path).exists();
                    let what = if exists { "exists" } else { "doesn't exist" };
                    self.tally.check(found == exists, format!("{path} {what}"));
                }
                Ok(())
            }
            Step::Log(log) => {
                header(number, "log", log.label.as_deref());
                self.log(&log.log).await
            }
            Step::Forms(forms) => {
                header(number, "forms", forms.label.as_deref());
                self.wait_forms(&forms.forms).await;
                Ok(())
            }
            Step::Remove(remove) => {
                header(number, "remove", remove.label.as_deref());
                for path in remove.remove {
                    remove_path(&path)?;
                    println!("     removed {path}");
                }
                Ok(())
            }
        }
    }

    async fn call(&mut self, number: usize, step: CallStep) -> Result<()> {
        let (tool, once) = match (&step.call, &step.call_once) {
            (Some(tool), None) => (tool.clone(), false),
            (None, Some(tool)) => (tool.clone(), true),
            _ => bail!("step {number}: give exactly one of call or call_once"),
        };
        let kind = if once { "call_once" } else { "call" };
        header(
            number,
            &format!("{kind} {tool} {}", Value::Object(step.args.clone())),
            step.label.as_deref(),
        );
        let mut params = CallToolRequestParams::new(tool).with_arguments(step.args.clone());
        if step.request_state.as_deref() == Some("previous") {
            params.request_state.clone_from(&self.last_state);
        }
        let forms_before = self.forms.lock().await.len();
        let started = Instant::now();
        let outcome = if once {
            match self.client.call_tool_once(params).await? {
                CallToolResponse::Complete(result) => Outcome::Complete(result),
                CallToolResponse::InputRequired(input) => {
                    self.last_state.clone_from(&input.request_state);
                    Outcome::InputRequired
                }
                _ => Outcome::Other,
            }
        } else {
            Outcome::Complete(self.client.call_tool(params).await?)
        };
        let elapsed = started.elapsed();
        println!("     took {} ms", elapsed.as_millis());
        self.check_call(&step, &outcome, elapsed);
        let forms = self.forms.lock().await;
        let shown = forms.get(forms_before..).unwrap_or_default().to_vec();
        drop(forms);
        if let Some(check) = &step.expect_form {
            check_forms(&mut self.tally, check, &shown);
        }
        Ok(())
    }

    fn check_call(&mut self, step: &CallStep, outcome: &Outcome, elapsed: Duration) {
        let text = match outcome {
            Outcome::Complete(result) => {
                let text = result_text(result);
                for line in text.lines() {
                    println!("     | {line}");
                }
                let is_error = result.is_error.unwrap_or(false);
                let what = if step.error {
                    "result is an error"
                } else {
                    "result is not an error"
                };
                self.tally.check(is_error == step.error, what);
                text
            }
            Outcome::InputRequired => {
                println!("     | (input required: the server returned a form)");
                String::new()
            }
            Outcome::Other => {
                println!("     | (an unexpected kind of result)");
                String::new()
            }
        };
        if let Some(expected) = step.expect_input_required {
            let got = matches!(outcome, Outcome::InputRequired);
            self.tally
                .check(got == expected, format!("input required: {expected}"));
        }
        for expected in &step.expect {
            self.tally.check(
                text.contains(expected.as_str()),
                format!("has {expected:?}"),
            );
        }
        for unexpected in &step.expect_not {
            self.tally.check(
                !text.contains(unexpected.as_str()),
                format!("hasn't {unexpected:?}"),
            );
        }
        if let Some(limit) = step.within_ms {
            self.tally.check(
                elapsed <= Duration::from_millis(limit),
                format!("returned within {limit} ms"),
            );
        }
    }

    async fn wait_forms(&mut self, check: &FormsCheck) {
        let deadline = Instant::now() + Duration::from_millis(check.timeout_ms);
        let forms = loop {
            let forms = self.forms.lock().await.clone();
            if forms.len() >= check.count || Instant::now() >= deadline {
                break forms;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        self.tally.check(
            forms.len() == check.count,
            format!(
                "{} form(s) shown in total (got {})",
                check.count,
                forms.len()
            ),
        );
        check_last_form(&mut self.tally, &check.last, forms.last());
    }

    async fn log(&mut self, check: &LogCheck) -> Result<()> {
        let Some(path) = self.stderr_log.clone() else {
            bail!("a log step needs server.stderr_log");
        };
        let deadline = Instant::now() + Duration::from_millis(check.timeout_ms);
        let text = loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let found = check
                .contains
                .iter()
                .all(|line| text.contains(line.as_str()));
            if found || Instant::now() >= deadline {
                break text;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        // The lines the checks are about, or the whole log when none of them is there
        let wanted = |line: &str| {
            check.contains.iter().any(|c| line.contains(c.as_str()))
                || check
                    .count
                    .as_ref()
                    .is_some_and(|c| line.contains(c.pattern.as_str()))
        };
        let shown: Vec<&str> = text.lines().filter(|line| wanted(line)).collect();
        let shown = if shown.is_empty() {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .collect()
        } else {
            shown
        };
        for line in shown {
            println!("     | {line}");
        }
        for expected in &check.contains {
            self.tally.check(
                text.contains(expected.as_str()),
                format!("has {expected:?}"),
            );
        }
        if let Some(count) = &check.count {
            let got = text.matches(count.pattern.as_str()).count();
            self.tally.check(
                got == count.equals,
                format!(
                    "{:?} appears {} time(s) (got {got})",
                    count.pattern, count.equals
                ),
            );
        }
        Ok(())
    }
}

/// What a tool call came back with.
enum Outcome {
    Complete(CallToolResult),
    InputRequired,
    Other,
}

fn header(number: usize, what: &str, label: Option<&str>) {
    match label {
        Some(label) => println!("{number:>3}. {label}: {what}"),
        None => println!("{number:>3}. {what}"),
    }
}

fn check_forms(tally: &mut Tally, check: &FormCheck, shown: &[Form]) {
    if let Some(expected) = check.shown {
        let what = if expected {
            "a form was shown"
        } else {
            "no form was shown"
        };
        tally.check(shown.is_empty() != expected, what);
    }
    if !check.fields.is_empty() || !check.no_fields.is_empty() || check.message.is_some() {
        check_last_form(tally, check, shown.last());
    }
}

fn check_last_form(tally: &mut Tally, check: &FormCheck, form: Option<&Form>) {
    let Some(form) = form else {
        if !check.fields.is_empty() || check.message.is_some() {
            tally.check(false, "a form to check");
        }
        return;
    };
    for field in &check.fields {
        tally.check(
            form.fields.contains(field),
            format!("the form has field {field:?}"),
        );
    }
    for field in &check.no_fields {
        tally.check(
            !form.fields.contains(field),
            format!("the form has no field {field:?}"),
        );
    }
    if let Some(message) = &check.message {
        tally.check(
            form.message.contains(message.as_str()),
            format!("the form says {message:?}"),
        );
    }
}

/// The text of a tool result, one content item per line.
fn result_text(result: &CallToolResult) -> String {
    let content = serde_json::to_value(&result.content).unwrap_or_default();
    content
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    item["text"]
                        .as_str()
                        .map_or_else(|| format!("[{}]", item["type"]), str::to_string)
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn remove_path(path: &str) -> Result<()> {
    let path = Path::new(path);
    let removed = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match removed {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Reads the script, with `${REPO}` and `${DIR}` replaced.
fn load(path: &Path) -> Result<Script> {
    let path = path
        .canonicalize()
        .with_context(|| format!("can't find {}", path.display()))?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("can't read {}", path.display()))?
        .replace("${REPO}", REPO)
        .replace("${DIR}", &dir.to_string_lossy());
    serde_json::from_str(&text).with_context(|| format!("{} isn't a valid script", path.display()))
}

/// Spawns the server and connects to it as the script's client.
async fn connect(script: &Script, person: Person) -> Result<RunningService<RoleClient, Person>> {
    let server = &script.server;
    let Some((program, args)) = server.command.split_first() else {
        bail!("server.command is empty");
    };
    let mut command = tokio::process::Command::new(program);
    command.args(args).envs(&server.env);
    if let Some(cwd) = &server.cwd {
        std::fs::create_dir_all(cwd).with_context(|| format!("creating {cwd}"))?;
        command.current_dir(cwd);
    }
    let stderr = match &server.stderr_log {
        Some(log) => {
            if let Some(parent) = Path::new(log).parent() {
                std::fs::create_dir_all(parent)?;
            }
            Stdio::from(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log)
                    .with_context(|| format!("opening {log}"))?,
            )
        }
        None => Stdio::null(),
    };
    let (transport, _) = TokioChildProcess::builder(command)
        .stderr(stderr)
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    let lifecycle = match script.client.protocol.as_str() {
        "legacy" => ClientLifecycleMode::Initialize,
        "2026-07-28" => ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
        other => bail!("unknown protocol {other:?}: use \"2026-07-28\" or \"legacy\""),
    };
    person
        .serve_with_lifecycle(transport, lifecycle)
        .await
        .context("connecting to the server")
}

fn client_config(client: &Client) -> ClientConfig {
    let capabilities = match (client.forms, client.tasks) {
        (true, true) => ClientCapabilities::builder()
            .enable_elicitation()
            .enable_tasks()
            .build(),
        (true, false) => ClientCapabilities::builder().enable_elicitation().build(),
        (false, true) => ClientCapabilities::builder().enable_tasks().build(),
        (false, false) => ClientCapabilities::default(),
    };
    ClientConfig::new(
        capabilities,
        Implementation::new(client.name.clone(), client.version.clone()),
    )
}

async fn run(path: &Path) -> Result<Tally> {
    let mut script = load(path)?;
    println!("mcp_script: {}", path.display());
    if let Some(description) = &script.description {
        println!("  {description}");
    }
    for path in &script.clean {
        remove_path(path)?;
    }
    let answers = std::mem::take(&mut script.answers)
        .into_iter()
        .map(Answer::try_from)
        .collect::<Result<VecDeque<_>>>()?;
    let forms = Arc::new(Mutex::new(Vec::new()));
    let person = Person {
        info: client_config(&script.client),
        answers: Arc::new(Mutex::new(answers)),
        forms: Arc::clone(&forms),
    };
    let client = connect(&script, person).await?;
    let mut runner = Runner {
        client,
        forms,
        stderr_log: script.server.stderr_log.as_ref().map(PathBuf::from),
        last_state: None,
        tally: Tally::default(),
    };
    for (index, step) in script.steps.into_iter().enumerate() {
        if let Err(e) = runner.run(index + 1, step).await {
            runner.tally.check(false, format!("step failed: {e:#}"));
        }
    }
    let Runner { client, tally, .. } = runner;
    let _ = client.cancel().await;
    Ok(tally)
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        eprintln!("usage: mcp_script <script.json>");
        return ExitCode::from(2);
    };
    match run(Path::new(&path)).await {
        Ok(tally) => {
            println!(
                "mcp_script: {} passed, {} failed",
                tally.passed, tally.failed
            );
            if tally.failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            println!("mcp_script: error: {e:#}");
            ExitCode::from(2)
        }
    }
}
