//! MCP Tool Router implementation for `ShadowPTY`.

use std::fmt::Write as _;
use std::time::Duration;

use rmcp::{
    handler::server::wrapper::Parameters,
    model::CallToolResult,
    schemars::{self, JsonSchema},
    tool, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::output::{Pattern, Syntax};
use crate::pty_manager::{
    DEFAULT_SESSION_ID, ExpectMatch, ExpectTarget, Expectation, PtyManager, Script,
};

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
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
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
    /// Identifier for this session (defaults to "default"). Starting an existing id replaces that session.
    pub session_id: Option<String>,
}

/// Parameters for `tui_input` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TuiInputParams {
    /// String containing keystrokes and symbolic tokens (e.g. "<ENTER>", "<ESC>", "<UP>", "<CTRL+C>").
    pub keys: String,
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_resize` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
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
pub struct TuiPasteParams {
    /// Text to send as a single bracketed paste (DECSET 2004).
    pub text: String,
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_expect` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
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
pub struct TuiReadParams {
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_end` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct TuiEndParams {
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_wait_gone` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
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
pub struct TuiWaitExitParams {
    /// Maximum time to wait in milliseconds (default 10000, at most 120000).
    pub timeout_ms: Option<u64>,
    /// Target session identifier (defaults to "default").
    pub session_id: Option<String>,
}

/// Parameters for `tui_list_sessions` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct TuiListSessionsParams {}

/// Parameters for `tui_take_screenshot` tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
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
}

impl ShadowPtyServer {
    #[must_use]
    pub const fn new(manager: PtyManager) -> Self {
        Self { manager }
    }
}

#[tool_router(server_handler)]
impl ShadowPtyServer {
    /// Spawns a new process in a native pseudo-terminal (PTY) and initializes the screen buffer.
    #[tool(
        name = "tui_start",
        description = "Spawns a command in a native pseudo-terminal (PTY) and initializes screen tracking. Optionally records to an asciicast v3 file. Use session_id to run several sessions at once."
    )]
    pub async fn tui_start(
        &self,
        Parameters(params): Parameters<TuiStartParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let rows = params.rows.unwrap_or(24);
        let cols = params.cols.unwrap_or(80);
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        let config = crate::pty_manager::PtyConfig::new(&params.command, &params.args, rows, cols)
            .with_record_path(params.record_path.as_deref());

        match self.manager.start_session(session_id, &config).await {
            Ok(info) => {
                let pid_str = info
                    .pid
                    .map_or_else(|| "unknown".to_string(), |p| p.to_string());
                let cmd = &info.command;
                let rows = info.rows;
                let cols = info.cols;
                let recording_str = params
                    .record_path
                    .as_ref()
                    .map_or_else(String::new, |path| format!(", recording to '{path}'"));
                let msg = format!(
                    "Started command '{cmd}' in PTY session '{session_id}' (pid: {pid_str}, rows: {rows}, cols: {cols}{recording_str})"
                );
                Ok(CallToolResult::success(vec![
                    rmcp::model::ContentBlock::text(msg),
                ]))
            }
            Err(e) => Ok(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(format!("Failed to start process: {e:#}")),
            ])),
        }
    }

    /// Sends raw keystrokes or symbolic tokens (<ENTER>, <UP>, <CTRL+C>, etc.) to the PTY.
    #[tool(
        name = "tui_input",
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

    /// Resizes the pseudo-terminal window and updates screen parser dimensions.
    #[tool(
        name = "tui_resize",
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
        description = "Terminates a pseudo-terminal session, killing the running program and releasing resources."
    )]
    pub async fn tui_end(
        &self,
        Parameters(params): Parameters<TuiEndParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let session_id = params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID);
        match self.manager.stop_session(session_id).await {
            Ok(info) => {
                let pid_str = info
                    .pid
                    .map_or_else(|| "unknown".to_string(), |p| p.to_string());
                let cmd = &info.command;
                let msg = format!(
                    "Terminated session '{session_id}' for command '{cmd}' (pid: {pid_str})"
                );
                Ok(CallToolResult::success(vec![
                    rmcp::model::ContentBlock::text(msg),
                ]))
            }
            Err(e) => Ok(CallToolResult::error(vec![
                rmcp::model::ContentBlock::text(format!("Failed to terminate session: {e:#}")),
            ])),
        }
    }

    /// Takes a screenshot of the current screen state in PNG (or SVG) format.
    #[tool(
        name = "tui_take_screenshot",
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

            if let Some(ref path_str) = params.output_path {
                match tokio::fs::write(path_str, &png_bytes).await {
                    Ok(()) => {
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
                let b64 = crate::rasterizer::png_to_base64(&png_bytes);
                Ok(image_result(b64, "image/png"))
            }
        } else {
            let theme = crate::screenshot::Theme::default();
            let svg = crate::screenshot::render_svg(&snapshot, &theme);

            if let Some(ref path_str) = params.output_path {
                match tokio::fs::write(path_str, svg.as_bytes()).await {
                    Ok(()) => {
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
                Ok(text_result(svg))
            }
        }
    }
}
