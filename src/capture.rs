//! Asking the person how sessions are captured: recorded, reported, watched live.
//!
//! The first `tui_start` that leaves `record_path`, `report_path` and `live` unset asks the
//! person once, through an MCP elicitation form the client shows them. The answer applies to
//! every later session of the server. When the client can't show forms, or the person
//! dismisses the form, `tui_start` tells the agent once that these options exist instead.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{
    ClientCapabilities, ElicitRequest, ElicitRequestParams, ElicitResult, ElicitationAction,
    ElicitationSchema, InputRequest, InputRequiredResult,
};
use rmcp::{Peer, RoleServer};
use serde_json::Value;

/// How long the person has to answer the form before the session starts without it.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(600);

/// What the agent passed to `tui_start`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Requested<'a> {
    pub record_path: Option<&'a str>,
    pub report_path: Option<&'a str>,
    pub live: Option<bool>,
}

impl Requested<'_> {
    /// The agent chose nothing, so the person may be asked.
    const fn is_unset(&self) -> bool {
        self.record_path.is_none() && self.report_path.is_none() && self.live.is_none()
    }
}

/// How one session is captured, once the agent's parameters and the person's answer are merged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub record_path: Option<String>,
    pub report_path: Option<String>,
    pub live: bool,
    /// Open the live page in the person's browser (only when `live`).
    pub open_browser: bool,
    /// Tell the agent the person can be offered recording, a report or the live view.
    pub hint: bool,
}

/// The person's answer to the form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Choices {
    pub record: Option<String>,
    pub report: Option<String>,
    pub live: bool,
    pub open_browser: bool,
}

/// How asking went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Chose(Choices),
    /// The person said no: capture nothing, don't remind them.
    Declined,
    /// The person closed the form or didn't answer in time.
    Dismissed,
    /// The client can't show forms.
    Unsupported,
}

/// A file the person chose. The first session writes to it; later ones write next to it.
#[derive(Debug)]
struct Slot {
    path: String,
    used: bool,
}

impl Slot {
    const fn new(path: String) -> Self {
        Self { path, used: false }
    }

    fn take(&mut self) -> String {
        if self.used {
            unused_sibling(&self.path)
        } else {
            self.used = true;
            self.path.clone()
        }
    }
}

#[derive(Debug, Default)]
enum State {
    #[default]
    NotAsked,
    Answered {
        record: Option<Slot>,
        report: Option<Slot>,
        live: bool,
        open_browser: bool,
    },
    /// The person couldn't be asked; `hinted` once the agent was told about the options.
    Unanswered { hinted: bool },
}

/// The person's capture choices for the life of the server.
#[derive(Debug, Default)]
pub struct Capture {
    state: State,
    /// Sessions whose live page was opened in the browser. Restarting one switches the open
    /// page to the new session, so it isn't opened again.
    opened: HashSet<String>,
    /// The form went to the client as an input request (protocol 2026-07-28) and no answer
    /// came back yet. Other starts don't send it again while it's out.
    form_out: bool,
}

impl Capture {
    /// Whether this `tui_start` should ask the person.
    #[must_use]
    pub const fn should_ask(&self, requested: &Requested<'_>) -> bool {
        matches!(self.state, State::NotAsked) && requested.is_unset()
    }

    /// Remembers how asking went.
    pub fn answer(&mut self, answer: Answer) {
        self.form_out = false;
        self.state = match answer {
            Answer::Chose(choices) => State::Answered {
                record: choices.record.map(Slot::new),
                report: choices.report.map(Slot::new),
                live: choices.live,
                open_browser: choices.open_browser,
            },
            Answer::Declined => State::Answered {
                record: None,
                report: None,
                live: false,
                open_browser: false,
            },
            Answer::Dismissed | Answer::Unsupported => State::Unanswered { hinted: false },
        };
    }

    /// How the session starting now is captured. What the agent passed wins; the person's
    /// answer fills in the rest.
    pub fn plan(&mut self, requested: &Requested<'_>) -> Plan {
        let mut plan = Plan {
            record_path: requested.record_path.map(str::to_string),
            report_path: requested.report_path.map(str::to_string),
            live: requested.live.unwrap_or(false),
            ..Plan::default()
        };
        match &mut self.state {
            State::NotAsked => {}
            State::Answered {
                record,
                report,
                live,
                open_browser,
            } => {
                if plan.record_path.is_none() {
                    plan.record_path = record.as_mut().map(Slot::take);
                }
                if plan.report_path.is_none() {
                    plan.report_path = report.as_mut().map(Slot::take);
                }
                plan.live = requested.live.unwrap_or(*live);
                plan.open_browser = plan.live && *open_browser;
            }
            State::Unanswered { hinted } => {
                plan.hint = !*hinted && requested.is_unset();
                *hinted |= plan.hint;
            }
        }
        plan
    }

    /// Whether the form is out with the client, waiting for it to call `tui_start` again.
    #[must_use]
    pub const fn form_out(&self) -> bool {
        self.form_out
    }

    /// The form was handed to the client as an input request.
    pub const fn send_form(&mut self) {
        self.form_out = true;
    }

    /// Whether to open the live page of `session_id` now (the first time only).
    pub fn first_open(&mut self, session_id: &str) -> bool {
        self.opened.insert(session_id.to_string())
    }
}

/// The key of the form in a `tui_start` input request (protocol 2026-07-28 and later).
pub const INPUT_KEY: &str = "shadowpty_capture";

/// The request state returned with the form. A client that calls `tui_start` again with it but
/// without an answer under [`INPUT_KEY`] closed the form.
pub const FORM_STATE: &str = "shadowpty_capture_form";

/// Whether the client can show forms, from the capabilities it sent with the request.
#[must_use]
pub fn supports_forms(capabilities: Option<&ClientCapabilities>) -> bool {
    capabilities
        .and_then(|c| c.elicitation.as_ref())
        // A client that names neither mode supports forms (MCP 2025-06-18)
        .is_some_and(|e| e.form.is_some() || e.url.is_none())
}

/// The form asking how to capture sessions, starting with `command`.
pub fn form(command: &str) -> Result<ElicitRequestParams, &'static str> {
    let defaults = Defaults::for_command(command);
    Ok(ElicitRequestParams::FormElicitationParams {
        meta: None,
        message: format!(
            "The assistant is starting `{command}` in a ShadowPTY terminal. Do you want to record \
             it, get a report of its checks, or watch it live in your browser? Your answer \
             applies to every session until the MCP server restarts."
        ),
        requested_schema: form_schema(&defaults)?,
    })
}

/// The form as an input request for `tui_start` to return; the client shows it and calls
/// `tui_start` again with the answer under [`INPUT_KEY`].
pub fn input_request(command: &str) -> Result<InputRequiredResult, &'static str> {
    let request = InputRequest::Elicitation(ElicitRequest::new(form(command)?));
    Ok(InputRequiredResult::new(
        Some([(INPUT_KEY.to_string(), request)].into()),
        Some(FORM_STATE.to_string()),
    ))
}

/// Reads the answer the client sent back for [`input_request`].
#[must_use]
pub fn read_response(response: &Value, command: &str) -> Answer {
    match serde_json::from_value::<ElicitResult>(response.clone()) {
        Ok(result) => to_answer(&result, command),
        Err(e) => {
            tracing::warn!("Unreadable answer to the capture form: {e}");
            Answer::Dismissed
        }
    }
}

/// Asks the person during the call (protocols before 2026-07-28, where the server can send
/// requests to the client while handling one).
pub async fn ask(peer: &Peer<RoleServer>, command: &str) -> Answer {
    let request = match form(command) {
        Ok(request) => request,
        Err(e) => {
            tracing::warn!("Can't build the capture form: {e}");
            return Answer::Unsupported;
        }
    };
    match peer
        .create_elicitation_with_timeout(request, Some(ANSWER_TIMEOUT))
        .await
    {
        Ok(result) => to_answer(&result, command),
        Err(e) => {
            tracing::warn!("Asking how to capture sessions failed: {e}");
            Answer::Dismissed
        }
    }
}

fn to_answer(result: &ElicitResult, command: &str) -> Answer {
    match result.action {
        ElicitationAction::Accept => Answer::Chose(parse_answer(
            result.content.as_ref(),
            &Defaults::for_command(command),
        )),
        ElicitationAction::Decline => Answer::Declined,
        _ => Answer::Dismissed,
    }
}

/// File paths offered in the form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defaults {
    pub record_path: String,
    pub report_path: String,
}

impl Defaults {
    /// Files named after the command, in the server's working directory, that don't exist yet.
    #[must_use]
    pub fn for_command(command: &str) -> Self {
        let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::in_dir(&dir, command)
    }

    #[must_use]
    pub fn in_dir(dir: &Path, command: &str) -> Self {
        let name = file_stem(command);
        let path = |file: String| {
            let path = dir.join(file).to_string_lossy().into_owned();
            if Path::new(&path).exists() {
                unused_sibling(&path)
            } else {
                path
            }
        };
        Self {
            record_path: path(format!("{name}.cast")),
            report_path: path(format!("{name}.report.jsonl")),
        }
    }
}

/// The command's file name, reduced to characters safe in a file name.
fn file_stem(command: &str) -> String {
    let base = Path::new(command)
        .file_name()
        .map_or_else(|| command.into(), |name| name.to_string_lossy());
    let stem: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if stem.trim_matches('_').is_empty() {
        "session".to_string()
    } else {
        stem
    }
}

/// `dir/name-N.ext` for the smallest N from 2 that doesn't exist yet. A compound extension
/// (`.report.jsonl`) stays whole.
fn unused_sibling(path: &str) -> String {
    let path = Path::new(path);
    let dir = path.parent().unwrap_or_else(|| Path::new(""));
    let file = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (name, ext) = file
        .find('.')
        .filter(|&dot| dot > 0)
        .map_or((file.as_str(), ""), |dot| file.split_at(dot));
    (2..=u32::MAX)
        .map(|n| dir.join(format!("{name}-{n}{ext}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or_else(|| dir.join(&file))
        .to_string_lossy()
        .into_owned()
}

fn form_schema(defaults: &Defaults) -> Result<ElicitationSchema, &'static str> {
    ElicitationSchema::builder()
        .optional_bool_with("record", |b| {
            b.title("Record the session")
                .description("Save an asciicast recording you can replay with `asciinema play`")
                .with_default(false)
        })
        .optional_string_with("record_path", |s| {
            s.title("Recording file")
                .with_default(defaults.record_path.clone())
        })
        .optional_bool_with("report", |b| {
            b.title("Write a report")
                .description("A JSON Lines file with every input, check and result")
                .with_default(false)
        })
        .optional_string_with("report_path", |s| {
            s.title("Report file")
                .with_default(defaults.report_path.clone())
        })
        .optional_bool_with("live", |b| {
            b.title("Watch live")
                .description("A private page on this machine showing the screen, every key and every check as the assistant works")
                .with_default(false)
        })
        .optional_bool_with("open_browser", |b| {
            b.title("Open the live page in my browser")
                .description("Otherwise the assistant gives you the link")
                .with_default(true)
        })
        .build()
}

/// Reads the form's answer; a missing field takes its default, an empty path the offered one.
#[must_use]
pub fn parse_answer(content: Option<&Value>, defaults: &Defaults) -> Choices {
    let flag = |name: &str, default: bool| {
        content
            .and_then(|c| c.get(name))
            .and_then(Value::as_bool)
            .unwrap_or(default)
    };
    let path = |name: &str, default: &str| {
        content
            .and_then(|c| c.get(name))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map_or_else(|| default.to_string(), expand_home)
    };
    Choices {
        record: flag("record", false).then(|| path("record_path", &defaults.record_path)),
        report: flag("report", false).then(|| path("report_path", &defaults.report_path)),
        live: flag("live", false),
        open_browser: flag("open_browser", true),
    }
}

/// `~/x` → `$HOME/x`, as the person would expect from a shell.
fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest).to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

/// Opens `url` in the person's default browser.
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    // stdout is the MCP channel: the opener must not inherit it
    let mut child = tokio::process::Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn defaults() -> Defaults {
        Defaults {
            record_path: "/tmp/x/htop.cast".into(),
            report_path: "/tmp/x/htop.report.jsonl".into(),
        }
    }

    #[test]
    fn test_parse_answer() {
        let all = json!({"record": true, "record_path": "  ", "report": true,
            "report_path": "/r.jsonl", "live": true, "open_browser": false});
        assert_eq!(
            parse_answer(Some(&all), &defaults()),
            Choices {
                record: Some("/tmp/x/htop.cast".into()),
                report: Some("/r.jsonl".into()),
                live: true,
                open_browser: false,
            }
        );
        let none = parse_answer(Some(&json!({})), &defaults());
        assert_eq!(
            none,
            Choices {
                open_browser: true,
                ..Choices::default()
            }
        );
        assert_eq!(parse_answer(None, &defaults()), none);
    }

    #[test]
    fn test_file_stem_and_defaults() {
        assert_eq!(file_stem("/usr/bin/htop"), "htop");
        assert_eq!(file_stem("my app.sh"), "my_app_sh");
        assert_eq!(file_stem("/"), "session");
        let d = Defaults::in_dir(Path::new("/nonexistent-dir"), "./vim");
        assert_eq!(d.record_path, "/nonexistent-dir/vim.cast");
        assert_eq!(d.report_path, "/nonexistent-dir/vim.report.jsonl");
    }

    #[test]
    fn test_unused_sibling_keeps_compound_extension() {
        assert_eq!(
            unused_sibling("/nonexistent-dir/a.cast"),
            "/nonexistent-dir/a-2.cast"
        );
        assert_eq!(
            unused_sibling("/nonexistent-dir/a.report.jsonl"),
            "/nonexistent-dir/a-2.report.jsonl"
        );
        assert_eq!(unused_sibling("/nonexistent-dir/a"), "/nonexistent-dir/a-2");
    }

    #[test]
    fn test_answer_applies_to_later_sessions_and_agent_wins() {
        let mut capture = Capture::default();
        let unset = Requested::default();
        assert!(capture.should_ask(&unset));
        assert!(!capture.should_ask(&Requested {
            live: Some(false),
            ..unset
        }));

        capture.answer(Answer::Chose(Choices {
            record: Some("/nonexistent-dir/htop.cast".into()),
            report: None,
            live: true,
            open_browser: true,
        }));
        assert!(!capture.should_ask(&unset));

        let first = capture.plan(&unset);
        assert_eq!(
            first.record_path.as_deref(),
            Some("/nonexistent-dir/htop.cast")
        );
        assert!(first.live && first.open_browser && !first.hint);
        assert_eq!(first.report_path, None);

        let second = capture.plan(&unset);
        assert_eq!(
            second.record_path.as_deref(),
            Some("/nonexistent-dir/htop-2.cast")
        );

        let explicit = capture.plan(&Requested {
            record_path: Some("/a.cast"),
            live: Some(false),
            ..unset
        });
        assert_eq!(explicit.record_path.as_deref(), Some("/a.cast"));
        assert!(!explicit.live && !explicit.open_browser);

        assert!(capture.first_open("default"));
        assert!(!capture.first_open("default"));
    }

    #[test]
    fn test_declined_captures_nothing_without_hint() {
        let mut capture = Capture::default();
        capture.answer(Answer::Declined);
        assert_eq!(capture.plan(&Requested::default()), Plan::default());
    }

    #[test]
    fn test_unanswered_hints_once() {
        let mut capture = Capture::default();
        capture.answer(Answer::Unsupported);
        let unset = Requested::default();
        assert!(!capture.should_ask(&unset));
        let explicit = capture.plan(&Requested {
            live: Some(true),
            ..unset
        });
        assert!(explicit.live && !explicit.hint);
        assert!(capture.plan(&unset).hint);
        assert!(!capture.plan(&unset).hint);
    }
}
