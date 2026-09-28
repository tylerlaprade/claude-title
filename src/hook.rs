use crate::probe;
use crate::state::{self, Dialog, State, StateKind};
use anyhow::{Context, Result};
use fs2::FileExt;
use serde::Deserialize;
use serde_json::Value;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Default, Deserialize)]
struct HookInput {
    agent_id: Option<String>,
    hook_event_name: Option<String>,
    session_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: Option<PathBuf>,
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Value,
    #[serde(default)]
    background_tasks: Vec<BackgroundTask>,
}

#[derive(Deserialize)]
struct BackgroundTask {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: String,
}

pub fn run() -> Result<()> {
    if env::var("CLAUDE_CODE_ENTRYPOINT").as_deref() != Ok("cli") {
        return Ok(());
    }

    let input: HookInput = serde_json::from_reader(io::stdin()).unwrap_or_default();
    let event = input.hook_event_name.as_deref().unwrap_or("");
    let agent = input.agent_id.as_deref().unwrap_or("");
    // Subagents share the main thread's dialogs but never its activity.
    let event_activity = if agent.is_empty() {
        activity_for_event(event)
    } else {
        None
    };
    if event_activity.is_none() && !concerns_dialogs(event) {
        return Ok(());
    }
    let claude_pid = claude_pid()?;
    let session_id = input.session_id.as_deref().unwrap_or("");
    let (event_activity, pending_beyond_shells, pending_shells) =
        if event_activity == Some(StateKind::Idle) {
            let (beyond_shells, shells) =
                classify_background_tasks(session_id, &input.background_tasks);
            if beyond_shells || !shells.is_empty() {
                (Some(StateKind::Pending), beyond_shells, shells)
            } else {
                (Some(StateKind::Idle), false, Vec::new())
            }
        } else {
            (event_activity, false, Vec::new())
        };
    let Some(tty) = tty_for_pid(claude_pid)? else {
        return Ok(());
    };
    if !fs::metadata(&tty).is_ok_and(|metadata| metadata.file_type().is_char_device()) {
        return Ok(());
    }

    let paths = state::paths_for_tty(&tty)?;
    let state_lock = state::lock(&paths.state)?;
    let previous = state::read(&paths.state)?
        .filter(|stored| stored.value.claude_pid == claude_pid && event != "SessionStart")
        .map(|stored| stored.value);
    if event == "Notification" && is_stale_dialog(previous.as_ref(), state::epoch()) {
        return Ok(());
    }
    let previous_dialogs = previous.as_ref().map_or(&[][..], |value| &value.dialogs);
    let dialogs = open_dialogs(
        event,
        agent,
        input.tool_name.as_deref().unwrap_or(""),
        &input.tool_input,
        previous_dialogs,
    );
    if event_activity.is_none() && event != "Notification" && dialogs == previous_dialogs {
        return Ok(());
    }
    let resolved_dialog = !previous_dialogs.is_empty() && dialogs.is_empty();
    let was_waiting = previous
        .as_ref()
        .is_some_and(|value| value.kind == StateKind::Waiting);
    // A dialog that opened without a permission request behind it, such as an
    // approval Claude Code asks for outside any tool, reports no close; the
    // main thread moving on is the only sign it was answered.
    let waiting = event == "Notification"
        || (was_waiting
            && if previous_dialogs.is_empty() {
                event_activity.is_none()
            } else {
                !dialogs.is_empty()
            });
    let (activity, pending_shells, pending_beyond_shells) = match (event_activity, &previous) {
        (Some(activity), _) => (activity, pending_shells, pending_beyond_shells),
        (None, Some(previous)) => (
            previous.activity,
            previous.pending_shells.clone(),
            previous.pending_beyond_shells,
        ),
        (None, None) => (StateKind::Unknown, Vec::new(), false),
    };
    let kind = if waiting {
        StateKind::Waiting
    } else {
        activity
    };
    let project = match (event_activity, &previous) {
        (None, Some(previous)) => previous.project.clone(),
        _ => project_name(input.cwd.as_deref()),
    };
    let is_prompt = event == "UserPromptSubmit";
    let input_transcript = input.transcript_path.filter(|path| path.is_file());
    let mut title_scan = previous
        .as_ref()
        .map(|value| value.title_scan.clone())
        .unwrap_or_default();
    let (transcript_path, transcript_offset) = if is_prompt {
        match input_transcript.as_deref() {
            Some(path) => {
                let offset = fs::metadata(path).map_or(0, |metadata| metadata.len());
                (Some(path.to_path_buf()), offset)
            }
            None => (None, 0),
        }
    } else {
        previous.map_or((None, 0), |value| {
            (value.transcript_path, value.transcript_offset)
        })
    };
    let custom_title = transcript_path
        .as_deref()
        .or(input_transcript.as_deref())
        .and_then(|path| title_scan.refresh(path));
    let value = State {
        kind,
        activity,
        epoch: state::epoch(),
        claude_pid,
        project,
        custom_title,
        title_scan,
        transcript_path,
        transcript_offset,
        dialogs,
        resolved_dialog,
        pending_session: session_id.to_string(),
        pending_shells,
        pending_beyond_shells,
    };
    state::write(&paths.state, &value)?;
    drop(state_lock);

    if kind != StateKind::End && !daemon_running(&paths.lock)? {
        spawn_daemon(&tty, &paths.state, &paths.lock, claude_pid)?;
    }
    Ok(())
}

// Stop reports every in-flight background task, but only work whose completion
// wakes the session should keep the title off Ready. The excluded kinds never
// do that: "dream" and "auto-mode scan" are internal idle-time chores, a
// teammate's registry entry stays "running" even while it idles, and a
// "cloud session" can park on browser-side user input for the rest of the
// session. Unrecognized kinds also fall through to Ready. Shells are probed
// by task id: one that runs until killed — a dev server holding a listening
// socket, a bare log tail — never wakes the session, while the rest are
// awaited work the daemon keeps re-probing during the pause. "monitor"
// counts on the assumption every monitor fires or times out; an upstream
// flag (dormant in 2.1.221) would auto-arm never-ending ambient monitors on
// artifact publishes, and if a session pins on Waiting after publishing an
// artifact, that flag has shipped and "monitor" needs a second look.
fn classify_background_tasks(session_id: &str, tasks: &[BackgroundTask]) -> (bool, Vec<String>) {
    let mut beyond_shells = false;
    let mut shells = Vec::new();
    for task in tasks {
        match task.kind.as_str() {
            // A shell with no id can never be probed, so it must not hold a
            // title nothing could clear.
            "shell" if !task.id.is_empty() => shells.push(task.id.clone()),
            "subagent" | "workflow" | "monitor" | "MCP task" => beyond_shells = true,
            _ => {}
        }
    }
    if !shells.is_empty() {
        let verdicts = probe::tasks(session_id, &shells);
        shells = shells
            .into_iter()
            .zip(verdicts)
            .filter(|(_, verdict)| {
                !matches!(
                    verdict,
                    probe::ShellProbe::Endless | probe::ShellProbe::Gone
                )
            })
            .map(|(id, _)| id)
            .collect();
    }
    (beyond_shells, shells)
}

// Claude Code arms a dialog's notification on a timer. Answer the dialog just
// as the timer fires and the notification reaches this hook after the dialog
// has already closed, which would pin Action required on a session that has
// moved on. Only the beat right after that close is refused, so a dialog
// opening at any other moment, with or without a tool behind it, still raises
// the title.
const STALE_DIALOG_SECONDS: u64 = 2;

fn is_stale_dialog(previous: Option<&State>, now: u64) -> bool {
    previous.is_some_and(|previous| {
        previous.resolved_dialog && now.saturating_sub(previous.epoch) <= STALE_DIALOG_SECONDS
    })
}

fn concerns_dialogs(event: &str) -> bool {
    matches!(
        event,
        "Notification"
            | "PermissionRequest"
            | "PostToolUse"
            | "PostToolUseFailure"
            | "PostToolBatch"
            | "SubagentStop"
    )
}

// As of Claude Code 2.1.284 the notification names neither the tool nor the
// agent behind a dialog, but the permission request that opens it names both
// the tool and, for a subagent, its agent_id. A dialog closes when its tool
// completes, whose input then carries every requested field (an answered
// question adds its answers). A denied or amended tool never completes with
// that input, but its batch still ends, and no batch of that agent can end
// while one of its dialogs is open. A turn boundary or a subagent's stop
// settles whatever that agent left open.
fn open_dialogs(
    event: &str,
    agent: &str,
    tool: &str,
    input: &Value,
    previous: &[Dialog],
) -> Vec<Dialog> {
    let mut open = previous.to_vec();
    match event {
        "PermissionRequest" => open.push(Dialog {
            agent: agent.to_string(),
            tool: tool.to_string(),
            input: input.clone(),
        }),
        "PostToolUse" | "PostToolUseFailure" => {
            if let Some(answered) = open.iter().position(|dialog| {
                dialog.agent == agent && dialog.tool == tool && carries(input, &dialog.input)
            }) {
                open.remove(answered);
            }
        }
        "PostToolBatch" | "SubagentStop" | "UserPromptSubmit" | "Stop" | "StopFailure" => {
            open.retain(|dialog| dialog.agent != agent);
        }
        _ => {}
    }
    open
}

fn carries(completed: &Value, requested: &Value) -> bool {
    match (completed, requested) {
        (Value::Object(completed), Value::Object(requested)) => requested
            .iter()
            .all(|(field, value)| completed.get(field) == Some(value)),
        _ => completed == requested,
    }
}

#[cfg(test)]
fn latest_session_title(path: &Path) -> Option<String> {
    crate::session_title::SessionTitle::default().refresh(path)
}

fn activity_for_event(event: &str) -> Option<StateKind> {
    match event {
        "SessionStart" | "Stop" | "StopFailure" => Some(StateKind::Idle),
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
            Some(StateKind::Busy)
        }
        "SessionEnd" => Some(StateKind::End),
        _ => None,
    }
}

fn claude_pid() -> Result<u32> {
    match env::var("CLAUDE_TITLE_PID") {
        Ok(value) => value
            .parse()
            .with_context(|| format!("invalid CLAUDE_TITLE_PID '{value}'")),
        Err(_) => Ok(unsafe { libc::getppid() as u32 }),
    }
}

fn tty_for_pid(pid: u32) -> Result<Option<PathBuf>> {
    if let Some(value) = env::var_os("CLAUDE_TITLE_TTY") {
        let path = PathBuf::from(value);
        return Ok(Some(if path.starts_with("/dev") {
            path
        } else {
            Path::new("/dev").join(path)
        }));
    }

    let output = crate::subprocess::output(Command::new("/bin/ps").args([
        "-o",
        "tty=",
        "-p",
        &pid.to_string(),
    ]))
    .context("failed to inspect Claude's terminal")?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() || value == "??" {
        return Ok(None);
    }
    Ok(Some(if value.starts_with("/dev/") {
        PathBuf::from(value)
    } else {
        Path::new("/dev").join(value)
    }))
}

fn project_name(cwd: Option<&Path>) -> String {
    let directory = env::var_os("CLAUDE_PROJECT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| cwd.map(Path::to_path_buf));
    directory
        .as_deref()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().replace(['\n', '\r'], " "))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Claude".to_string())
}

fn daemon_running(path: &Path) -> Result<bool> {
    let file = open_lock(path)?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            FileExt::unlock(&file)?;
            Ok(false)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error).with_context(|| format!("failed to lock {}", path.display())),
    }
}

fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))
}

fn spawn_daemon(tty: &Path, state: &Path, lock: &Path, pid: u32) -> Result<()> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(state.with_extension("log"))?;
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("daemon")
        .arg("--tty")
        .arg(tty)
        .arg("--state")
        .arg(state)
        .arg("--lock")
        .arg(lock)
        .arg("--pid")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().context("failed to start title daemon")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(command: &str) -> Value {
        serde_json::json!({ "command": command, "description": "Run it" })
    }

    fn requested(agent: &str, tool: &str, input: &Value, previous: &[Dialog]) -> Vec<Dialog> {
        open_dialogs("PermissionRequest", agent, tool, input, previous)
    }

    #[test]
    fn a_dialog_closes_when_its_tool_completes() {
        let open = requested("", "Bash", &bash("make"), &[]);
        assert_eq!(open.len(), 1);
        assert!(open_dialogs("PostToolUse", "", "Bash", &bash("make"), &open).is_empty());
    }

    #[test]
    fn a_sibling_completing_leaves_the_dialog_open() {
        let open = requested("", "Bash", &bash("make"), &[]);
        assert_eq!(
            open_dialogs("PostToolUse", "", "Bash", &bash("ls"), &open),
            open
        );
        assert_eq!(
            open_dialogs("PostToolUse", "", "Read", &bash("make"), &open),
            open
        );
    }

    #[test]
    fn an_answered_question_closes_its_dialog() {
        let question = serde_json::json!({ "questions": [{ "question": "Deploy?" }] });
        let open = requested("", "AskUserQuestion", &question, &[]);
        let answered = serde_json::json!({
            "questions": [{ "question": "Deploy?" }],
            "answers": { "Deploy?": "Yes" },
        });
        assert!(open_dialogs("PostToolUse", "", "AskUserQuestion", &answered, &open).is_empty());
    }

    #[test]
    fn a_subagents_dialog_closes_only_with_that_subagents_tool() {
        let open = requested("worker", "Bash", &bash("make"), &[]);
        assert_eq!(
            open_dialogs("PostToolUse", "", "Bash", &bash("make"), &open),
            open
        );
        assert!(open_dialogs("PostToolUse", "worker", "Bash", &bash("make"), &open).is_empty());
    }

    #[test]
    fn a_denied_tool_closes_its_dialog_when_its_batch_ends() {
        let open = requested("worker", "Bash", &bash("make"), &[]);
        let open = requested("", "Bash", &bash("ls"), &open);
        let settled = open_dialogs("PostToolBatch", "worker", "", &Value::Null, &open);
        assert_eq!(settled, requested("", "Bash", &bash("ls"), &[]));
    }

    #[test]
    fn a_stopped_subagent_leaves_no_dialog_behind() {
        let open = requested("worker", "Bash", &bash("make"), &[]);
        assert!(open_dialogs("SubagentStop", "worker", "", &Value::Null, &open).is_empty());
    }

    #[test]
    fn a_turn_boundary_settles_only_the_main_threads_dialogs() {
        let open = requested("", "Bash", &bash("make"), &[]);
        let open = requested("worker", "Bash", &bash("make"), &open);
        for boundary in ["UserPromptSubmit", "Stop"] {
            assert_eq!(
                open_dialogs(boundary, "", "", &Value::Null, &open),
                requested("worker", "Bash", &bash("make"), &[])
            );
        }
    }

    fn state(epoch: u64, resolved_dialog: bool) -> State {
        State {
            kind: StateKind::Busy,
            activity: StateKind::Busy,
            epoch,
            claude_pid: 1,
            project: "example".to_string(),
            custom_title: None,
            title_scan: crate::session_title::SessionTitle::default(),
            transcript_path: None,
            transcript_offset: 0,
            dialogs: Vec::new(),
            resolved_dialog,
            pending_session: String::new(),
            pending_shells: Vec::new(),
            pending_beyond_shells: false,
        }
    }

    #[test]
    fn a_dialog_announced_after_it_closed_is_stale() {
        assert!(is_stale_dialog(Some(&state(100, true)), 101));
    }

    #[test]
    fn a_dialog_that_opens_later_stands() {
        assert!(!is_stale_dialog(Some(&state(100, true)), 130));
    }

    #[test]
    fn a_dialog_that_opens_off_a_settled_session_stands() {
        assert!(!is_stale_dialog(Some(&state(100, false)), 101));
        assert!(!is_stale_dialog(None, 101));
    }

    #[test]
    fn latest_rename_is_what_the_title_reflects() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
                "\n",
                r#"{"type":"custom-title","customTitle":"First"}"#,
                "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#,
                "\n",
                r#"{"type":"custom-title","customTitle":"Smart-Title"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(latest_session_title(&path).as_deref(), Some("Smart-Title"));
    }

    #[test]
    fn a_cli_assigned_name_reaches_the_title() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"agent-setting","agentSetting":"tyler-1","sessionId":"s"}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
                "\n",
                r#"{"type":"agent-setting","agentSetting":"tyler-1","sessionId":"s"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(latest_session_title(&path).as_deref(), Some("tyler-1"));
    }

    #[test]
    fn a_rename_wins_over_a_cli_assigned_name_it_follows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"agent-setting","agentSetting":"tyler-1","sessionId":"s"}"#,
                "\n",
                r#"{"type":"custom-title","customTitle":"chosen","sessionId":"s"}"#,
                "\n",
                r#"{"type":"agent-setting","agentSetting":"tyler-1","sessionId":"s"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(latest_session_title(&path).as_deref(), Some("chosen"));
    }

    #[test]
    fn a_transcript_with_no_rename_leaves_the_title_alone() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        fs::write(
            &path,
            r#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#,
        )
        .unwrap();
        assert!(latest_session_title(&path).is_none());
    }

    #[test]
    fn a_partly_written_transcript_does_not_panic() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        fs::write(&path, r#"{"type":"custom-title","customTitle":"pending"#).unwrap();
        assert!(latest_session_title(&path).is_none());
    }
}
