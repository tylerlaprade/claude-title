use claude_title::state;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
                &mut master_fd,
                &mut slave_fd,
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
    fs::write(
        &transcript,
        b"\"content\":[{\"type\":\"text\",\"text\":\"[Request interrupted by user",
    )
    .unwrap();
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

    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        r#"{"hook_event_name":"Notification","cwd":"/tmp/example"}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9a\xa0 Action required | example\x07");

    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        r#"{"hook_event_name":"PostToolUse","cwd":"/tmp/example"}"#,
    );
    pty.wait_for(b" Working | example\x07");

    let mut transcript_file = OpenOptions::new().append(true).open(&transcript).unwrap();
    transcript_file
        .write_all(b"\n\"content\":[{\"type\":\"text\",\"text\":\"[Request interrupted by user")
        .unwrap();
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

    run_hook(
        &pty.slave_path,
        first_claude.0.id(),
        r#"{"hook_event_name":"Notification","cwd":"/tmp/example"}"#,
    );
    pty.wait_for(b"\x1b]0;\xe2\x9a\xa0 Action required | example\x07");

    let mut transcript_file = OpenOptions::new().append(true).open(&transcript).unwrap();
    transcript_file
        .write_all(b"\n\"content\":[{\"type\":\"text\",\"text\":\"[Request interrupted by user")
        .unwrap();
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
