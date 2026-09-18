use claude_title::state;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
            libc::close(slave_fd);
            let flags = libc::fcntl(master_fd, libc::F_GETFL);
            assert_ne!(flags, -1);
            assert_ne!(
                libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK),
                -1
            );
        }
        Self {
            master: unsafe { File::from_raw_fd(master_fd) },
            slave_path,
        }
    }

    fn read_for(&mut self, duration: Duration) -> Vec<u8> {
        let deadline = Instant::now() + duration;
        let mut output = Vec::new();
        while Instant::now() < deadline {
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

    fn read_slowly_for(&mut self, duration: Duration) -> Vec<u8> {
        let deadline = Instant::now() + duration;
        let mut output = Vec::new();
        while Instant::now() < deadline {
            let mut buffer = [0; 128];
            if let Ok(count) = self.master.read(&mut buffer) {
                output.extend_from_slice(&buffer[..count]);
            }
            thread::sleep(Duration::from_millis(1));
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
    pty.wait_for(b" Working | example\x07");
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
    assert!(!contains(&sibling_output, b" Working | example"));

    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        r#"{"hook_event_name":"PostToolUse","cwd":"/tmp/example","tool_name":"AskUserQuestion"}"#,
    );
    pty.wait_for(b" Working | example\x07");

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
    pty.wait_for(b" Working | example\x07");

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
    pty.wait_for(b" Working | pause\x07");

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
    pty.wait_for(b" Working | pause\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","cwd":"/tmp/pause","background_tasks":[{"id":"d1","type":"dream","status":"running","description":"dreaming"},{"id":"a1","type":"auto-mode scan","status":"running","description":"scanning"},{"id":"t1","type":"teammate","status":"running","description":"resting"},{"id":"n1","type":"novel_chore","status":"running","description":"future work"}]}"#,
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
    pty.wait_for(b" Working | pause\x07");

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

    let prompt = format!(
        r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/serve","transcript_path":{}}}"#,
        serde_json::to_string(&transcript).unwrap()
    );
    run_hook_with_tasks_root(&pty.slave_path, claude.0.id(), &prompt, &tasks_root);
    pty.wait_for(b" Working | serve\x07");

    run_hook_with_tasks_root(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","session_id":"session","cwd":"/tmp/serve","background_tasks":[{"id":"t1serve","type":"shell","status":"running","description":"dev server","command":"npm run dev"}]}"#,
        &tasks_root,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9c\xb3 Ready | serve\x07");

    run_hook_with_tasks_root(&pty.slave_path, claude.0.id(), &prompt, &tasks_root);
    pty.wait_for(b" Working | serve\x07");

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
    pty.wait_for(b" Working | serve\x07");

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
    pty.wait_for(b" Working | serve\x07");
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
    pty.wait_for(b" Working | Smart-Title\x07");

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","cwd":"/tmp/nulspace-io"}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9c\xb3 Ready | Smart-Title\x07");

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
    pty.wait_for(b" Working | Renamed-Again\x07");

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
    pty.wait_for(b" Working | tyler-1\x07");

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

    let mut child = Command::new(&binary)
        .arg("hook")
        .env("CLAUDE_CODE_ENTRYPOINT", "cli")
        .env("CLAUDE_TITLE_TASKS_ROOT", "/var/empty")
        .env("CLAUDE_TITLE_TTY", &pty.slave_path)
        .env("CLAUDE_TITLE_PID", claude.0.id().to_string())
        .env("CLAUDE_PROJECT_DIR", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"hook_event_name":"SessionStart","cwd":"/tmp/upgrade"}"#)
        .unwrap();
    assert!(child.wait().unwrap().success());
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
    while Instant::now() < deadline && paths.lock.exists() {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !paths.lock.exists(),
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_claude-title"))
        .arg("hook")
        .env("CLAUDE_CODE_ENTRYPOINT", "cli")
        .env("CLAUDE_TITLE_TASKS_ROOT", tasks_root)
        .env("CLAUDE_TITLE_TTY", tty)
        .env("CLAUDE_TITLE_PID", pid.to_string())
        .env("CLAUDE_PROJECT_DIR", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

fn wait_for_daemon_exit(tty: &Path) {
    let paths = state::paths_for_tty(tty).unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if !paths.lock.exists() {
            let _ = fs::remove_file(paths.state);
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("title daemon did not exit");
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

// Claude Code sends each frame as one blocking write; the kernel parks that
// write at the pty's high-water mark, and a title written meanwhile lands
// inside the frame. The frame here is nothing but 19-byte color sequences, so
// any title that lands mid-write splits one and the terminal prints its tail.
#[test]
fn a_title_never_lands_inside_another_writers_escape_sequence() {
    const COLOR: &[u8] = b"\x1b[38;2;78;186;101m";
    let directory = tempfile::tempdir().unwrap();
    let transcript = directory.path().join("transcript.jsonl");
    fs::write(&transcript, b"start\n").unwrap();
    let mut pty = Pty::open();
    let claude = sleeper();

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        &format!(
            r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/frames","transcript_path":{}}}"#,
            serde_json::to_string(&transcript).unwrap()
        ),
    );
    pty.wait_for(b" Working | frames\x07");

    let mut slave = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(&pty.slave_path)
        .unwrap();
    let flooding = Arc::new(AtomicBool::new(true));
    let writer = thread::spawn({
        let flooding = Arc::clone(&flooding);
        let frame = COLOR.repeat(2048);
        move || {
            while flooding.load(Ordering::Relaxed) {
                slave.write_all(&frame).unwrap();
            }
        }
    });
    let mut output = pty.read_slowly_for(Duration::from_millis(1500));
    flooding.store(false, Ordering::Relaxed);
    while !writer.is_finished() {
        output.extend(pty.read_for(Duration::from_millis(50)));
    }
    writer.join().unwrap();
    output.extend(pty.read_for(Duration::from_millis(100)));

    let title_starts: Vec<usize> = output
        .windows(4)
        .enumerate()
        .filter(|(_, window)| *window == b"\x1b]0;")
        .map(|(index, _)| index)
        .collect();
    let split_titles: Vec<String> = title_starts
        .iter()
        .filter(|&&index| index > 0 && !matches!(output[index - 1], b'm' | b'\x07'))
        .map(|&index| {
            String::from_utf8_lossy(
                &output[index.saturating_sub(24)..(index + 24).min(output.len())],
            )
            .into_owned()
        })
        .collect();
    assert!(
        count(&output, COLOR) >= 2048,
        "the flood never reached the terminal"
    );
    assert!(
        split_titles.is_empty(),
        "titles landed inside escape sequences: {split_titles:?}"
    );

    pty.wait_for(b" Working | frames\x07");
    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"SessionEnd","cwd":"/tmp/frames"}"#,
    );
    pty.wait_for(b"\x1b]0;\x07");
    drop(claude);
    wait_for_daemon_exit(&pty.slave_path);
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn between(&mut self, low: usize, high: usize) -> usize {
        low + self.below(high - low)
    }
}

const GLYPHS: [&str; 8] = ["▓", "▒", "░", "✳", "⠋", "⠙", "漢", "字"];

// One burst of what a terminal app writes: text, wide glyphs, colors, cursor
// motion, hyperlinks, and its own tab titles, in a random order and size.
fn app_burst(rng: &mut XorShift, buffer: &mut Vec<u8>) {
    let target = rng.between(200, 40_000);
    while buffer.len() < target {
        match rng.below(12) {
            0..=3 => {
                for _ in 0..rng.between(1, 40) {
                    buffer.push(b" abcdefghijklmnopqrstuvwxyz0123456789"[rng.below(37)]);
                }
            }
            4 | 5 => buffer.extend_from_slice(GLYPHS[rng.below(GLYPHS.len())].as_bytes()),
            6 => {
                let color = format!(
                    "\x1b[38;2;{};{};{}m",
                    rng.below(256),
                    rng.below(256),
                    rng.below(256)
                );
                buffer.extend_from_slice(color.as_bytes());
            }
            7 => buffer.extend_from_slice(b"\x1b[0m"),
            8 => {
                let motion: &[&[u8]] = &[
                    b"\x1b[3A",
                    b"\x1b[K",
                    b"\x1b[2J",
                    b"\x1b[?25l",
                    b"\x1b[?25h",
                    b"\x1b[1;5H",
                    b"\x1b[999;999H",
                ];
                buffer.extend_from_slice(motion[rng.below(motion.len())]);
            }
            9 => {
                let terminator: &[u8] = if rng.below(2) == 0 {
                    b"\x07"
                } else {
                    b"\x1b\\"
                };
                let link = format!("\x1b]8;;https://example.com/{}", rng.below(1000));
                buffer.extend_from_slice(link.as_bytes());
                buffer.extend_from_slice(terminator);
                buffer.extend_from_slice(b"link");
                buffer.extend_from_slice(b"\x1b]8;;");
                buffer.extend_from_slice(terminator);
            }
            _ => {
                let title = format!("\x1b]0;app frame {}\x07", rng.below(1000));
                buffer.extend_from_slice(title.as_bytes());
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum VtState {
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

// Walks the byte stream the way a terminal would and reports every place a
// sequence or a multi-byte character was cut short by something else.
fn sequence_breaks(output: &[u8]) -> Vec<String> {
    let mut breaks = Vec::new();
    let mut state = VtState::Ground;
    let mut utf8_pending = 0;
    let mut osc_start = 0;
    let mut osc_contents: Vec<Vec<u8>> = Vec::new();
    for (index, &byte) in output.iter().enumerate() {
        let context = || {
            String::from_utf8_lossy(
                &output[index.saturating_sub(32)..(index + 32).min(output.len())],
            )
            .into_owned()
        };
        if utf8_pending > 0 {
            if (0x80..=0xBF).contains(&byte) {
                utf8_pending -= 1;
                continue;
            }
            breaks.push(format!("character cut at {index}: {:?}", context()));
            utf8_pending = 0;
        }
        match state {
            VtState::Ground => match byte {
                0x1b => state = VtState::Escape,
                0xC0..=0xDF => utf8_pending = 1,
                0xE0..=0xEF => utf8_pending = 2,
                0xF0..=0xF7 => utf8_pending = 3,
                _ => {}
            },
            VtState::Escape => match byte {
                b'[' => state = VtState::Csi,
                b']' => {
                    state = VtState::Osc;
                    osc_start = index + 1;
                }
                0x1b => breaks.push(format!("escape cut at {index}: {:?}", context())),
                _ => state = VtState::Ground,
            },
            VtState::Csi => match byte {
                0x40..=0x7E => state = VtState::Ground,
                0x1b => {
                    breaks.push(format!("CSI cut at {index}: {:?}", context()));
                    state = VtState::Escape;
                }
                _ => {}
            },
            VtState::Osc => match byte {
                0x07 => {
                    osc_contents.push(output[osc_start..index].to_vec());
                    state = VtState::Ground;
                }
                0x1b => state = VtState::OscEscape,
                _ => {}
            },
            VtState::OscEscape => {
                if byte == b'\\' {
                    osc_contents.push(output[osc_start..index - 1].to_vec());
                    state = VtState::Ground;
                } else {
                    breaks.push(format!("OSC cut at {index}: {:?}", context()));
                    state = if byte == b'[' {
                        VtState::Csi
                    } else {
                        VtState::Escape
                    };
                }
            }
        }
    }
    for content in osc_contents {
        let text = String::from_utf8_lossy(&content).into_owned();
        let title = text.strip_prefix("0;");
        let daemon_title = title.is_some_and(|title| {
            title.is_empty()
                || title.ends_with(" | fuzz")
                    && (title.starts_with("✳ Ready")
                        || title.starts_with("⧗ Waiting")
                        || title.starts_with("⚠ Action required")
                        || FRAMES.iter().any(|frame| title.starts_with(frame)))
        });
        let app_sequence =
            title.is_some_and(|title| title.starts_with("app frame ")) || text.starts_with("8;;");
        if !daemon_title && !app_sequence {
            breaks.push(format!("unexpected sequence contents: {text:?}"));
        }
    }
    breaks
}

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// Any content, any drain rate, every daemon state: no title may land inside
// another writer's sequence or character, and no app bytes may land inside a
// title. The judge is a terminal parser over what the terminal received.
#[test]
fn a_title_never_interrupts_any_terminal_sequence() {
    let directory = tempfile::tempdir().unwrap();
    let transcript = directory.path().join("transcript.jsonl");
    fs::write(&transcript, b"start\n").unwrap();
    let mut pty = Pty::open();
    let claude = sleeper();
    let prompt = format!(
        r#"{{"hook_event_name":"UserPromptSubmit","cwd":"/tmp/fuzz","transcript_path":{}}}"#,
        serde_json::to_string(&transcript).unwrap()
    );
    let dialog = r#"{"hook_event_name":"Notification","cwd":"/tmp/fuzz","message":"Claude needs your permission to use Bash"}"#;
    let shell_left_running = r#"{"hook_event_name":"Stop","cwd":"/tmp/fuzz","background_tasks":[{"id":"b1","type":"shell","status":"running","description":"sleep","command":"sleep 5"}]}"#;

    run_hook(&pty.slave_path, claude.0.id(), &prompt);
    pty.wait_for(b" Working | fuzz\x07");

    let mut output = Vec::new();
    for seed in [
        0x9E37_79B9_7F4A_7C15_u64,
        0xD1B5_4A32_D192_ED03,
        0x2545_F491_4F6C_DD1D,
    ] {
        let mut slave = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&pty.slave_path)
            .unwrap();
        let flooding = Arc::new(AtomicBool::new(true));
        let writer = thread::spawn({
            let flooding = Arc::clone(&flooding);
            move || {
                let mut rng = XorShift(seed);
                let mut burst = Vec::new();
                while flooding.load(Ordering::Relaxed) {
                    burst.clear();
                    app_burst(&mut rng, &mut burst);
                    slave.write_all(&burst).unwrap();
                    thread::sleep(Duration::from_millis(rng.between(5, 40) as u64));
                }
            }
        });
        let seed_start = output.len();
        output.extend(pty.read_slowly_for(Duration::from_millis(150)));
        run_hook(&pty.slave_path, claude.0.id(), dialog);
        output.extend(pty.read_for(Duration::from_millis(150)));
        run_hook(&pty.slave_path, claude.0.id(), shell_left_running);
        output.extend(pty.read_slowly_for(Duration::from_millis(150)));
        run_hook(&pty.slave_path, claude.0.id(), &prompt);
        output.extend(pty.read_for(Duration::from_millis(150)));
        // A loaded machine drains and settles more slowly; keep the flood
        // going until this seed has seen real interleaving, within a bound.
        let deadline = Instant::now() + Duration::from_millis(2500);
        let mut slow = true;
        while Instant::now() < deadline
            && (output.len() - seed_start < 50_000
                || count(&output[seed_start..], b" | fuzz\x07") < 2)
        {
            output.extend(if slow {
                pty.read_slowly_for(Duration::from_millis(100))
            } else {
                pty.read_for(Duration::from_millis(100))
            });
            slow = !slow;
        }
        flooding.store(false, Ordering::Relaxed);
        while !writer.is_finished() {
            output.extend(pty.read_for(Duration::from_millis(50)));
        }
        writer.join().unwrap();
    }
    output.extend(pty.read_for(Duration::from_millis(100)));

    let breaks = sequence_breaks(&output);
    assert!(
        breaks.is_empty(),
        "terminal stream was corrupted: {breaks:#?}"
    );
    assert!(
        output.len() >= 150_000,
        "the flood only delivered {} bytes",
        output.len()
    );
    assert!(count(&output, b"\x1b]0;app frame ") >= 20);
    assert!(count(&output, b"\x1b]8;;") >= 20);
    let daemon_titles = count(&output, b" | fuzz\x07");
    assert!(
        daemon_titles >= 6,
        "only {daemon_titles} daemon titles landed during the flood"
    );

    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"Stop","cwd":"/tmp/fuzz"}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9c\xb3 Ready | fuzz\x07");
    run_hook(
        &pty.slave_path,
        claude.0.id(),
        r#"{"hook_event_name":"SessionEnd","cwd":"/tmp/fuzz"}"#,
    );
    pty.wait_for(b"\x1b]0;\x07");
    drop(claude);
    wait_for_daemon_exit(&pty.slave_path);
}

#[test]
fn the_parser_flags_titles_that_cut_sequences_and_characters() {
    let inside_color = b"\x1b[38;2;7\x1b]0;\xe2\xa0\xb8 Working | fuzz\x078;186;101m";
    assert_eq!(sequence_breaks(inside_color).len(), 1);
    let inside_glyph = b"\xe2\x96\x1b]0;\xe2\x9c\xb3 Ready | fuzz\x07\x93";
    assert_eq!(sequence_breaks(inside_glyph).len(), 1);
    let inside_link = b"\x1b]8;;https://a\x1b]0;\xe2\x9c\xb3 Ready | fuzz\x07\x07";
    assert_eq!(sequence_breaks(inside_link).len(), 1);
    let clean = b"\x1b[38;2;7;8;9m\xe2\x96\x93\x1b]0;\xe2\x9c\xb3 Ready | fuzz\x07\x1b]8;;x\x1b\\y\x1b]0;app frame 1\x07";
    assert!(sequence_breaks(clean).is_empty());
}
