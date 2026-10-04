//! MCP Tool Router implementation for `ShadowPTY`.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    ServerHandler,
    handler::server::{common::FromContextPart, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolResponse, CallToolResult, DiscoverResult, InitializeRequestParams,
        InitializeResult, InputRequiredResult, ProtocolVersion,
    },
    schemars::{self, JsonSchema},
    service::RequestContext,
    tool, tool_handler, tool_router, {Peer, RoleServer},
};
use serde::{Deserialize, Serialize};

use crate::capture::{self, Capture, Note, Reach, Requested};
use crate::client_log::{ClientLog, ClientSeen};
use crate::live::LiveServer;
use crate::output::{Pattern, Syntax};
use crate::pty_manager::{
    DEFAULT_SESSION_ID, ExpectMatch, ExpectTarget, Expectation, PtyManager, ScreenshotTaken,
    Script, SignalTarget,
};
use crate::signals::{parse_signal, signal_name};

/// Upper limit for any wait, so a single call can't hang the agent indefinitely.
const MAX_WAIT_MS: u64 = 120_000;

fn wait_duration(requested_ms: Option<u64>, default_ms: u64) -> Duration {
    Duration::from_millis(requested_ms.unwrap_or(default_ms).min(MAX_WAIT_MS))
}

fn text_result(text: String) -> CallToolResult {
    CallToolResult::success(vec![rmcp::model::ContentBlock::text(text)])
}

fn image_result(base64_data: String, mime_type: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![rmcp::model::ContentBlock::image(
        base64_data,
        mime_type,
    )])
}

fn error_result(text: String) -> CallToolResult {
    CallToolResult::error(vec![rmcp::model::ContentBlock::text(text)])
}

/// Most output shown on each side of a match when `include_context` is set.
const CONTEXT_CHARS: usize = 500;

/// The last `max` characters of `text`.
fn last_chars(text: &str, max: usize) -> String {
    let skip = text.chars().count().saturating_sub(max);
    text.chars().skip(skip).collect()
}

/// Describes a `tui_expect` match for the model: which pattern, the matched text, where it is on
/// screen, and (when asked) the output around it.
fn describe_match(
    found: &ExpectMatch,
    sources: &[String],
    session_id: &str,
    include_context: bool,
) -> String {
    let mut text = if sources.len() == 1 {
        format!("Matched {:?}", found.matched)
    } else {
        format!(
            "Matched pattern {} of {} ('{}'): {:?}",
            found.index + 1,
            sources.len(),
            sources[found.index],
            found.matched
        )
    };
    if let Some(line) = &found.line {
        let _ = write!(text, " on row {}: {:?}", line.row + 1, line.text);
    }
    let _ = write!(text, " in session '{session_id}'");
    if include_context {
        let before = last_chars(&found.before, CONTEXT_CHARS);
        if !before.trim().is_empty() {
            let _ = write!(text, "\nOutput before the match:\n{before}");
        }
        let after: String = found.after.chars().take(CONTEXT_CHARS).collect();
        if !after.trim().is_empty() {
            let _ = write!(text, "\nOutput after the match (still unread):\n{after}");
        }
    }
    text
}

/// How a tool's patterns are interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PatternSyntax {
    /// Matched verbatim (the default).
    Literal,
    /// Regular expression.
    Regex,
    /// Shell-style glob, matched anywhere in the text: `*` and `?` within a line, `[abc]`,
    /// `[a-z]`, `[!abc]`; `\\` escapes.
    Glob,
}

/// Combines the `syntax` and older `is_regex` parameters. They only conflict when they say
/// different things about whether the pattern is a regex.
fn resolve_syntax(syntax: Option<PatternSyntax>, is_regex: Option<bool>) -> Result<Syntax, String> {
    match (syntax, is_regex) {
        (Some(PatternSyntax::Regex), Some(false))
        | (Some(PatternSyntax::Literal | PatternSyntax::Glob), Some(true)) => Err(
            "`syntax` and `is_regex` disagree; use `syntax` alone (\"literal\", \"regex\" or \"glob\")"
                .into(),
        ),
        (Some(PatternSyntax::Literal), _) | (None, None | Some(false)) => Ok(Syntax::Literal),
        (Some(PatternSyntax::Regex), _) | (None, Some(true)) => Ok(Syntax::Regex),
        (Some(PatternSyntax::Glob), _) => Ok(Syntax::Glob),
    }
}

/// Patterns given to a tool as `pattern` or `patterns`, with the text the caller wrote.
struct ToolPatterns {
    sources: Vec<String>,
    compiled: Vec<Pattern>,
}

/// Validates and compiles `pattern` / `patterns` (exactly one must be given). The error is a
/// message for the tool result.
fn parse_patterns(
    pattern: Option<String>,
    patterns: Option<Vec<String>>,
    syntax: Syntax,
) -> Result<ToolPatterns, String> {
    let sources = match (pattern, patterns) {
        (Some(pattern), None) => vec![pattern],
        (None, Some(patterns)) if !patterns.is_empty() => patterns,
        (Some(_), Some(_)) => return Err("Give either `pattern` or `patterns`, not both".into()),
        _ => return Err("Give a `pattern` or a non-empty `patterns` list to wait for".into()),
    };
    let compiled = sources
        .iter()
        .map(|source| {
            Pattern::with_syntax(source, syntax)
                .map_err(|e| format!("Invalid pattern '{source}': {e}"))
        })
        .collect::<Result<_, _>>()?;
    Ok(ToolPatterns { sources, compiled })
}

/// Parameters for `tui_start` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiStartParams {
    /// The executable command to spawn (e.g. "top", "htop", "bash").
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Number of terminal rows (default 24).
    pub rows: Option<u16>,
    /// Number of terminal columns (default 80).
    pub cols: Option<u16>,
    /// Optional filesystem path where session will be recorded in asciicast v3 format.
    pub record_path: Option<String>,
    /// Optional filesystem path for a JSON Lines report of the session: inputs, every check
    /// (expect, waits, scripts) with pass/fail and timing, screenshots, the exit status, and a
    /// summary written when the session ends.
    pub report_path: Option<String>,
    /// Identifier for this session (defaults to "default"). Starting an existing id replaces that session.
    pub session_id: Option<String>,
    /// If true, the session can be watched live in a browser: the reply includes a link to a
    /// local, view-only page showing the screen and a timeline of inputs and checks
    /// (default false). The link contains a secret token and is meant for the person.
    pub live: Option<bool>,
}

/// Parameters for `tui_input` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiInputParams {
    /// String containing keystrokes and symbolic tokens (e.g. "<ENTER>", "<ESC>", "<UP>", "<CTRL+C>").
    pub keys: String,
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Which processes `tui_signal` signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SignalTargetParam {
    /// The terminal's foreground process group, like Ctrl+C in a real terminal (the default).
    Foreground,
    /// Only the process started by `tui_start` (e.g. the shell, not the job it runs).
    Process,
}

impl From<SignalTargetParam> for SignalTarget {
    fn from(target: SignalTargetParam) -> Self {
        match target {
            SignalTargetParam::Foreground => Self::Foreground,
            SignalTargetParam::Process => Self::Process,
        }
    }
}

/// Parameters for `tui_signal` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiSignalParams {
    /// Signal name, with or without "SIG": INT, TERM, HUP, QUIT, KILL, TSTP, STOP, CONT, USR1,
    /// USR2, WINCH, ALRM, PIPE, TTIN, TTOU, and the crash signals ABRT, SEGV, BUS, FPE, TRAP.
    pub signal: String,
    /// "foreground" (default): the terminal's foreground process group, as Ctrl+C does.
    /// "process": only the process started by `tui_start`.
    pub target: Option<SignalTargetParam>,
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_resize` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiResizeParams {
    /// New number of terminal rows.
    pub rows: u16,
    /// New number of terminal columns.
    pub cols: u16,
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_paste` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiPasteParams {
    /// Text to send as a single bracketed paste (DECSET 2004).
    pub text: String,
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_expect` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiExpectParams {
    /// Substring or regex pattern to wait for. Give either `pattern` or `patterns`.
    pub pattern: Option<String>,
    /// Several patterns: waits for whichever appears first and reports which one it was
    /// (e.g. `["Password:", "Permission denied", "$ "]`). Give either `pattern` or `patterns`.
    pub patterns: Option<Vec<String>>,
    /// How the patterns are read: "literal" (default), "regex" or "glob" (`*`, `?`, `[a-z]`).
    pub syntax: Option<PatternSyntax>,
    /// Older form of `syntax: "regex"`: if true, the patterns are regular expressions.
    pub is_regex: Option<bool>,
    /// If true, match against the rendered screen text instead of new output (default false).
    /// The reply then says which row the match is on.
    pub screen_mode: Option<bool>,
    /// If true (stream mode), the reply also shows up to 500 characters of output before the
    /// match and after it (the part after stays unread). Default false.
    pub include_context: Option<bool>,
    /// Maximum time to wait in milliseconds (default 10000, at most 120000).
    pub timeout_ms: Option<u64>,
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_wait_stable` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiWaitStableParams {
    /// How long output must stay quiet, in milliseconds (default 100).
    pub quiet_period_ms: Option<u64>,
    /// Maximum time to wait in milliseconds (default 3000, at most 120000).
    pub max_wait_ms: Option<u64>,
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_run_script` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiRunScriptParams {
    /// Shell commands to run in order.
    pub commands: Vec<String>,
    /// Prompt that the shell prints when a command finishes (default "$").
    pub prompt_pattern: Option<String>,
    /// How `prompt_pattern` is read: "literal" (default), "regex" or "glob".
    pub syntax: Option<PatternSyntax>,
    /// Older form of `syntax: "regex"`: if true, `prompt_pattern` is a regular expression.
    pub is_regex: Option<bool>,
    /// Maximum time to wait for each command's prompt, in milliseconds (default 30000, at most 120000).
    pub timeout_ms: Option<u64>,
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_read` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiReadParams {
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_end` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiEndParams {
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_wait_gone` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiWaitGoneParams {
    /// Text or regex that should disappear from the screen (e.g. "Loading"). Give either
    /// `pattern` or `patterns`.
    pub pattern: Option<String>,
    /// Several patterns: waits until none of them is on screen. Give either `pattern` or
    /// `patterns`.
    pub patterns: Option<Vec<String>>,
    /// How the patterns are read: "literal" (default), "regex" or "glob" (`*`, `?`, `[a-z]`).
    pub syntax: Option<PatternSyntax>,
    /// Older form of `syntax: "regex"`: if true, the patterns are regular expressions.
    pub is_regex: Option<bool>,
    /// Maximum time to wait in milliseconds (default 10000, at most 120000).
    pub timeout_ms: Option<u64>,
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_wait_exit` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiWaitExitParams {
    /// Maximum time to wait in milliseconds (default 10000, at most 120000).
    pub timeout_ms: Option<u64>,
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_list_sessions` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiListSessionsParams {}

/// Parameters for `tui_take_screenshot` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[schemars(transform = crate::schema::without_null)]
pub struct TuiScreenshotParams {
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
    /// Format of screenshot: "png" (default) or "svg".
    pub format: Option<String>,
    /// Optional absolute path where the screenshot file should be saved.
    pub output_path: Option<String>,
    /// Whether to include the cursor in the screenshot (default true).
    pub include_cursor: Option<bool>,
    /// Scale factor for PNG (1 or 2, default 1).
    pub scale: Option<u8>,
}

/// The `ShadowPTY` MCP Server holding the state manager.
#[derive(Clone)]
pub struct ShadowPtyServer {
    manager: PtyManager,
    /// The live viewer's HTTP server, started on the first `tui_start` with `live: true`.
    live: Arc<tokio::sync::OnceCell<LiveServer>>,
    /// How the person wants sessions captured. Only held briefly, never while the person
    /// answers.
    capture: Arc<tokio::sync::Mutex<Capture>>,
    /// Logs the connected client once, see [`crate::client_log`].
    client_log: Arc<ClientLog>,
}

impl ShadowPtyServer {
    #[must_use]
    pub fn new(manager: PtyManager) -> Self {
        Self {
            manager,
            live: Arc::new(tokio::sync::OnceCell::new()),
            capture: Arc::new(tokio::sync::Mutex::new(Capture::default())),
            client_log: Arc::new(ClientLog::default()),
        }
    }

    /// Puts the default recording and report files in `dir` instead of the working directory,
    /// and gives the person `timeout` to answer the capture form sent as a session starts.
    #[must_use]
    pub fn with_capture(mut self, dir: PathBuf, timeout: Duration) -> Self {
        self.capture = Arc::new(tokio::sync::Mutex::new(Capture::new(Some(dir), timeout)));
        self
    }

    /// Adds a screenshot that was rendered (and saved, if it has a path) to the session report.
    async fn report_screenshot(&self, session_id: &str, shot: ScreenshotTaken<'_>) {
        // The screenshot is taken either way; a session that ended meanwhile just can't log it
        let _ = self
            .manager
            .record_screenshot_session(session_id, &shot)
            .await;
    }

    /// Starts showing a just-started session in the live viewer (starting the viewer if needed)
    /// and returns its link. `command` is the command line shown on the page.
    async fn watch_live(&self, session_id: &str, command: &str) -> anyhow::Result<String> {
        let live = self.live.get_or_try_init(LiveServer::start).await?;
        live.watch(&self.manager, session_id, command).await
    }

    /// A session was started without `live`: stop showing whatever ran under its id before.
    fn forget_live(&self, session_id: &str) {
        if let Some(live) = self.live.get() {
            live.forget(session_id);
        }
    }

    /// Opens the live page of `session_id` in the person's browser, the first time only.
    async fn open_live(&self, session_id: &str, url: &str) -> Option<std::io::Result<()>> {
        let first = self.capture.lock().await.first_open(session_id);
        first.then(|| capture::open_in_browser(url))
    }
}

impl ShadowPtyServer {
    /// `tui_start` without a client: never asks the person and captures only what `params`
    /// asks for.
    pub async fn start(&self, params: TuiStartParams) -> CallToolResult {
        self.start_with(params, None).await
    }

    /// `tui_end` without a client: a session still waiting for the person's decision keeps its
    /// files where they are.
    pub async fn end(&self, params: TuiEndParams) -> CallToolResult {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        let settled = self.capture.lock().await.settle(session_id);
        self.finish(session_id, settled).await
    }

    /// Plans the session's capture, starts it, and sends the capture form in the background
    /// when the plan says so. `client` is how to reach the person, if there's a client.
    async fn start_with(
        &self,
        params: TuiStartParams,
        client: Option<(&Peer<RoleServer>, Reach)>,
    ) -> CallToolResult {
        let reach = client.map_or(Reach::Library, |(_, reach)| reach);
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        let requested = Requested {
            record_path: params.record_path.as_deref(),
            report_path: params.report_path.as_deref(),
            live: params.live,
        };
        let mut capture = self.capture.lock().await;
        let plan = capture.plan((session_id, &params.command), &requested, reach);
        let background = match (plan.ask_now, client) {
            (true, Some((peer, _))) => {
                let (done, waiting) = tokio::sync::watch::channel(false);
                capture.asking_in_background(waiting);
                let pending = capture.pending(session_id).cloned();
                pending.map(|pending| (peer.clone(), pending, done, capture.timeout()))
            }
            _ => None,
        };
        drop(capture);

        let result = self.launch(&params, &plan).await;
        if result.is_error == Some(true) {
            // No session to ask about: the next start asks instead
            self.capture.lock().await.abandon(session_id, plan.ask_now);
            return result;
        }
        if let Some((peer, pending, done, timeout)) = background {
            let server = self.clone();
            let ask = BackgroundAsk {
                session_id: session_id.to_string(),
                command_line: command_line(&params.command, &params.args),
                pending,
                timeout,
            };
            tokio::spawn(async move { server.ask_in_background(&peer, ask, done).await });
        }
        result
    }

    /// Sends the capture form while the session runs, then applies the answer: it settles the
    /// session's files at `tui_end`, and opens the live page if the person asked for it.
    async fn ask_in_background(
        &self,
        peer: &Peer<RoleServer>,
        ask: BackgroundAsk,
        done: tokio::sync::watch::Sender<bool>,
    ) {
        let defaults = ask.pending.defaults();
        let answer = capture::ask(
            peer,
            (&ask.pending.command, &defaults),
            capture::Moment::Start,
            ask.timeout,
        )
        .await;
        let live = matches!(&answer, capture::Answer::Chose(choices) if choices.live);
        self.capture.lock().await.answer(answer);
        let _ = done.send(true);
        if live && self.manager.is_session_active(&ask.session_id).await {
            match self.watch_live(&ask.session_id, &ask.command_line).await {
                Ok(url) => {
                    if let Some(Err(e)) = self.open_live(&ask.session_id, &url).await {
                        tracing::warn!("Couldn't open the live page {url}: {e}");
                    }
                }
                Err(e) => tracing::warn!("The live viewer is unavailable: {e:#}"),
            }
        }
    }

    /// What happens to the files of session `session_id` as it ends: the person's decision,
    /// asking for it first when needed. `Err` is the form to return to the client, which calls
    /// `tui_end` again with the answer.
    async fn settle_capture(
        &self,
        session_id: &str,
        reach: Reach,
        client: (&Peer<RoleServer>, &Retry),
    ) -> Result<Option<capture::Settled>, InputRequiredResult> {
        let (peer, retry) = client;
        let mut capture = self.capture.lock().await;
        let Some(pending) = capture.pending(session_id).cloned() else {
            return Ok(None);
        };
        // A background form is still out: wait for it (at most its timeout), without the lock
        if let Some(mut done) = capture.waiting() {
            let limit = capture.timeout() + BACKGROUND_GRACE;
            drop(capture);
            let _ = tokio::time::timeout(limit, done.wait_for(|done| *done)).await;
            capture = self.capture.lock().await;
        }
        if !capture.decided() && capture.waiting().is_none() {
            let defaults = pending.defaults();
            let answer = match reach {
                Reach::Library | Reach::NoForms => Some(capture::Answer::Unsupported),
                Reach::Background => {
                    let timeout = capture.timeout();
                    drop(capture);
                    let answer = capture::ask(
                        peer,
                        (&pending.command, &defaults),
                        capture::Moment::End,
                        timeout,
                    )
                    .await;
                    capture = self.capture.lock().await;
                    Some(answer)
                }
                Reach::AtEnd => match retry.response() {
                    Some(response) => Some(capture::read_response(response, &defaults)),
                    // The client came back from the form without an answer: it was closed
                    None if retry.after_form() => Some(capture::Answer::Dismissed),
                    // Another session's form is out: this one keeps the defaults
                    None if capture.form_out() => None,
                    None => match capture::input_request(&pending.command, &defaults) {
                        Ok(form) => {
                            capture.form_sent();
                            drop(capture);
                            return Err(form);
                        }
                        Err(e) => {
                            tracing::warn!("Can't build the capture form: {e}");
                            Some(capture::Answer::Unsupported)
                        }
                    },
                },
            };
            if let Some(answer) = answer {
                capture.answer(answer);
            }
        }
        let settled = capture.settle(session_id);
        drop(capture);
        Ok(settled)
    }

    /// Stops the session, then moves or deletes its files as `settled` says.
    async fn finish(&self, session_id: &str, settled: Option<capture::Settled>) -> CallToolResult {
        if let Some(live) = self.live.get() {
            live.capture_final_frame(&self.manager, session_id).await;
        }
        let stopped = match self.manager.stop_session(session_id).await {
            Ok(stopped) => stopped,
            Err(e) => return error_result(format!("Failed to terminate session: {e:#}")),
        };
        let pid_str = stopped
            .info
            .pid
            .map_or_else(|| "unknown".to_string(), |p| p.to_string());
        let cmd = &stopped.info.command;
        let mut msg =
            format!("Terminated session '{session_id}' for command '{cmd}' (pid: {pid_str})");
        let mut report_path = stopped.report_path.clone();
        if let Some(settled) = settled {
            let (done, problems) = capture::carry_out(settled).await;
            match done.record.kept_at() {
                Some(path) => {
                    let _ = write!(msg, ". Recording saved to '{path}'");
                }
                None => msg.push_str(". Recording deleted, as the person chose"),
            }
            report_path = done.report.kept_at().map(str::to_string);
            if report_path.is_none() {
                msg.push_str(". Report deleted, as the person chose");
            }
            for problem in problems {
                let _ = write!(msg, ". Note: {problem}");
            }
        }
        if let Some(path) = &report_path {
            let _ = write!(msg, ". Report '{path}': {}", stopped.totals);
        }
        text_result(msg)
    }

    /// Starts the session, captured as `plan` says.
    async fn launch(&self, params: &TuiStartParams, plan: &capture::Plan) -> CallToolResult {
        let rows = params.rows.unwrap_or(24);
        let cols = params.cols.unwrap_or(80);
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        let config = crate::pty_manager::PtyConfig::new(&params.command, &params.args, rows, cols)
            .with_record_path(plan.record_path.as_deref())
            .with_report_path(plan.report_path.as_deref());

        let info = match self.manager.start_session(session_id, &config).await {
            Ok(info) => info,
            Err(e) => {
                return error_result(format!("Failed to start process: {e:#}"));
            }
        };
        let pid_str = info
            .pid
            .map_or_else(|| "unknown".to_string(), |p| p.to_string());
        let cmd = &info.command;
        let rows = info.rows;
        let cols = info.cols;
        let recording_str = plan
            .record_path
            .as_ref()
            .map_or_else(String::new, |path| format!(", recording to '{path}'"));
        let reporting_str = plan
            .report_path
            .as_ref()
            .map_or_else(String::new, |path| format!(", reporting to '{path}'"));
        let mut msg = format!(
            "Started command '{cmd}' in PTY session '{session_id}' (pid: {pid_str}, rows: {rows}, cols: {cols}{recording_str}{reporting_str})"
        );
        if plan.live {
            let command = command_line(&params.command, &params.args);
            match self.watch_live(session_id, &command).await {
                Ok(url) => {
                    let _ = write!(msg, ", watch live at {url}");
                    if plan.open_browser {
                        match self.open_live(session_id, &url).await {
                            Some(Ok(())) => msg.push_str(" (opened in the person's browser)"),
                            Some(Err(e)) => {
                                let _ = write!(
                                    msg,
                                    " (couldn't open the browser: {e}; give the person the link)"
                                );
                            }
                            None => {}
                        }
                    }
                }
                Err(e) => {
                    let _ = write!(msg, ", but the live viewer is unavailable: {e:#}");
                }
            }
        } else {
            self.forget_live(session_id);
        }
        if let Some(note) = plan.note {
            msg.push('\n');
            msg.push_str(match note {
                Note::Defaults => {
                    "These are the default files. ShadowPTY can also record elsewhere (record_path, report_path) or show the session to the person in their browser as it runs (live: true). Ask the person whether they want any of these."
                }
                Note::AskingNow => {
                    "The person is being asked how to keep this session's recording and report, and whether to watch it live. Until they answer, both go to the files above, and they're kept if no answer comes."
                }
                Note::AskAtEnd => {
                    "When this session ends, tui_end asks the person whether to keep the recording and report and where; until then both go to the files above. To let the person watch live, ask them and start with live: true."
                }
            });
        }
        text_result(msg)
    }
}

/// Extra time to wait for a background form past its own timeout, for the answer to land.
const BACKGROUND_GRACE: Duration = Duration::from_secs(2);

/// A capture form sent in the background as session `session_id` starts.
struct BackgroundAsk {
    session_id: String,
    /// The command line, as shown on the live page.
    command_line: String,
    pending: capture::Pending,
    timeout: Duration,
}

/// How the client can reach the person, from its capabilities and protocol.
fn reach(context: &RequestContext<RoleServer>) -> Reach {
    if !capture::supports_forms(context.client_capabilities().as_ref()) {
        Reach::NoForms
    } else if context
        .protocol_version()
        .is_some_and(|v| v.as_str() >= ProtocolVersion::V_2026_07_28.as_str())
    {
        Reach::AtEnd
    } else {
        Reach::Background
    }
}

/// What a client sends when it calls `tui_end` again after the capture form (protocol
/// 2026-07-28): the person's answer, and the request state returned with the form.
pub struct Retry {
    responses: Option<rmcp::model::InputResponses>,
    state: Option<String>,
}

impl Retry {
    /// The answer to the capture form, if the client sent one.
    fn response(&self) -> Option<&serde_json::Value> {
        self.responses.as_ref()?.get(capture::INPUT_KEY)
    }

    /// Whether this call follows the capture form.
    fn after_form(&self) -> bool {
        self.state.as_deref() == Some(capture::FORM_STATE)
    }
}

impl<S> FromContextPart<ToolCallContext<'_, S>> for Retry {
    fn from_context_part(context: &mut ToolCallContext<'_, S>) -> Result<Self, rmcp::ErrorData> {
        Ok(Self {
            responses: context.input_responses.take(),
            state: context.request_state.take(),
        })
    }
}

/// The command line as shown to the person watching.
fn command_line(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

#[tool_router]
impl ShadowPtyServer {
    /// Spawns a new process in a native pseudo-terminal (PTY) and initializes the screen buffer.
    #[tool(
        name = "tui_start",
        title = "Start TUI session",
        annotations(
            title = "Start TUI session",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        ),
        description = "Spawns a command in a native pseudo-terminal (PTY) and initializes screen tracking. Optionally records to an asciicast v3 file (record_path) and writes a JSON Lines report of every check and its result (report_path). Use session_id to run several sessions at once. With live: true, the reply includes a local link where a person can watch the session live in a browser (view-only); give them the link, don't open it. When record_path, report_path and live are all left unset, the session is recorded with a report to default files, and the person decides (in the background, or when tui_end is called) whether to keep them and where; their answer applies to later sessions too. tui_start never waits for the person."
    )]
    pub async fn tui_start(
        &self,
        Parameters(params): Parameters<TuiStartParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // Clients on 2026-07-28 skip `initialize` and send who they are with each request
        self.client_log.once(ClientSeen {
            protocol: context.protocol_version().as_ref(),
            client: context.client_info().as_ref(),
            capabilities: context.client_capabilities().as_ref(),
        });
        Ok(self
            .start_with(params, Some((&context.peer, reach(&context))))
            .await)
    }

    /// Sends raw keystrokes or symbolic tokens (<ENTER>, <UP>, <CTRL+C>, etc.) to the PTY.
    #[tool(
        name = "tui_input",
        title = "Send keys",
        annotations(
            title = "Send keys",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        ),
        description = "Sends keystrokes to the running TUI application. Supports tokens like <ENTER>, <ESC>, <UP>, <DOWN>, <CTRL+C>, etc."
    )]
    pub async fn tui_input(
        &self,
        Parameters(params): Parameters<TuiInputParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        match self
            .manager
            .send_input_session(session_id, &params.keys)
            .await
        {
            Ok(bytes_written) => {
                let msg = format!(
                    "Sent {} bytes to session '{}' for input {:?}",
                    bytes_written, session_id, params.keys
                );
                Ok(CallToolResult::success(vec![
                    rmcp::model::ContentBlock::text(msg),
                ]))
            }
            Err(e) => Ok(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(format!("Failed to send input: {e:#}")),
            ])),
        }
    }

    /// Sends text as a single bracketed paste.
    #[tool(
        name = "tui_paste",
        title = "Paste text",
        annotations(
            title = "Paste text",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        ),
        description = "Pastes text (e.g. a multiline script) using bracketed paste mode, so shells and editors receive it as one paste instead of typed keys."
    )]
    pub async fn tui_paste(
        &self,
        Parameters(params): Parameters<TuiPasteParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        Ok(
            match self
                .manager
                .send_paste_session(session_id, &params.text)
                .await
            {
                Ok(bytes) => text_result(format!("Pasted {bytes} bytes to session '{session_id}'")),
                Err(e) => error_result(format!("Failed to paste text: {e:#}")),
            },
        )
    }

    /// Waits for a literal or regex pattern in new output or on screen.
    #[tool(
        name = "tui_expect",
        title = "Wait for text",
        annotations(
            title = "Wait for text",
            read_only_hint = true,
            open_world_hint = false
        ),
        description = "Waits until a literal or regex pattern appears, instead of sleeping and polling tui_read. With `patterns`, waits for whichever appears first and says which one matched. By default it searches output that neither tui_read nor an earlier tui_expect has returned yet; with screen_mode it searches the rendered screen text. Fails early if the process exits."
    )]
    pub async fn tui_expect(
        &self,
        Parameters(params): Parameters<TuiExpectParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        let ToolPatterns { sources, compiled } =
            match resolve_syntax(params.syntax, params.is_regex)
                .and_then(|syntax| parse_patterns(params.pattern, params.patterns, syntax))
            {
                Ok(parsed) => parsed,
                Err(message) => return Ok(error_result(message)),
            };
        let expectation = Expectation {
            patterns: compiled,
            target: if params.screen_mode.unwrap_or(false) {
                ExpectTarget::Screen
            } else {
                ExpectTarget::Stream
            },
            timeout: wait_duration(params.timeout_ms, 10_000),
        };

        Ok(
            match self.manager.expect_session(session_id, &expectation).await {
                Ok(found) => text_result(describe_match(
                    &found,
                    &sources,
                    session_id,
                    params.include_context.unwrap_or(false),
                )),
                Err(e) => error_result(format!("Expect failed: {e:#}")),
            },
        )
    }

    /// Waits until text disappears from the screen.
    #[tool(
        name = "tui_wait_gone",
        title = "Wait for text to disappear",
        annotations(
            title = "Wait for text to disappear",
            read_only_hint = true,
            open_world_hint = false
        ),
        description = "Waits until a literal or regex pattern is no longer on the rendered screen, e.g. a spinner or \"Loading...\" message, and reports how long that took. With `patterns`, waits until none of them is on screen. Returns at once if the text isn't showing, so if it may not have appeared yet, wait for it first with tui_expect (screen_mode). Fails early if the process exits with it still on screen."
    )]
    pub async fn tui_wait_gone(
        &self,
        Parameters(params): Parameters<TuiWaitGoneParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        let ToolPatterns { sources, compiled } =
            match resolve_syntax(params.syntax, params.is_regex)
                .and_then(|syntax| parse_patterns(params.pattern, params.patterns, syntax))
            {
                Ok(parsed) => parsed,
                Err(message) => return Ok(error_result(message)),
            };
        let timeout = wait_duration(params.timeout_ms, 10_000);

        Ok(
            match self
                .manager
                .wait_gone_session(session_id, &compiled, timeout)
                .await
            {
                Ok(elapsed) => {
                    let what = if sources.len() == 1 {
                        format!("'{}' is", sources[0])
                    } else {
                        format!("{} patterns are", sources.len())
                    };
                    text_result(format!(
                        "{what} no longer on screen in session '{session_id}' (after {} ms)",
                        elapsed.as_millis()
                    ))
                }
                Err(e) => error_result(format!("Wait gone failed: {e:#}")),
            },
        )
    }

    /// Waits for the process to exit and reports its exit code or signal.
    #[tool(
        name = "tui_wait_exit",
        title = "Wait for exit",
        annotations(
            title = "Wait for exit",
            read_only_hint = true,
            open_world_hint = false
        ),
        description = "Waits until the session's process exits and reports how it ended (exit code or signal), plus any output not yet returned by tui_read or tui_expect. Returns immediately if it has already exited. The session stays open for tui_read and screenshots until tui_end."
    )]
    pub async fn tui_wait_exit(
        &self,
        Parameters(params): Parameters<TuiWaitExitParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        let timeout = wait_duration(params.timeout_ms, 10_000);

        Ok(
            match self.manager.wait_exit_session(session_id, timeout).await {
                Ok(exit) => {
                    let mut text = format!("Process in session '{session_id}' {}.", exit.status);
                    if !exit.output.trim().is_empty() {
                        text.push_str("\nUnread output:\n");
                        text.push_str(exit.output.trim_end());
                    }
                    text_result(text)
                }
                Err(e) => error_result(format!("Wait exit failed: {e:#}")),
            },
        )
    }

    /// Waits until the application stops producing output.
    #[tool(
        name = "tui_wait_stable",
        title = "Wait for quiet screen",
        annotations(
            title = "Wait for quiet screen",
            read_only_hint = true,
            open_world_hint = false
        ),
        description = "Waits until the application has produced no output for quiet_period_ms, so the next tui_read sees a finished screen. Returns immediately if the process has exited."
    )]
    pub async fn tui_wait_stable(
        &self,
        Parameters(params): Parameters<TuiWaitStableParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        let quiet_period = wait_duration(params.quiet_period_ms, 100);
        let max_wait = wait_duration(params.max_wait_ms, 3_000);

        Ok(
            match self
                .manager
                .wait_stable_session(session_id, quiet_period, max_wait)
                .await
            {
                Ok(()) => text_result(format!("Screen stable in session '{session_id}'")),
                Err(e) => error_result(format!("Wait stable failed: {e:#}")),
            },
        )
    }

    /// Runs shell commands in order, waiting for the prompt after each.
    #[tool(
        name = "tui_run_script",
        title = "Run shell commands",
        annotations(
            title = "Run shell commands",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        ),
        description = "Runs shell commands one at a time in a shell session, waiting for the prompt after each, and returns each command's output. Output from before the call is ignored, and the echo of each command is skipped. Stops at the first command whose prompt doesn't appear within timeout_ms."
    )]
    pub async fn tui_run_script(
        &self,
        Parameters(params): Parameters<TuiRunScriptParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        let prompt_source = params.prompt_pattern.as_deref().unwrap_or("$");
        let syntax = match resolve_syntax(params.syntax, params.is_regex) {
            Ok(syntax) => syntax,
            Err(message) => return Ok(error_result(message)),
        };
        let prompt = match Pattern::with_syntax(prompt_source, syntax) {
            Ok(prompt) => prompt,
            Err(e) => {
                return Ok(error_result(format!(
                    "Invalid prompt pattern '{prompt_source}': {e}"
                )));
            }
        };
        let script = Script {
            commands: &params.commands,
            prompt,
            timeout_per_command: wait_duration(params.timeout_ms, 30_000),
        };

        let outcome = match self.manager.run_script_session(session_id, &script).await {
            Ok(outcome) => outcome,
            Err(e) => return Ok(error_result(format!("Run script failed: {e:#}"))),
        };

        let mut report = outcome
            .steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                format!(
                    "=== Command {}: {} ===\n{}",
                    i + 1,
                    step.command,
                    step.output
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(match outcome.error {
            None => text_result(report),
            Some(error) => {
                if !report.is_empty() {
                    report.push_str("\n\n");
                }
                report.push_str("Script stopped: ");
                report.push_str(&error);
                error_result(report)
            }
        })
    }

    /// Sends a signal to the session's process without ending the session.
    #[tool(
        name = "tui_signal",
        title = "Send signal",
        annotations(
            title = "Send signal",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        ),
        description = "Sends a signal (INT, TERM, HUP, TSTP, STOP, CONT, USR1, KILL, ...) to the running app without ending the session, e.g. to test that it shuts down cleanly on TERM or reloads on HUP. By default it goes to the terminal's foreground process group, like Ctrl+C would; target \"process\" signals only the process tui_start launched. Unlike <CTRL+C> in tui_input, it works even when the app has turned off keyboard signals (raw mode). Follow up with tui_expect or tui_wait_exit to check the effect."
    )]
    pub async fn tui_signal(
        &self,
        Parameters(params): Parameters<TuiSignalParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        let signal = match parse_signal(&params.signal) {
            Ok(signal) => signal,
            Err(e) => return Ok(error_result(format!("{e:#}"))),
        };
        let target = params
            .target
            .map_or(SignalTarget::Foreground, SignalTarget::from);
        let name =
            signal_name(signal.as_raw()).unwrap_or_else(|| format!("signal {}", signal.as_raw()));

        Ok(
            match self
                .manager
                .signal_session(session_id, signal, target)
                .await
            {
                Ok(delivery) => {
                    let whom = match delivery.target {
                        SignalTarget::Foreground => {
                            format!("the foreground process group ({})", delivery.id)
                        }
                        SignalTarget::Process => format!("process {}", delivery.id),
                    };
                    text_result(format!("Sent {name} to {whom} in session '{session_id}'"))
                }
                Err(e) => error_result(format!("Failed to send {name}: {e:#}")),
            },
        )
    }

    /// Resizes the pseudo-terminal window and updates screen parser dimensions.
    #[tool(
        name = "tui_resize",
        title = "Resize terminal",
        annotations(
            title = "Resize terminal",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        ),
        description = "Resizes the pseudo-terminal window and screen grid to test TUI responsive layout."
    )]
    pub async fn tui_resize(
        &self,
        Parameters(params): Parameters<TuiResizeParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        match self
            .manager
            .resize_session(session_id, params.rows, params.cols)
            .await
        {
            Ok((rows, cols)) => {
                let msg =
                    format!("Resized terminal session '{session_id}' to {rows} rows x {cols} cols");
                Ok(CallToolResult::success(vec![
                    rmcp::model::ContentBlock::text(msg),
                ]))
            }
            Err(e) => Ok(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(format!("Failed to resize terminal: {e:#}")),
            ])),
        }
    }

    /// Reads the current TUI screen state, preserving layout, colors, and text attributes.
    #[tool(
        name = "tui_read",
        title = "Read screen",
        annotations(title = "Read screen", read_only_hint = true, open_world_hint = false),
        description = "Reads the current screen state formatted with semantic tags (<fg:...>, <bg:...>, <bold>, etc.). Output shown on this screen counts as seen: a later tui_expect only matches newer output."
    )]
    pub async fn tui_read(
        &self,
        Parameters(params): Parameters<TuiReadParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        match self.manager.read_screen_session(session_id).await {
            Ok(screen_text) => Ok(CallToolResult::success(vec![
                rmcp::model::ContentBlock::text(screen_text),
            ])),
            Err(e) => Ok(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(format!("Failed to read screen: {e:#}")),
            ])),
        }
    }

    /// Lists all currently active pseudo-terminal sessions.
    #[tool(
        name = "tui_list_sessions",
        title = "List sessions",
        annotations(
            title = "List sessions",
            read_only_hint = true,
            open_world_hint = false
        ),
        description = "Lists all active PTY sessions with their process id, dimensions, recording state, and exit_status (null while running, otherwise {\"exit_code\": N}, {\"signal\": N} or \"unknown\")."
    )]
    pub async fn tui_list_sessions(
        &self,
        Parameters(_): Parameters<TuiListSessionsParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let sessions = self.manager.list_sessions().await;
        match serde_json::to_string_pretty(&sessions) {
            Ok(json_str) => Ok(CallToolResult::success(vec![
                rmcp::model::ContentBlock::text(json_str),
            ])),
            Err(e) => Ok(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(format!("Failed to serialize sessions: {e:#}")),
            ])),
        }
    }

    /// Terminates a pseudo-terminal session, killing the child process and releasing resources.
    #[tool(
        name = "tui_end",
        title = "End session",
        annotations(
            title = "End session",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        ),
        description = "Terminates a pseudo-terminal session, killing the running program and releasing resources. If the session writes a report, finishes it and says how many checks passed and failed. If the person hasn't yet decided how to keep the session's recording and report, they may be asked now; the reply says where the files went."
    )]
    pub async fn tui_end(
        &self,
        Parameters(params): Parameters<TuiEndParams>,
        context: RequestContext<RoleServer>,
        retry: Retry,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        let settled = match self
            .settle_capture(session_id, reach(&context), (&context.peer, &retry))
            .await
        {
            Ok(settled) => settled,
            Err(form) => return Ok(form.into()),
        };
        Ok(self.finish(session_id, settled).await.into())
    }

    /// Takes a screenshot of the current screen state in PNG (or SVG) format.
    #[tool(
        name = "tui_take_screenshot",
        title = "Take screenshot",
        annotations(
            title = "Take screenshot",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        ),
        description = "Takes a screenshot of the current terminal screen. Format can be 'png' (default) or 'svg'. If output_path is specified (must be absolute), writes the file to disk; otherwise returns the content directly (base64 PNG image or SVG text)."
    )]
    pub async fn tui_take_screenshot(
        &self,
        Parameters(params): Parameters<TuiScreenshotParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        let format = params.format.as_deref().unwrap_or("png");

        if format != "svg" && format != "png" {
            return Ok(error_result(format!(
                "Unsupported screenshot format '{format}'. Supported formats: 'png', 'svg'"
            )));
        }

        if let Some(ref path_str) = params.output_path {
            let path = std::path::Path::new(path_str);
            if !path.is_absolute() {
                return Ok(error_result(format!(
                    "output_path must be an absolute path: '{path_str}'"
                )));
            }
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
                && !parent.exists()
            {
                return Ok(error_result(format!(
                    "Parent directory does not exist for output_path: '{}'",
                    parent.display()
                )));
            }
        }

        let mut snapshot = match self.manager.snapshot_session(session_id).await {
            Ok(snap) => snap,
            Err(e) => return Ok(error_result(format!("Failed to take snapshot: {e:#}"))),
        };

        if params.include_cursor == Some(false) {
            snapshot.cursor = None;
        }

        if format == "png" {
            let scale = params.scale.unwrap_or(1);
            let options = crate::rasterizer::PngOptions {
                scale,
                cursor_color: crate::palette::DEFAULT_FOREGROUND,
            };
            let png_bytes = match crate::rasterizer::render_png(&snapshot, options) {
                Ok(bytes) => bytes,
                Err(e) => {
                    return Ok(error_result(format!(
                        "Failed to render PNG screenshot: {e:#}"
                    )));
                }
            };

            let shot = ScreenshotTaken {
                format,
                path: params.output_path.as_deref(),
                bytes: png_bytes.len(),
            };
            if let Some(ref path_str) = params.output_path {
                match tokio::fs::write(path_str, &png_bytes).await {
                    Ok(()) => {
                        self.report_screenshot(session_id, shot).await;
                        let scale_usize = usize::from(options.scale.clamp(1, 4));
                        let pixel_w = usize::from(snapshot.cols) * (9 * scale_usize);
                        let pixel_h = usize::from(snapshot.rows) * (18 * scale_usize);
                        let bytes = png_bytes.len();
                        let msg = format!(
                            "Saved PNG screenshot to '{path_str}' ({}x{} cells, {pixel_w}x{pixel_h} px, {bytes} bytes)",
                            snapshot.cols, snapshot.rows
                        );
                        Ok(text_result(msg))
                    }
                    Err(e) => Ok(error_result(format!(
                        "Failed to write screenshot to '{path_str}': {e:#}"
                    ))),
                }
            } else {
                self.report_screenshot(session_id, shot).await;
                let b64 = crate::rasterizer::png_to_base64(&png_bytes);
                Ok(image_result(b64, "image/png"))
            }
        } else {
            let theme = crate::screenshot::Theme::default();
            let svg = crate::screenshot::render_svg(&snapshot, &theme);
            let shot = ScreenshotTaken {
                format,
                path: params.output_path.as_deref(),
                bytes: svg.len(),
            };

            if let Some(ref path_str) = params.output_path {
                match tokio::fs::write(path_str, svg.as_bytes()).await {
                    Ok(()) => {
                        self.report_screenshot(session_id, shot).await;
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let pixel_w = (f64::from(snapshot.cols) * theme.cell_width) as usize;
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let pixel_h = (f64::from(snapshot.rows) * theme.cell_height) as usize;
                        let bytes = svg.len();
                        let msg = format!(
                            "Saved SVG screenshot to '{path_str}' ({}x{} cells, {pixel_w}x{pixel_h} px, {bytes} bytes)",
                            snapshot.cols, snapshot.rows
                        );
                        Ok(text_result(msg))
                    }
                    Err(e) => Ok(error_result(format!(
                        "Failed to write screenshot to '{path_str}': {e:#}"
                    ))),
                }
            } else {
                self.report_screenshot(session_id, shot).await;
                Ok(text_result(svg))
            }
        }
    }
}

#[tool_handler(
    name = "shadowpty",
    instructions = "ShadowPTY runs terminal (TUI) programs headlessly so you can drive and check them. \
A session starts with tui_start and stays open until tui_end; several can run at once under different session_id values. \
Send keys with tui_input (tokens like <ENTER>, <UP>, <CTRL+C>) or text with tui_paste. \
Never sleep: wait with tui_expect (text appears), tui_wait_gone (text disappears), tui_wait_stable (output goes quiet) or tui_wait_exit (process ends). \
Look at the screen with tui_read (text with color and style tags) or tui_take_screenshot. \
For shell sessions, tui_run_script runs commands one at a time and returns each one's output. \
Recordings, reports and the live viewer link are for the person: pass their paths and links on to them. \
End every session you start with tui_end."
)]
impl ServerHandler for ShadowPtyServer {
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, rmcp::ErrorData> {
        context.peer.set_peer_info(request.clone());
        let result = self.negotiate_initialize(&request)?;
        self.client_log.once(ClientSeen {
            protocol: Some(&result.protocol_version),
            client: Some(&request.client_info),
            capabilities: Some(&request.capabilities),
        });
        Ok(result)
    }

    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, rmcp::ErrorData> {
        self.client_log.once(ClientSeen {
            protocol: context.protocol_version().as_ref(),
            client: context.client_info().as_ref(),
            capabilities: context.client_capabilities().as_ref(),
        });
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_declares_title_and_hints() {
        let tools = ShadowPtyServer::tool_router().list_all();
        assert_eq!(tools.len(), 14);
        for tool in tools {
            let name = &tool.name;
            assert!(
                tool.title.as_deref().is_some_and(|t| !t.is_empty()),
                "{name}: title"
            );
            let a = tool.annotations.clone().unwrap_or_default();
            assert!(
                a.title.as_deref().is_some_and(|t| !t.is_empty()),
                "{name}: annotations.title"
            );
            assert!(a.open_world_hint.is_some(), "{name}: openWorldHint");
            assert!(a.read_only_hint.is_some(), "{name}: readOnlyHint");
            if a.read_only_hint == Some(false) {
                assert!(a.destructive_hint.is_some(), "{name}: destructiveHint");
                assert!(a.idempotent_hint.is_some(), "{name}: idempotentHint");
            }
        }
    }

    #[test]
    fn mcpb_manifest_lists_every_tool() {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../mcpb/manifest.json")).unwrap_or_default();
        let mut listed: Vec<&str> = manifest["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["name"].as_str())
            .collect();
        let tools = ShadowPtyServer::tool_router().list_all();
        let mut served: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        listed.sort_unstable();
        served.sort_unstable();
        assert_eq!(listed, served);
    }

    /// Every `type` array and `anyOf` in `schema`, with where they are.
    fn nullables(schema: &serde_json::Value, at: &str, found: &mut Vec<String>) {
        match schema {
            serde_json::Value::Object(map) => {
                let null_type = map
                    .get("type")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|types| types.iter().any(|t| t == "null"));
                let null_branch = map
                    .get("anyOf")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|branches| branches.iter().any(|b| b["type"] == "null"));
                if null_type || null_branch {
                    found.push(at.to_string());
                }
                for (key, value) in map {
                    nullables(value, &format!("{at}.{key}"), found);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    nullables(item, at, found);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn input_schemas_have_no_null_types() {
        let mut found = Vec::new();
        for tool in ShadowPtyServer::tool_router().list_all() {
            let schema = serde_json::Value::Object((*tool.input_schema).clone());
            nullables(&schema, &tool.name, &mut found);
        }
        assert!(found.is_empty(), "nullable parameters: {found:?}");
    }

    #[test]
    fn server_info_carries_instructions() {
        let server = ShadowPtyServer::new(PtyManager::new());
        let info = server.get_info();
        assert_eq!(info.server_info.name, "shadowpty");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.instructions.is_some_and(|i| i.contains("tui_start")));
    }
}
