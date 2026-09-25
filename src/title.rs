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
    Ghostty(crate::ghostty::Connection),
}

#[derive(Debug)]
pub(crate) struct TerminalDetached;

impl std::fmt::Display for TerminalDetached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session no longer belongs to a Ghostty process")
    }
}

impl std::error::Error for TerminalDetached {}

impl TerminalTitle {
    pub(crate) fn open(tty: &Path, owner: u32) -> Result<Self> {
        if std::env::var("TERM_PROGRAM").as_deref() == Ok("ghostty") {
            let ghostty =
                crate::probe::terminal_ancestor(owner, "ghostty")?.ok_or(TerminalDetached)?;
            #[cfg(target_os = "macos")]
            return crate::ghostty::Connection::open(tty, ghostty).map(Self::Ghostty);
            #[cfg(not(target_os = "macos"))]
            let _ = ghostty;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_removes_control_characters() {
        assert_eq!(clean_title("one\n\ttwo\u{7f} ✳"), "onetwo ✳");
    }
}
