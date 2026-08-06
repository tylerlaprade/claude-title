use crate::probe;
use crate::state::{self, State, StateKind};
use anyhow::{Context, Result};
use fs2::FileExt;
use serde::Deserialize;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Default, Deserialize)]
struct HookInput {
    hook_event_name: Option<String>,
    session_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: Option<PathBuf>,
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
    let Some(kind) = input
        .hook_event_name
        .as_deref()
        .and_then(state_kind_for_event)
    else {
        return Ok(());
    };
    let claude_pid = claude_pid()?;
    let session_id = input.session_id.as_deref().unwrap_or("");
    let (kind, pending_beyond_shells, pending_shells) = if kind == StateKind::Idle {
        let (beyond_shells, shells) =
            classify_background_tasks(session_id, &input.background_tasks);
        if beyond_shells || !shells.is_empty() {
            (StateKind::Pending, beyond_shells, shells)
        } else {
            (StateKind::Idle, false, Vec::new())
        }
    } else {
        (kind, false, Vec::new())
    };
    let Some(tty) = tty_for_pid(claude_pid)? else {
        return Ok(());
    };
    if !fs::metadata(&tty).is_ok_and(|metadata| metadata.file_type().is_char_device()) {
        return Ok(());
    }

    let paths = state::paths_for_tty(&tty)?;
    let previous = state::read(&paths.state)?
        .filter(|stored| stored.value.claude_pid == claude_pid)
        .map(|stored| stored.value);
    let is_prompt = input.hook_event_name.as_deref() == Some("UserPromptSubmit");
    let (transcript_path, transcript_offset) = if is_prompt {
        match input.transcript_path.filter(|path| path.is_file()) {
            Some(path) => {
                let offset = fs::metadata(&path).map_or(0, |metadata| metadata.len());
                (Some(path), offset)
            }
            None => (None, 0),
        }
    } else {
        previous.map_or((None, 0), |value| {
            (value.transcript_path, value.transcript_offset)
        })
    };
    let value = State {
        kind,
        epoch: state::epoch(),
        claude_pid,
        project: project_name(input.cwd.as_deref()),
        transcript_path,
        transcript_offset,
        pending_session: session_id.to_string(),
        pending_shells,
        pending_beyond_shells,
    };
    state::write(&paths.state, &value)?;

    if !daemon_running(&paths.lock)? {
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
// awaited work the daemon keeps re-probing during the pause.
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

fn state_kind_for_event(event: &str) -> Option<StateKind> {
    match event {
        "SessionStart" | "Stop" | "StopFailure" => Some(StateKind::Idle),
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
            Some(StateKind::Busy)
        }
        "Notification" => Some(StateKind::Waiting),
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

    let output = Command::new("/bin/ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
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
        .stderr(Stdio::null());
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
