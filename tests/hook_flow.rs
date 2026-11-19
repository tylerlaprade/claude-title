use claude_title::state;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Pty {
    master: File,
    // A title Claude Code wrote is lost on macOS if its terminal has no open
    // end left when the write returns.
    _slave: File,
    slave_path: PathBuf,
}

impl Pty {
    fn open() -> Self {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        let result = unsafe {
            libc::openpty(
                &raw mut master_fd,
                &raw mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(result, 0);
        let slave_path = tty_path(slave_fd);
        unsafe {
            let flags = libc::fcntl(master_fd, libc::F_GETFL);
            assert_ne!(flags, -1);
            assert_ne!(
                libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK),
                -1
            );
        }
        Self {
            master: unsafe { File::from_raw_fd(master_fd) },
            _slave: unsafe { File::from_raw_fd(slave_fd) },
            slave_path,
        }
    }

    fn read_for(&mut self, duration: Duration) -> Vec<u8> {
        let deadline = Instant::now() + duration;
        let mut output = Vec::new();
        while Instant::now() < deadline {
            write_nudged_title(&self.slave_path);
            let mut buffer = [0; 4096];
            match self.master.read(&mut buffer) {
                Ok(0) => thread::sleep(Duration::from_millis(10)),
                Ok(count) => output.extend_from_slice(&buffer[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("failed to read pseudo-terminal: {error}"),
            }
        }
        output
    }

    fn wait_for(&mut self, needle: &[u8]) -> Vec<u8> {
        self.wait_for_within(needle, Duration::from_secs(3))
    }

    fn wait_for_within(&mut self, needle: &[u8], patience: Duration) -> Vec<u8> {
        let deadline = Instant::now() + patience;
        let mut output = Vec::new();
        while Instant::now() < deadline {
            output.extend(self.read_for(Duration::from_millis(50)));
            if contains(&output, needle) {
                return output;
            }
        }
        panic!(
            "terminal output did not contain {:?}: {:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&output)
        );
    }
}

#[test]
fn hook_flow_updates_the_title_and_hands_off_cleanly() {
    let directory = tempfile::tempdir().unwrap();
    let transcript = directory.path().join("transcript.jsonl");
    append_records(&transcript, &[INTERRUPT_RECORD]);
    let mut pty = Pty::open();
    let first_claude = sleeper();

    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        r#"{"hook_event_name":"SessionStart","cwd":"/tmp/example"}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9c\xb3 Ready | example\x07");

    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/example","transcript_path":{}}}"#,
            serde_json::to_string(&transcript).unwrap()
        ),
    );
    pty.wait_for(b"\x1b]0;Working | example\x07");
    let old_marker_output = pty.read_for(Duration::from_millis(650));
    assert!(!contains(&old_marker_output, b"\xe2\x9c\xb3 Ready"));

    for tool in ["AskUserQuestion", "Read"] {
        run_hook(
            &pty.slave_path,
            first_claude.0.id(),
            &format!(
                r#"{{"hook_event_name":"PreToolUse","cwd":"/tmp/example","tool_name":"{tool}"}}"#
            ),
        );
    }
    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"PermissionRequest","cwd":"/tmp/example","tool_name":"AskUserQuestion","tool_input":{QUESTION}}}"#
        ),
    );
    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        r#"{"hook_event_name":"Notification","cwd":"/tmp/example"}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9a\xa0 Action required | example\x07");

    // A tool running alongside the open dialog finishes without clearing it.
    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        r#"{"hook_event_name":"PostToolUse","cwd":"/tmp/example","tool_name":"Read"}"#,
    );
    let sibling_output = pty.read_for(Duration::from_millis(650));
    assert!(!contains(&sibling_output, b"Working | example"));

    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"PostToolUse","cwd":"/tmp/example","tool_name":"AskUserQuestion","tool_input":{ANSWERED_QUESTION}}}"#
        ),
    );
    pty.wait_for(b"\x1b]0;Working | example\x07");

    // The dialog's notification can be a beat slower than the answer that
    // closed it, and must not put the title back on a session that moved on.
    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        r#"{"hook_event_name":"Notification","cwd":"/tmp/example"}"#,
    );
    let late_notification_output = pty.read_for(Duration::from_millis(650));
    assert!(!contains(
        &late_notification_output,
        b"\xe2\x9a\xa0 Action required"
    ));

    append_records(&transcript, &[INTERRUPT_RECORD]);
    pty.wait_for(b"\x1b]0;\xe2\x9c\xb3 Ready | example\x07");

    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/example","transcript_path":{}}}"#,
            serde_json::to_string(&transcript).unwrap()
        ),
    );
    pty.wait_for(b"\x1b]0;Working | example\x07");

    // Escape with a queued message flushes the abandoned turn after the prompt
    // hook has read the transcript length, so the interrupt lands past the
    // recorded offset and must not read as an interrupt of the new turn.
    append_records(&transcript, &[INTERRUPT_RECORD, PROMPT_RECORD]);
    let queued_prompt_output = pty.read_for(Duration::from_millis(650));
    assert!(!contains(&queued_prompt_output, b"\xe2\x9c\xb3 Ready"));

    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        r#"{"hook_event_name":"Notification","cwd":"/tmp/example","message":"Claude needs your permission to use Bash"}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9a\xa0 Action required | example\x07");

    append_records(&transcript, &[INTERRUPT_RECORD]);
    pty.wait_for(b"\x1b]0;\xe2\x9c\xb3 Ready | example\x07");

    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        r#"{"hook_event_name":"SessionEnd","cwd":"/tmp/example"}"#,
    );
    let first_clear = pty.wait_for(b"\x1b]0;\x07");
    assert_eq!(count(&first_clear, b"\x1b]0;\x07"), 1);
    drop(first_claude);

    let second_claude = sleeper();
    run_hook(
        &pty.slave_path,
        second_claude.0.id(),
        r#"{"hook_event_name":"SessionStart","cwd":"/tmp/second"}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9c\xb3 Ready | second\x07");

    run_hook(
        &pty.slave_path,
        second_claude.0.id(),
        r#"{"hook_event_name":"SessionEnd","cwd":"/tmp/second"}"#,
    );
    let second_clear = pty.wait_for(b"\x1b]0;\x07");
    drop(second_claude);
    wait_for_daemon_exit(&pty.slave_path);
    let trailing = pty.read_for(Duration::from_millis(300));
    assert_eq!(count(&[second_clear, trailing].concat(), b"\x1b]0;\x07"), 1);
}

const QUESTION: &str =
    r#"{"questions":[{"question":"Deploy?","options":[{"label":"Yes"},{"label":"No"}]}]}"#;
const ANSWERED_QUESTION: &str = r#"{"questions":[{"question":"Deploy?","options":[{"label":"Yes"},{"label":"No"}]}],"answers":{"Deploy?":"Yes"}}"#;
const WORKER_COMMAND: &str =
    r#"{"command":"rg -n lint ~/Code/other","description":"Search another repo"}"#;

#[test]
fn a_subagents_answered_dialog_releases_the_title() {
    let mut pty = Pty::open();
    let claude = sleeper();
    let background_subagent = r#"{"hook_event_name":"Stop","cwd":"/tmp/lead","background_tasks":[{"id":"worker","type":"subagent","status":"running","description":"survey","agent_type":"general-purpose"}]}"#;
    let worker_asks = format!(
        r#"{{"hook_event_name":"PermissionRequest","agent_id":"worker","cwd":"/tmp/lead","tool_name":"Bash","tool_input":{WORKER_COMMAND}}}"#
    );
    let dialog_raised = r#"{"hook_event_name":"Notification","cwd":"/tmp/lead","notification_type":"permission_prompt"}"#;

    run_hook(&pty.slave_path, claude.0.id(), background_subagent);
    pty.wait_for(b"\x1b]0;\xe2\xa7\x97 Waiting | lead\x07");

    run_hook(&pty.slave_path, claude.0.id(), &worker_asks);
    run_hook(&pty.slave_path, claude.0.id(), dialog_raised);
    pty.wait_for(b"\x1b]0;\xe2\x9a\xa0 Action required | lead\x07");

    // Approved, the subagent's tool completes and the lead goes back to
    // waiting on it.
    run_hook(
        &pty.slave_path,
        claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"PostToolUse","agent_id":"worker","cwd":"/tmp/lead","tool_name":"Bash","tool_input":{WORKER_COMMAND}}}"#
        ),
    );
    pty.wait_for(b"\x1b]0;\xe2\xa7\x97 Waiting | lead\x07");

    // The lead's own work does not answer a subagent's open dialog.
    run_hook(&pty.slave_path, claude.0.id(), &worker_asks);
    run_hook(&pty.slave_path, claude.0.id(), dialog_raised);
    pty.wait_for(b"\x1b]0;\xe2\x9a\xa0 Action required | lead\x07");
    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"PreToolUse","cwd":"/tmp/lead","tool_name":"Read"}"#,
    );
    let lead_output = pty.read_for(Duration::from_millis(650));
    assert!(!contains(&lead_output, b" Working | lead"));

    // Denied, the subagent's tool never completes, but its batch ends.
    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"PostToolBatch","agent_id":"worker","cwd":"/tmp/lead"}"#,
    );
    pty.wait_for(b"\x1b]0;Working | lead\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"SessionEnd","cwd":"/tmp/lead"}"#,
    );
    pty.wait_for(b"\x1b]0;\x07");
    drop(claude);
    wait_for_daemon_exit(&pty.slave_path);
}

#[test]
fn stop_with_pending_background_tasks_shows_waiting() {
    let directory = tempfile::tempdir().unwrap();
    let transcript = directory.path().join("transcript.jsonl");
    fs::write(&transcript, b"start\n").unwrap();
    let mut pty = Pty::open();
    let claude = sleeper();

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/pause","transcript_path":{}}}"#,
            serde_json::to_string(&transcript).unwrap()
        ),
    );
    pty.wait_for(b"\x1b]0;Working | pause\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","cwd":"/tmp/pause","background_tasks":[{"id":"b1","type":"shell","status":"running","description":"sleep","command":"sleep 5"}]}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\xa7\x97 Waiting | pause\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/pause","transcript_path":{}}}"#,
            serde_json::to_string(&transcript).unwrap()
        ),
    );
    pty.wait_for(b"\x1b]0;Working | pause\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","cwd":"/tmp/pause","background_tasks":[{"id":"d1","type":"dream","status":"running","description":"dreaming"},{"id":"a1","type":"auto-mode scan","status":"running","description":"scanning"},{"id":"t1","type":"teammate","status":"running","description":"resting"},{"id":"n1","type":"novel_chore","status":"running","description":"future work"},{"id":"m1","type":"monitor","status":"running","description":"live updates for artifact https://claude.ai/artifact/abc (auto-armed on publish)"},{"id":"m2","type":"monitor","status":"running","description":"presence on artifact https://claude.ai/artifact/abc"}]}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9c\xb3 Ready | pause\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/pause","transcript_path":{}}}"#,
            serde_json::to_string(&transcript).unwrap()
        ),
    );
    pty.wait_for(b"\x1b]0;Working | pause\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","cwd":"/tmp/pause","background_tasks":[{"id":"m3","type":"monitor","status":"running","description":"deploy socket"}]}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\xa7\x97 Waiting | pause\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/pause","transcript_path":{}}}"#,
            serde_json::to_string(&transcript).unwrap()
        ),
    );
    pty.wait_for(b"\x1b]0;Working | pause\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","cwd":"/tmp/pause","background_tasks":[{"id":"a2","type":"subagent","status":"running","description":"explore","agent_type":"Explore"}]}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\xa7\x97 Waiting | pause\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"SessionEnd","cwd":"/tmp/pause"}"#,
    );
    pty.wait_for(b"\x1b]0;\x07");
    drop(claude);
    wait_for_daemon_exit(&pty.slave_path);
}

#[test]
fn serving_and_killed_shells_release_the_waiting_title() {
    let directory = tempfile::tempdir().unwrap();
    let transcript = directory.path().join("transcript.jsonl");
    fs::write(&transcript, b"start\n").unwrap();
    let tasks_root = directory.path().join("tasks-root");
    let tasks = tasks_root.join("project").join("session").join("tasks");
    fs::create_dir_all(&tasks).unwrap();
    let mut pty = Pty::open();
    let claude = sleeper();

    // Real processes holding their task output files open, the way Claude
    // Code's background spawns do.
    let server = ChildGuard(
        Command::new("python3")
            .args([
                "-c",
                r#"import socket,time; s=socket.socket(); s.bind(("127.0.0.1",0)); s.listen(1); time.sleep(45)"#,
            ])
            .stdin(Stdio::null())
            .stdout(File::create(tasks.join("t1serve.output")).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut worker = ChildGuard(
        Command::new("sleep")
            .arg("45")
            .stdin(Stdio::null())
            .stdout(File::create(tasks.join("t2work.output")).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    run_hook_with_tasks_root(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"SessionStart","cwd":"/tmp/serve"}"#,
        &tasks_root,
    );
    let prompt = format!(
        r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/serve","transcript_path":{}}}"#,
        serde_json::to_string(&transcript).unwrap()
    );
    run_hook_with_tasks_root(&pty.slave_path, claude.0.id(), &prompt, &tasks_root);
    pty.wait_for(b"\x1b]0;Working | serve\x07");

    run_hook_with_tasks_root(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","session_id":"session","cwd":"/tmp/serve","background_tasks":[{"id":"t1serve","type":"shell","status":"running","description":"dev server","command":"npm run dev"}]}"#,
        &tasks_root,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9c\xb3 Ready | serve\x07");

    run_hook_with_tasks_root(&pty.slave_path, claude.0.id(), &prompt, &tasks_root);
    pty.wait_for(b"\x1b]0;Working | serve\x07");

    run_hook_with_tasks_root(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","session_id":"session","cwd":"/tmp/serve","background_tasks":[{"id":"t2work","type":"shell","status":"running","description":"long task","command":"sleep 45"}]}"#,
        &tasks_root,
    );
    pty.wait_for(b"\x1b]0;\xe2\xa7\x97 Waiting | serve\x07");

    // A task-list kill fires no wake; the re-probe must release the title.
    worker.0.kill().unwrap();
    worker.0.wait().unwrap();
    pty.wait_for_within(
        b"\x1b]0;\xe2\x9c\xb3 Ready | serve\x07",
        Duration::from_secs(12),
    );

    run_hook_with_tasks_root(&pty.slave_path, claude.0.id(), &prompt, &tasks_root);
    pty.wait_for(b"\x1b]0;Working | serve\x07");

    // A shell already dead at Stop never enters the waiting state at all.
    run_hook_with_tasks_root(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","session_id":"session","cwd":"/tmp/serve","background_tasks":[{"id":"t2work","type":"shell","status":"running","description":"long task","command":"sleep 45"}]}"#,
        &tasks_root,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9c\xb3 Ready | serve\x07");

    // A server that binds only after the turn ends: released by the re-probe.
    let late = ChildGuard(
        Command::new("python3")
            .args([
                "-c",
                r#"import socket,time; time.sleep(6); s=socket.socket(); s.bind(("127.0.0.1",0)); s.listen(1); time.sleep(45)"#,
            ])
            .stdin(Stdio::null())
            .stdout(File::create(tasks.join("t3late.output")).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    run_hook_with_tasks_root(&pty.slave_path, claude.0.id(), &prompt, &tasks_root);
    pty.wait_for(b"\x1b]0;Working | serve\x07");
    run_hook_with_tasks_root(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","session_id":"session","cwd":"/tmp/serve","background_tasks":[{"id":"t3late","type":"shell","status":"running","description":"slow server","command":"npm run dev"}]}"#,
        &tasks_root,
    );
    pty.wait_for(b"\x1b]0;\xe2\xa7\x97 Waiting | serve\x07");
    pty.wait_for_within(
        b"\x1b]0;\xe2\x9c\xb3 Ready | serve\x07",
        Duration::from_secs(20),
    );

    run_hook_with_tasks_root(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"SessionEnd","cwd":"/tmp/serve"}"#,
        &tasks_root,
    );
    pty.wait_for(b"\x1b]0;\x07");
    drop(late);
    drop(server);
    drop(claude);
    wait_for_daemon_exit(&pty.slave_path);
}

#[test]
fn a_renamed_session_shows_its_name_in_place_of_the_project() {
    let directory = tempfile::tempdir().unwrap();
    let transcript = directory.path().join("transcript.jsonl");
    append_records(
        &transcript,
        &[br#"{"type":"custom-title","customTitle":"Smart-Title"}"#],
    );
    let mut pty = Pty::open();
    let claude = sleeper();

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/nulspace-io","transcript_path":{}}}"#,
            serde_json::to_string(&transcript).unwrap()
        ),
    );
    pty.wait_for(b"\x1b]0;Smart-Title | Working | nulspace-io\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","cwd":"/tmp/nulspace-io"}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9c\xb3 Smart-Title | Ready | nulspace-io\x07");

    // A later rename lands on the same session.
    append_records(
        &transcript,
        &[br#"{"type":"custom-title","customTitle":"Renamed-Again"}"#],
    );
    run_hook(
        &pty.slave_path,
        claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/nulspace-io","transcript_path":{}}}"#,
            serde_json::to_string(&transcript).unwrap()
        ),
    );
    pty.wait_for(b"\x1b]0;Renamed-Again | Working | nulspace-io\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"SessionEnd","cwd":"/tmp/nulspace-io"}"#,
    );
    pty.wait_for(b"\x1b]0;\x07");
    drop(claude);
    wait_for_daemon_exit(&pty.slave_path);
}

#[test]
fn a_cli_assigned_session_name_shows_in_place_of_the_project() {
    let directory = tempfile::tempdir().unwrap();
    let transcript = directory.path().join("transcript.jsonl");
    append_records(
        &transcript,
        &[br#"{"type":"agent-setting","agentSetting":"tyler-1","sessionId":"s"}"#],
    );
    let mut pty = Pty::open();
    let claude = sleeper();

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/nulspace-io","transcript_path":{}}}"#,
            serde_json::to_string(&transcript).unwrap()
        ),
    );
    pty.wait_for(b"\x1b]0;tyler-1 | Working | nulspace-io\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"SessionEnd","cwd":"/tmp/nulspace-io"}"#,
    );
    pty.wait_for(b"\x1b]0;\x07");
    drop(claude);
    wait_for_daemon_exit(&pty.slave_path);
}

#[test]
fn a_daemon_steps_aside_when_the_binary_is_replaced() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("claude-title");
    fs::copy(env!("CARGO_BIN_EXE_claude-title"), &binary).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();

    let mut pty = Pty::open();
    let claude = sleeper();

    run_hook_as_claude(
        &binary,
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"SessionStart","cwd":"/tmp/upgrade"}"#,
        Path::new("/var/empty"),
    );
    pty.wait_for(b"\x1b]0;\xe2\x9c\xb3 Ready | upgrade\x07");

    let paths = state::paths_for_tty(&pty.slave_path).unwrap();
    assert!(paths.lock.exists());

    // A cargo install writes a new file and renames it into place, giving the
    // replacement a fresh inode. Reproduce that here.
    let replacement = directory.path().join("claude-title.next");
    fs::copy(env!("CARGO_BIN_EXE_claude-title"), &replacement).unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&replacement, &binary).unwrap();

    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline && daemon_holds_lock(&paths.lock) {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !daemon_holds_lock(&paths.lock),
        "daemon did not step aside after the binary was replaced"
    );

    let _ = fs::remove_file(&paths.state);
    drop(claude);
}

fn sleeper() -> ChildGuard {
    ChildGuard(
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn run_hook(tty: &Path, pid: u32, input: &str) {
    // An empty root: probes settle on Unknown instead of reading real sessions.
    run_hook_with_tasks_root(tty, pid, input, Path::new("/var/empty"));
}

fn run_hook_with_tasks_root(tty: &Path, pid: u32, input: &str, tasks_root: &Path) {
    run_hook_as_claude(
        Path::new(env!("CARGO_BIN_EXE_claude-title")),
        tty,
        pid,
        input,
        tasks_root,
    );
}

struct WatchingClaude {
    binary: PathBuf,
    tty: PathBuf,
    pid: u32,
    tasks_root: PathBuf,
    watched: PathBuf,
    seen: Option<Vec<u8>>,
}

static WATCHING_CLAUDES: Mutex<Vec<WatchingClaude>> = Mutex::new(Vec::new());

// Stands in for Claude Code: it runs the hook, writes the terminal sequence
// the hook hands back into its own terminal, and watches the paths a
// SessionStart hook names.
fn run_hook_as_claude(binary: &Path, tty: &Path, pid: u32, input: &str, tasks_root: &Path) {
    let mut child = Command::new(binary)
        .arg("hook")
        .env_remove("TERM_PROGRAM")
        .env("CLAUDE_CODE_ENTRYPOINT", "cli")
        .env("CLAUDE_TITLE_TASKS_ROOT", tasks_root)
        .env("CLAUDE_TITLE_TTY", tty)
        .env("CLAUDE_TITLE_PID", pid.to_string())
        .env("CLAUDE_PROJECT_DIR", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    if output.stdout.is_empty() {
        return;
    }
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    if let Some(sequence) = answer["terminalSequence"].as_str() {
        OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(tty)
            .unwrap()
            .write_all(sequence.as_bytes())
            .unwrap();
    }
    if let Some(watched) = answer["hookSpecificOutput"]["watchPaths"][0].as_str() {
        let watched = PathBuf::from(watched);
        let mut claudes = WATCHING_CLAUDES.lock().unwrap();
        claudes.retain(|claude| claude.tty != tty);
        claudes.push(WatchingClaude {
            binary: binary.to_path_buf(),
            tty: tty.to_path_buf(),
            pid,
            tasks_root: tasks_root.to_path_buf(),
            seen: fs::read(&watched).ok(),
            watched,
        });
    }
}

fn write_nudged_title(tty: &Path) {
    let changed = {
        let mut claudes = WATCHING_CLAUDES.lock().unwrap();
        claudes
            .iter_mut()
            .find(|claude| claude.tty == tty)
            .and_then(|claude| {
                let now = fs::read(&claude.watched).ok();
                if now == claude.seen {
                    return None;
                }
                claude.seen = now;
                Some((
                    claude.binary.clone(),
                    claude.pid,
                    claude.tasks_root.clone(),
                    claude.watched.clone(),
                ))
            })
    };
    if let Some((binary, pid, tasks_root, watched)) = changed {
        run_hook_as_claude(
            &binary,
            tty,
            pid,
            &format!(
                r#"{{"hook_event_name":"FileChanged","file_path":{}}}"#,
                serde_json::to_string(&watched).unwrap()
            ),
            &tasks_root,
        );
    }
}

fn wait_for_daemon_exit(tty: &Path) {
    let paths = state::paths_for_tty(tty).unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if !daemon_holds_lock(&paths.lock) {
            let _ = fs::remove_file(paths.state);
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("title daemon did not exit");
}

fn daemon_holds_lock(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return false;
    };
    match file.try_lock_exclusive() {
        Ok(()) => false,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => true,
        Err(error) => panic!("failed to inspect daemon lock: {error}"),
    }
}

#[test]
fn concurrent_permission_requests_preserve_every_open_dialog() {
    let mut pty = Pty::open();
    let claude = sleeper();
    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"SessionStart"}"#,
    );
    pty.wait_for(b"Ready");
    thread::scope(|scope| {
        for index in 0..8 {
            let tty = &pty.slave_path;
            let pid = claude.0.id();
            scope.spawn(move || {
                run_hook(
                    tty,
                    pid,
                    &format!(
                        r#"{{"hook_event_name":"PermissionRequest","tool_name":"tool-{index}"}}"#
                    ),
                );
            });
        }
    });
    let paths = state::paths_for_tty(&pty.slave_path).unwrap();
    let current = state::read(&paths.state).unwrap().unwrap();
    assert_eq!(current.value.dialogs.len(), 8);
    drop(claude);
    wait_for_daemon_exit(&pty.slave_path);
}

#[test]
fn subagent_hooks_do_not_overwrite_the_main_session_title() {
    let mut pty = Pty::open();
    let claude = sleeper();
    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"SessionStart","cwd":"/tmp/main"}"#,
    );
    pty.wait_for(b"Ready | main");
    let paths = state::paths_for_tty(&pty.slave_path).unwrap();
    let before = state::read(&paths.state).unwrap().unwrap().raw;
    for event in ["PreToolUse", "PostToolUse", "PostToolBatch"] {
        run_hook(
            &pty.slave_path,
            claude.0.id(),
            &format!(
                r#"{{"hook_event_name":"{event}","agent_id":"worker","tool_name":"Bash","cwd":"/tmp/worker"}}"#
            ),
        );
    }
    assert_eq!(state::read(&paths.state).unwrap().unwrap().raw, before);
    drop(claude);
    wait_for_daemon_exit(&pty.slave_path);
}

#[test]
fn a_session_end_does_not_start_a_new_title_daemon() {
    let mut pty = Pty::open();
    let claude = sleeper();
    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"SessionEnd"}"#,
    );
    assert_eq!(pty.read_for(Duration::from_millis(300)), b"\x1b]0;\x07");
    let paths = state::paths_for_tty(&pty.slave_path).unwrap();
    assert!(!daemon_holds_lock(&paths.lock));
    fs::remove_file(paths.state).unwrap();
}

fn tty_path(fd: RawFd) -> PathBuf {
    let mut buffer = vec![0_i8; 1024];
    let result = unsafe { libc::ttyname_r(fd, buffer.as_mut_ptr(), buffer.len()) };
    assert_eq!(result, 0);
    let bytes = buffer
        .into_iter()
        .take_while(|byte| *byte != 0)
        .map(|byte| byte as u8)
        .collect::<Vec<_>>();
    PathBuf::from(String::from_utf8(bytes).unwrap())
}

const INTERRUPT_RECORD: &[u8] = br#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
const PROMPT_RECORD: &[u8] = br#"{"type":"user","message":{"role":"user","content":"carry on"}}"#;

fn append_records(path: &Path, records: &[&[u8]]) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for record in records {
        file.write_all(record).unwrap();
        file.write_all(b"\n").unwrap();
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}
