//! Asking the person how sessions are captured: recorded, reported, watched live.
//!
//! `tui_start` never waits for the person. The first session started without `record_path`,
//! `report_path` and `live` records and writes its report to the default files ([`Defaults`])
//! until the person decides, and the decision moves or deletes them when the session ends:
//!
//! - Before protocol 2026-07-28 the form goes to the client in the background as the session
//!   starts. If the person doesn't answer within the answer timeout, the defaults apply.
//! - On 2026-07-28 a server can only ask during a call, so `tui_end` returns the form.
//! - When the client can't show forms, or the person declines or closes the form, the defaults
//!   apply: keep the recording and the report in the default files.
//!
//! The answer (or the defaults) then applies to every later session of the server.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{
    ClientCapabilities, ElicitRequest, ElicitRequestParams, ElicitResult, ElicitationAction,
    ElicitationSchema, InputRequest, InputRequiredResult,
};
use rmcp::{Peer, RoleServer};
use serde_json::Value;
use tokio::sync::watch;

/// How long the person has to answer the form before the defaults apply.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// What the agent passed to `tui_start`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Requested<'a> {
    pub record_path: Option<&'a str>,
    pub report_path: Option<&'a str>,
    pub live: Option<bool>,
}

impl Requested<'_> {
    /// The agent chose nothing, so the person decides.
    const fn is_unset(&self) -> bool {
        self.record_path.is_none() && self.report_path.is_none() && self.live.is_none()
    }
}

/// How the server can reach the person, from what the client supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Called as a library (no client): capture only what was passed, never ask.
    Library,
    /// The client can't show forms: the defaults apply.
    NoForms,
    /// Send the form in the background as the session starts (protocols before 2026-07-28).
    Background,
    /// Return the form from `tui_end` (2026-07-28, where the server can only ask during a call).
    AtEnd,
}

/// What `tui_start` tells the agent about capture, besides where the files go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// The defaults apply and the person was never asked: the agent may offer the rest.
    Defaults,
    /// The person is being asked in the background.
    AskingNow,
    /// The person will be asked when the session ends.
    AskAtEnd,
}

/// How one session is captured, once the agent's parameters and the person's answer are merged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub record_path: Option<String>,
    pub report_path: Option<String>,
    pub live: bool,
    /// Open the live page in the person's browser (only when `live`).
    pub open_browser: bool,
    pub note: Option<Note>,
    /// Send the form in the background now (see [`Reach::Background`]).
    pub ask_now: bool,
}

/// The person's answer to the form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Choices {
    pub record: Option<String>,
    pub report: Option<String>,
    pub live: bool,
}

/// How asking went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Chose(Choices),
    /// The person said no to the form: the defaults apply, without reminders.
    Declined,
    /// The person closed the form or didn't answer in time: the defaults apply.
    Dismissed,
    /// The client can't show forms: the defaults apply.
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
    /// The form is out. `done` turns true once a background form is answered (before
    /// 2026-07-28); it's `None` for a form returned to the client from `tui_end`.
    Asking { done: Option<watch::Receiver<bool>> },
    Answered {
        record: Option<Slot>,
        report: Option<Slot>,
        live: bool,
    },
    /// Keep the recording and the report in the default files. `hinted` once the agent was told
    /// it may offer the person more.
    Defaulted { hinted: bool },
}

/// A session recording to the default files until the person decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub command: String,
    pub record: String,
    pub report: String,
}

impl Pending {
    /// The files this session writes, offered as the defaults in the form.
    #[must_use]
    pub fn defaults(&self) -> Defaults {
        Defaults {
            record_path: self.record.clone(),
            report_path: self.report.clone(),
        }
    }
}

/// What happens to one of a pending session's files once the person decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fate {
    Keep(String),
    Move { from: String, to: String },
    Delete(String),
}

impl Fate {
    fn of(file: String, destination: Option<String>) -> Self {
        match destination {
            Some(to) if to == file => Self::Keep(file),
            Some(to) => Self::Move { from: file, to },
            None => Self::Delete(file),
        }
    }

    /// Where the file is once its fate is carried out, if it's kept.
    #[must_use]
    pub fn kept_at(&self) -> Option<&str> {
        match self {
            Self::Keep(path) | Self::Move { to: path, .. } => Some(path),
            Self::Delete(_) => None,
        }
    }
}

/// What happens to a pending session's recording and report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub record: Fate,
    pub report: Fate,
}

/// The person's capture choices for the life of the server.
#[derive(Debug)]
pub struct Capture {
    state: State,
    /// Sessions recording to the default files until the person decides, by session id.
    pending: HashMap<String, Pending>,
    /// Sessions whose live page was opened in the browser. Restarting one switches the open
    /// page to the new session, so it isn't opened again.
    opened: HashSet<String>,
    /// Where the default files go (the working directory if unset).
    dir: Option<PathBuf>,
    timeout: Duration,
}

impl Default for Capture {
    fn default() -> Self {
        Self::new(None, ANSWER_TIMEOUT)
    }
}

impl Capture {
    /// Default files in `dir` (the working directory if `None`); the person has `timeout` to
    /// answer a background form.
    #[must_use]
    pub fn new(dir: Option<PathBuf>, timeout: Duration) -> Self {
        Self {
            state: State::NotAsked,
            pending: HashMap::new(),
            opened: HashSet::new(),
            dir,
            timeout,
        }
    }

    /// How long the person has to answer a background form.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Default files for a session of `command`.
    #[must_use]
    pub fn defaults(&self, command: &str) -> Defaults {
        self.dir.as_deref().map_or_else(
            || Defaults::for_command(command),
            |dir| Defaults::in_dir(dir, command),
        )
    }

    /// How session `session_id` of `command` starting now is captured. What the agent passed
    /// wins; the person's answer, or the defaults, fill in the rest.
    pub fn plan(&mut self, session: (&str, &str), requested: &Requested<'_>, reach: Reach) -> Plan {
        let (session_id, command) = session;
        // A new session under this id; whatever ran before keeps its files where they are
        self.pending.remove(session_id);
        let mut plan = Plan {
            record_path: requested.record_path.map(str::to_string),
            report_path: requested.report_path.map(str::to_string),
            live: requested.live.unwrap_or(false),
            ..Plan::default()
        };
        if reach == Reach::Library {
            return plan;
        }
        if reach == Reach::NoForms && matches!(self.state, State::NotAsked) {
            self.answer(Answer::Unsupported);
        }
        let defaults = self.defaults(command);
        match &mut self.state {
            State::Answered {
                record,
                report,
                live,
            } => {
                if plan.record_path.is_none() {
                    plan.record_path = record.as_mut().map(Slot::take);
                }
                if plan.report_path.is_none() {
                    plan.report_path = report.as_mut().map(Slot::take);
                }
                plan.live = requested.live.unwrap_or(*live);
                plan.open_browser = plan.live && *live;
            }
            State::Defaulted { hinted } => {
                plan.record_path.get_or_insert(defaults.record_path);
                plan.report_path.get_or_insert(defaults.report_path);
                if requested.is_unset() && !*hinted {
                    plan.note = Some(Note::Defaults);
                    *hinted = true;
                }
            }
            State::NotAsked | State::Asking { .. } if requested.is_unset() => {
                let first = matches!(self.state, State::NotAsked);
                plan.ask_now = first && reach == Reach::Background;
                plan.note = Some(if reach == Reach::AtEnd {
                    Note::AskAtEnd
                } else {
                    Note::AskingNow
                });
                self.pending.insert(
                    session_id.to_string(),
                    Pending {
                        command: command.to_string(),
                        record: defaults.record_path.clone(),
                        report: defaults.report_path.clone(),
                    },
                );
                plan.record_path = Some(defaults.record_path);
                plan.report_path = Some(defaults.report_path);
            }
            State::NotAsked | State::Asking { .. } => {}
        }
        plan
    }

    /// A background form went out; `done` turns true once it's answered.
    pub fn asking_in_background(&mut self, done: watch::Receiver<bool>) {
        self.state = State::Asking { done: Some(done) };
    }

    /// Session `session_id` failed to start: forget it. With `asked_now` (its plan's
    /// [`Plan::ask_now`]), also take back the background form that was about to go out for it,
    /// so the next start asks instead.
    pub fn abandon(&mut self, session_id: &str, asked_now: bool) {
        self.pending.remove(session_id);
        if asked_now && matches!(self.state, State::Asking { done: Some(_) }) {
            self.state = State::NotAsked;
        }
    }

    /// The form was returned to the client from `tui_end`.
    pub fn form_sent(&mut self) {
        self.state = State::Asking { done: None };
    }

    /// Whether a form returned from `tui_end` is still out with the client.
    #[must_use]
    pub const fn form_out(&self) -> bool {
        matches!(self.state, State::Asking { done: None })
    }

    /// The background form's completion, while it's out.
    #[must_use]
    pub fn waiting(&self) -> Option<watch::Receiver<bool>> {
        match &self.state {
            State::Asking { done } => done.clone(),
            _ => None,
        }
    }

    /// Whether the person's decision (or the defaults) is known.
    #[must_use]
    pub const fn decided(&self) -> bool {
        matches!(self.state, State::Answered { .. } | State::Defaulted { .. })
    }

    /// The pending session `session_id`, if it waits for the person's decision.
    #[must_use]
    pub fn pending(&self, session_id: &str) -> Option<&Pending> {
        self.pending.get(session_id)
    }

    /// Remembers how asking went.
    pub fn answer(&mut self, answer: Answer) {
        self.state = match answer {
            Answer::Chose(choices) => State::Answered {
                record: choices.record.map(Slot::new),
                report: choices.report.map(Slot::new),
                live: choices.live,
            },
            Answer::Declined => State::Defaulted { hinted: true },
            Answer::Dismissed | Answer::Unsupported => State::Defaulted { hinted: false },
        };
    }

    /// What happens to the files of pending session `session_id`, now that it ends. Before the
    /// person decided, they keep the defaults.
    pub fn settle(&mut self, session_id: &str) -> Option<Settled> {
        let pending = self.pending.remove(session_id)?;
        Some(match &mut self.state {
            State::Answered { record, report, .. } => Settled {
                record: Fate::of(pending.record, record.as_mut().map(Slot::take)),
                report: Fate::of(pending.report, report.as_mut().map(Slot::take)),
            },
            _ => Settled {
                record: Fate::Keep(pending.record),
                report: Fate::Keep(pending.report),
            },
        })
    }

    /// Whether to open the live page of `session_id` now (the first time only).
    pub fn first_open(&mut self, session_id: &str) -> bool {
        self.opened.insert(session_id.to_string())
    }
}

/// Carries out a settled session's file fates. Returns them with problems folded in: a file
/// that couldn't be moved stays where it was.
pub async fn carry_out(settled: Settled) -> (Settled, Vec<String>) {
    let mut problems = Vec::new();
    let record = carry_out_one(settled.record, &mut problems).await;
    let report = carry_out_one(settled.report, &mut problems).await;
    (Settled { record, report }, problems)
}

async fn carry_out_one(fate: Fate, problems: &mut Vec<String>) -> Fate {
    match fate {
        Fate::Keep(_) => fate,
        Fate::Delete(ref path) => {
            if let Err(e) = tokio::fs::remove_file(path).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                problems.push(format!("couldn't delete '{path}': {e}"));
            }
            fate
        }
        Fate::Move { from, to } => match move_file(&from, &to).await {
            Ok(()) => Fate::Move { from, to },
            Err(e) => {
                problems.push(format!("couldn't move '{from}' to '{to}': {e}"));
                Fate::Keep(from)
            }
        },
    }
}

/// Renames `from` to `to`, copying when they're on different filesystems.
async fn move_file(from: &str, to: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(to).parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::fs::rename(from, to).await.is_ok() {
        return Ok(());
    }
    tokio::fs::copy(from, to).await?;
    tokio::fs::remove_file(from).await
}

/// The key of the form in an input request (protocol 2026-07-28 and later).
pub const INPUT_KEY: &str = "shadowpty_capture";

/// The request state returned with the form. A client that calls `tui_end` again with it but
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

/// When the form is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// As the session starts: it can also offer watching it live.
    Start,
    /// As the session ends: only what to keep.
    End,
}

/// The form asking how to capture sessions of `command`, offering `defaults` as the files.
pub fn form(
    command: &str,
    defaults: &Defaults,
    moment: Moment,
) -> Result<ElicitRequestParams, &'static str> {
    let message = match moment {
        Moment::Start => format!(
            "The assistant started `{command}` in a ShadowPTY terminal. It's being recorded, with \
             a report of every check, to the files below. Keep them, choose other files, or \
             watch the session live in your browser? Without an answer in {} seconds, both are \
             kept. Your answer applies to every session until the MCP server restarts.",
            ANSWER_TIMEOUT.as_secs()
        ),
        Moment::End => format!(
            "The assistant is ending `{command}` in a ShadowPTY terminal. It was recorded, with \
             a report of every check, to the files below. Keep them, or choose other files? Your \
             answer applies to every session until the MCP server restarts."
        ),
    };
    Ok(ElicitRequestParams::FormElicitationParams {
        meta: None,
        message,
        requested_schema: form_schema(defaults, moment)?,
    })
}

/// The form as an input request for `tui_end` to return; the client shows it and calls
/// `tui_end` again with the answer under [`INPUT_KEY`].
pub fn input_request(
    command: &str,
    defaults: &Defaults,
) -> Result<InputRequiredResult, &'static str> {
    let request =
        InputRequest::Elicitation(ElicitRequest::new(form(command, defaults, Moment::End)?));
    Ok(InputRequiredResult::new(
        Some([(INPUT_KEY.to_string(), request)].into()),
        Some(FORM_STATE.to_string()),
    ))
}

/// Reads the answer the client sent back for [`input_request`].
#[must_use]
pub fn read_response(response: &Value, defaults: &Defaults) -> Answer {
    match serde_json::from_value::<ElicitResult>(response.clone()) {
        Ok(result) => to_answer(&result, defaults),
        Err(e) => {
            tracing::warn!("Unreadable answer to the capture form: {e}");
            Answer::Dismissed
        }
    }
}

/// Asks the person during a call or in the background (protocols before 2026-07-28, where the
/// server can send requests to the client). No answer within `timeout` counts as dismissed.
pub async fn ask(
    peer: &Peer<RoleServer>,
    (command, defaults): (&str, &Defaults),
    moment: Moment,
    timeout: Duration,
) -> Answer {
    let request = match form(command, defaults, moment) {
        Ok(request) => request,
        Err(e) => {
            tracing::warn!("Can't build the capture form: {e}");
            return Answer::Unsupported;
        }
    };
    match peer
        .create_elicitation_with_timeout(request, Some(timeout))
        .await
    {
        Ok(result) => to_answer(&result, defaults),
        Err(e) => {
            tracing::warn!("Asking how to capture sessions failed: {e}");
            Answer::Dismissed
        }
    }
}

fn to_answer(result: &ElicitResult, defaults: &Defaults) -> Answer {
    match result.action {
        ElicitationAction::Accept => Answer::Chose(parse_answer(result.content.as_ref(), defaults)),
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

fn form_schema(defaults: &Defaults, moment: Moment) -> Result<ElicitationSchema, &'static str> {
    let builder = ElicitationSchema::builder()
        .optional_bool_with("record", |b| {
            b.title("Keep the recording")
                .description("An asciicast recording you can replay with `asciinema play`")
                .with_default(true)
        })
        .optional_string_with("record_path", |s| {
            s.title("Recording file")
                .with_default(defaults.record_path.clone())
        })
        .optional_bool_with("report", |b| {
            b.title("Keep the report")
                .description("A JSON Lines file with every input, check and result")
                .with_default(true)
        })
        .optional_string_with("report_path", |s| {
            s.title("Report file")
                .with_default(defaults.report_path.clone())
        });
    match moment {
        Moment::Start => builder.optional_bool_with("live", |b| {
            b.title("Watch live")
                .description("Opens a private page on this machine in your browser, showing the screen, every key and every check as the assistant works")
                .with_default(false)
        }),
        Moment::End => builder,
    }
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
        record: flag("record", true).then(|| path("record_path", &defaults.record_path)),
        report: flag("report", true).then(|| path("report_path", &defaults.report_path)),
        live: flag("live", false),
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

    fn capture() -> Capture {
        Capture::new(Some(PathBuf::from("/nonexistent-dir")), ANSWER_TIMEOUT)
    }

    #[test]
    fn test_parse_answer() {
        let all = json!({"record": true, "record_path": "  ", "report": true,
            "report_path": "/r.jsonl", "live": true});
        assert_eq!(
            parse_answer(Some(&all), &defaults()),
            Choices {
                record: Some("/tmp/x/htop.cast".into()),
                report: Some("/r.jsonl".into()),
                live: true,
            }
        );
        // Missing fields keep both files where they are
        let kept = Choices {
            record: Some("/tmp/x/htop.cast".into()),
            report: Some("/tmp/x/htop.report.jsonl".into()),
            live: false,
        };
        assert_eq!(parse_answer(Some(&json!({})), &defaults()), kept);
        assert_eq!(parse_answer(None, &defaults()), kept);
        let none = json!({"record": false, "report": false});
        assert_eq!(parse_answer(Some(&none), &defaults()), Choices::default());
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
    fn test_undecided_session_records_to_defaults_and_settles_with_the_answer() {
        let mut capture = capture();
        let unset = Requested::default();
        let first = capture.plan(("one", "htop"), &unset, Reach::AtEnd);
        assert_eq!(
            first.record_path.as_deref(),
            Some("/nonexistent-dir/htop.cast")
        );
        assert_eq!(first.note, Some(Note::AskAtEnd));
        assert!(!first.ask_now);
        assert!(capture.pending("one").is_some());

        // An explicit start isn't pending and gets nothing it didn't ask for
        let explicit = capture.plan(
            ("explicit", "htop"),
            &Requested {
                live: Some(false),
                ..unset
            },
            Reach::AtEnd,
        );
        assert_eq!(explicit, Plan::default());
        assert!(capture.pending("explicit").is_none());

        capture.answer(Answer::Chose(Choices {
            record: Some("/nonexistent-dir/kept.cast".into()),
            report: None,
            live: false,
        }));
        assert_eq!(
            capture.settle("one"),
            Some(Settled {
                record: Fate::Move {
                    from: "/nonexistent-dir/htop.cast".into(),
                    to: "/nonexistent-dir/kept.cast".into(),
                },
                report: Fate::Delete("/nonexistent-dir/htop.report.jsonl".into()),
            })
        );
        assert_eq!(capture.settle("one"), None, "settled once");

        // Later sessions take the answer from the start
        let later = capture.plan(("two", "htop"), &unset, Reach::AtEnd);
        assert_eq!(
            later.record_path.as_deref(),
            Some("/nonexistent-dir/kept-2.cast")
        );
        assert_eq!(later.report_path, None);
        assert!(capture.pending("two").is_none());
    }

    #[test]
    fn test_background_asks_once() {
        let mut capture = capture();
        let unset = Requested::default();
        let first = capture.plan(("one", "htop"), &unset, Reach::Background);
        assert!(first.ask_now);
        assert_eq!(first.note, Some(Note::AskingNow));
        let (_done, waiting) = watch::channel(false);
        capture.asking_in_background(waiting);
        let second = capture.plan(("two", "htop"), &unset, Reach::Background);
        assert!(!second.ask_now, "the form is already out");
        assert!(capture.pending("two").is_some());
        assert!(capture.waiting().is_some());

        // A second session failing to start leaves the first one's form out
        capture.abandon("two", second.ask_now);
        assert!(capture.waiting().is_some());
        assert!(capture.pending("two").is_none());
        // The one that sent it failing takes it back
        capture.abandon("one", first.ask_now);
        assert!(capture.waiting().is_none());
        assert!(
            capture
                .plan(("three", "htop"), &unset, Reach::Background)
                .ask_now
        );
    }

    #[test]
    fn test_defaults_keep_files_and_hint_once() {
        let mut capture = capture();
        let unset = Requested::default();
        let first = capture.plan(("one", "htop"), &unset, Reach::NoForms);
        assert_eq!(
            first.record_path.as_deref(),
            Some("/nonexistent-dir/htop.cast")
        );
        assert_eq!(
            first.report_path.as_deref(),
            Some("/nonexistent-dir/htop.report.jsonl")
        );
        assert_eq!(first.note, Some(Note::Defaults));
        assert!(capture.pending("one").is_none(), "nothing to decide later");
        let second = capture.plan(("two", "htop"), &unset, Reach::NoForms);
        assert_eq!(second.note, None);

        // An undecided session that ends before the person answers keeps the defaults
        let mut capture = self::capture();
        capture.plan(("one", "htop"), &unset, Reach::AtEnd);
        assert_eq!(
            capture.settle("one"),
            Some(Settled {
                record: Fate::Keep("/nonexistent-dir/htop.cast".into()),
                report: Fate::Keep("/nonexistent-dir/htop.report.jsonl".into()),
            })
        );
    }

    #[test]
    fn test_declined_keeps_defaults_without_hint() {
        let mut capture = capture();
        capture.answer(Answer::Declined);
        let plan = capture.plan(("one", "htop"), &Requested::default(), Reach::AtEnd);
        assert_eq!(
            plan.record_path.as_deref(),
            Some("/nonexistent-dir/htop.cast")
        );
        assert_eq!(plan.note, None);
    }

    #[test]
    fn test_library_captures_only_what_was_passed() {
        let mut capture = capture();
        let plan = capture.plan(("one", "htop"), &Requested::default(), Reach::Library);
        assert_eq!(plan, Plan::default());
        assert!(capture.pending("one").is_none());
        assert!(!capture.decided());
    }

    #[test]
    fn test_restart_forgets_the_pending_session() {
        let mut capture = capture();
        let unset = Requested::default();
        capture.plan(("one", "htop"), &unset, Reach::AtEnd);
        capture.plan(
            ("one", "htop"),
            &Requested {
                live: Some(false),
                ..unset
            },
            Reach::AtEnd,
        );
        assert!(capture.pending("one").is_none());
    }

    #[tokio::test]
    async fn test_carry_out_moves_and_deletes() {
        let dir = std::env::temp_dir().join(format!("shadowpty_carry_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_default();
        let path = |name: &str| dir.join(name).to_string_lossy().into_owned();
        std::fs::write(path("a.cast"), "a").unwrap_or_default();
        std::fs::write(path("a.report.jsonl"), "r").unwrap_or_default();
        let (done, problems) = carry_out(Settled {
            record: Fate::Move {
                from: path("a.cast"),
                to: path("sub/b.cast"),
            },
            report: Fate::Delete(path("a.report.jsonl")),
        })
        .await;
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(done.record.kept_at(), Some(path("sub/b.cast").as_str()));
        assert!(Path::new(&path("sub/b.cast")).exists());
        assert!(!Path::new(&path("a.cast")).exists());
        assert!(!Path::new(&path("a.report.jsonl")).exists());

        // A file that can't be moved stays where it was
        let (kept, problems) = carry_out(Settled {
            record: Fate::Move {
                from: path("missing.cast"),
                to: path("c.cast"),
            },
            report: Fate::Keep(path("x")),
        })
        .await;
        assert_eq!(problems.len(), 1);
        assert_eq!(kept.record, Fate::Keep(path("missing.cast")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
