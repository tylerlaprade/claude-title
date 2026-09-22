use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const OUTPUT_SETTLE_READS: u8 = 3;
const OUTPUT_SETTLE_GAP: Duration = Duration::from_millis(1);
const OUTPUT_SETTLE_PATIENCE: Duration = Duration::from_millis(50);

pub(crate) enum TerminalTitle {
    Osc(File),
    #[cfg(target_os = "macos")]
    Ghostty(ghostty::Connection),
}

impl TerminalTitle {
    pub(crate) fn open(tty: &Path) -> Result<Self> {
        #[cfg(target_os = "macos")]
        if std::env::var("TERM_PROGRAM").as_deref() == Ok("ghostty") {
            return ghostty::Connection::open(tty).map(Self::Ghostty);
        }
        OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(tty)
            .with_context(|| format!("failed to open {}", tty.display()))
            .map(Self::Osc)
    }

    pub(crate) fn write(&mut self, title: &str) -> Result<bool> {
        let title = clean_title(title);
        match self {
            Self::Osc(tty) => {
                if !output_settled(tty) {
                    return Ok(false);
                }
                tty.write_all(format!("\u{1b}]0;{title}\u{7}").as_bytes())
                    .context("failed to write terminal title")?;
            }
            #[cfg(target_os = "macos")]
            Self::Ghostty(connection) => connection.write(&title)?,
        }
        Ok(true)
    }
}

fn clean_title(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character >= ' ' && *character != '\u{7f}')
        .collect()
}

fn queued_output(tty: &File) -> Option<libc::c_int> {
    let mut queued: libc::c_int = 0;
    let result = unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCOUTQ, &raw mut queued) };
    (result == 0).then_some(queued)
}

fn output_settled(tty: &File) -> bool {
    let deadline = Instant::now() + OUTPUT_SETTLE_PATIENCE;
    let mut empty_reads = 0;
    loop {
        match queued_output(tty) {
            None => return true,
            Some(0) => {
                empty_reads += 1;
                if empty_reads == OUTPUT_SETTLE_READS {
                    return true;
                }
            }
            Some(_) => empty_reads = 0,
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(OUTPUT_SETTLE_GAP);
    }
}

#[cfg(target_os = "macos")]
mod ghostty {
    use anyhow::{Context, Result, bail};
    use std::io::{BufRead, BufReader, Write};
    use std::path::Path;
    use std::process::{Child, ChildStdout, Command, Stdio};
    use std::time::{Duration, Instant};

    const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

    const SCRIPT: &str = include_str!("ghostty.js");

    pub(crate) struct Connection {
        child: Child,
        replies: BufReader<ChildStdout>,
    }

    impl Connection {
        pub(super) fn open(tty: &Path) -> Result<Self> {
            Self::start(SCRIPT, tty)
        }

        fn start(script: &str, tty: &Path) -> Result<Self> {
            let mut child = Command::new("/usr/bin/osascript")
                .args(["-l", "JavaScript", "-e", script])
                .arg(tty)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .context("failed to start Ghostty title connection")?;
            let replies =
                BufReader::new(child.stdout.take().context("missing Ghostty reply pipe")?);
            let mut connection = Self { child, replies };
            crate::subprocess::nonblocking(connection.replies.get_ref())?;
            crate::subprocess::nonblocking(connection.child.stdin.as_ref().unwrap())?;
            connection.receive()?;
            Ok(connection)
        }

        pub(super) fn write(&mut self, title: &str) -> Result<()> {
            let input = self
                .child
                .stdin
                .as_mut()
                .context("missing Ghostty input pipe")?;
            serde_json::to_writer(&mut *input, title)?;
            input.write_all(b"\n")?;
            self.receive()
        }

        fn receive(&mut self) -> Result<()> {
            let deadline = Instant::now() + REPLY_TIMEOUT;
            let mut reply = Vec::new();
            loop {
                match self.replies.read_until(b'\n', &mut reply) {
                    Ok(0) => bail!("Ghostty title connection closed"),
                    Ok(_) if reply.ends_with(b"\n") => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error).context("failed to read Ghostty reply"),
                }
                if Instant::now() >= deadline {
                    bail!("Ghostty title reply timed out");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if reply != b"true\n" {
                bail!(
                    "Ghostty title update failed: {}",
                    String::from_utf8_lossy(&reply).trim()
                );
            }
            Ok(())
        }
    }

    impl Drop for Connection {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::title::TerminalTitle;
        use std::fs::File;
        use std::io::Read;
        use std::os::fd::FromRawFd;

        fn mock_script(body: &str) -> String {
            SCRIPT.replace("connectGhostty(argv[0])", body)
        }

        #[test]
        fn an_absent_ghostty_is_not_launched() {
            let script = SCRIPT.replace(
                "$.NSRunningApplication.runningApplicationsWithBundleIdentifier(\"com.mitchellh.ghostty\")",
                "({count: 0})",
            );
            let script = script.replace("Application(", "testApplication(");
            let script = format!(
                "const testApplication = () => {{ throw new Error('Unexpected application lookup'); }};\n{script}"
            );
            let error = Connection::start(&script, Path::new("/dev/test"))
                .err()
                .unwrap();
            assert!(error.to_string().contains("Ghostty terminal not found"));
        }

        #[test]
        fn a_closed_ghostty_is_not_reacquired_for_a_title_update() {
            let script =
                mock_script("({setTitle: () => { throw new Error('Process has exited'); }})");
            let mut connection = Connection::start(&script, Path::new("/dev/test")).unwrap();
            assert!(
                connection
                    .write("Ready")
                    .unwrap_err()
                    .to_string()
                    .contains("Process has exited")
            );
        }

        #[test]
        fn a_stalled_reply_times_out_and_reaps_the_helper() {
            let mut connection = Connection::start(
                "ObjC.import('Foundation'); this.run = () => { $.NSFileHandle.fileHandleWithStandardOutput.writeData($('true\\n').dataUsingEncoding($.NSUTF8StringEncoding)); $.NSThread.sleepForTimeInterval(30); };",
                Path::new("/dev/test"),
            ).unwrap();
            let helper_pid = connection.child.id();
            let started = Instant::now();
            assert!(
                connection
                    .write("Ready")
                    .unwrap_err()
                    .to_string()
                    .contains("timed out")
            );
            assert!(started.elapsed() < Duration::from_secs(5));
            drop(connection);
            assert_eq!(unsafe { libc::kill(helper_pid as i32, 0) }, -1);
        }

        #[test]
        #[ignore = "requires a local Ghostty installation"]
        fn a_native_process_target_does_not_relaunch_a_quit_ghostty() {
            struct GhosttyProcess(Child);
            impl Drop for GhosttyProcess {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let mut ghostty = GhosttyProcess(
                Command::new("/Applications/Ghostty.app/Contents/MacOS/ghostty")
                    .args([
                        "--config-default-files=false",
                        "--initial-window=false",
                        "--window-save-state=never",
                        "--auto-update=off",
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let script = SCRIPT.to_string()
                + r#"
this.run = argv => {
    try {
        const pid = Number(argv[0]);
        terminals(pid);
        reply(true);
        while ($.getchar() !== 10) {}
        terminals(pid);
        reply(true);
    } catch (error) { reply(String(error)); }
};
"#;
            let pid = ghostty.0.id().to_string();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut connection = loop {
                match Connection::start(&script, Path::new(&pid)) {
                    Ok(connection) => break connection,
                    Err(error) if Instant::now() >= deadline => {
                        panic!("native connection failed: {error}")
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            };
            ghostty.0.kill().unwrap();
            ghostty.0.wait().unwrap();
            let error = connection.write("Ready").unwrap_err();
            assert!(!error.to_string().contains("timed out"), "{error}");
        }

        #[test]
        fn native_titles_preserve_unicode_and_treat_titles_as_data() {
            let script = mock_script(r"({setTitle: title => title === expected.shift()})");
            let script = format!(
                "const expected = ['⠋ Working | 漢字', '✳ Ready | \"quoted\" \\\\ path', ''];\n{script}"
            );
            let mut connection = Connection::start(&script, Path::new("/dev/test")).unwrap();
            for title in ["⠋ Working | 漢字", "✳ Ready | \"quoted\" \\ path", ""] {
                connection.write(title).unwrap();
            }
        }

        #[test]
        fn a_missing_surface_does_not_fall_back_to_terminal_output() {
            let script = mock_script("(() => { throw new Error('Missing surface'); })()");
            assert!(Connection::start(&script, Path::new("/dev/missing")).is_err());
        }

        #[test]
        fn rejected_actions_are_reported() {
            let script = mock_script("({setTitle: () => false})");
            let mut connection = Connection::start(&script, Path::new("/dev/test")).unwrap();
            assert!(connection.write("Ready").is_err());
        }

        #[test]
        fn native_titles_do_not_split_a_partially_written_divider() {
            let script = mock_script("({setTitle: () => true})");
            let connection = Connection::start(&script, Path::new("/dev/test")).unwrap();
            let helper_pid = connection.child.id();
            let mut title = TerminalTitle::Ghostty(connection);
            let mut master = -1;
            let mut slave = -1;
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &raw mut master,
                        &raw mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            let mut master = unsafe { File::from_raw_fd(master) };
            let mut slave = unsafe { File::from_raw_fd(slave) };
            slave.write_all(b"\xe2").unwrap();
            let mut output = [0; 3];
            master.read_exact(&mut output[..1]).unwrap();
            assert!(title.write("⠋ Working | divider").unwrap());
            slave.write_all(b"\x94\x80").unwrap();
            master.read_exact(&mut output[1..]).unwrap();
            assert_eq!(std::str::from_utf8(&output).unwrap(), "─");
            drop(title);
            assert_eq!(unsafe { libc::kill(helper_pid as i32, 0) }, -1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_removes_control_characters() {
        assert_eq!(clean_title("one\n\ttwo\u{7f} ✳"), "onetwo ✳");
    }
}
