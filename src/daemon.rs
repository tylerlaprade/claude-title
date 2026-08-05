use crate::probe;
use crate::state::{self, StateKind, StoredState};
use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const INTERRUPT_MARKER: &[u8] =
    b"\"content\":[{\"type\":\"text\",\"text\":\"[Request interrupted by user";
const SHELL_PROBE_INTERVAL: Duration = Duration::from_secs(2);

// A shell that vanishes after finishing wakes the session within a beat, and
// that wake rewrites the state before two probes pass. Two consecutive misses
// therefore mean the shell ended without a wake — killed from the task list —
// and nothing else will clear the waiting title.
const SHELL_GONE_MISSES: u8 = 2;

struct PendingShell {
    task_id: String,
    misses: u8,
}

struct ShellWatch {
    raw: Vec<u8>,
    shells: Vec<PendingShell>,
    last_probe: Instant,
}

pub fn run(tty_path: &Path, state_path: &Path, lock_path: &Path, initial_pid: u32) -> Result<()> {
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let mut lock = open_lock(lock_path)?;
    match FileExt::try_lock_exclusive(&lock) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to lock {}", lock_path.display()));
        }
    }
    lock.set_len(0)?;
    lock.write_all(format!("{}\n", std::process::id()).as_bytes())?;
    lock.flush()?;

    let result = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(tty_path)
        .with_context(|| format!("failed to open {}", tty_path.display()))
        .and_then(|mut tty| run_loop(&mut tty, state_path, initial_pid));
    remove_own_lock(&mut lock, lock_path);
    result
}

fn run_loop(tty: &mut File, state_path: &Path, initial_pid: u32) -> Result<()> {
    let mut mode = None;
    let mut static_title: Option<(StateKind, String)> = None;
    let mut frame = 0;
    let mut monitor_pid = initial_pid;
    let mut transcript_path = None;
    let mut transcript_start = 0;
    let mut transcript_position = 0;
    let mut last_scan = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let mut last_liveness_check = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let mut ended_dead_since = None;
    let mut shell_watch: Option<ShellWatch> = None;

    loop {
        let now = Instant::now();
        if let Some(current) = state::read(state_path)? {
            monitor_pid = current.value.claude_pid;
            let new_mode = current.value.kind;
            if matches!(new_mode, StateKind::Busy | StateKind::Waiting)
                && (current.value.transcript_path != transcript_path
                    || current.value.transcript_offset != transcript_start)
            {
                transcript_path = current.value.transcript_path.clone();
                transcript_start = current.value.transcript_offset;
                transcript_position = transcript_start;
            }
            if mode != Some(new_mode) {
                mode = Some(new_mode);
                if new_mode == StateKind::Busy {
                    frame = 0;
                }
            }

            match new_mode {
                StateKind::Busy => {
                    static_title = None;
                    write_title(
                        tty,
                        &format!("{} Working | {}", FRAMES[frame], current.value.project),
                    )?;
                    frame = (frame + 1) % FRAMES.len();
                }
                StateKind::Idle | StateKind::Unknown => {
                    let title = (new_mode, current.value.project.clone());
                    if static_title.as_ref() != Some(&title) {
                        write_title(tty, &format!("✳ Ready | {}", current.value.project))?;
                        static_title = Some(title);
                    }
                }
                StateKind::Pending => {
                    let title = (StateKind::Pending, current.value.project.clone());
                    if static_title.as_ref() != Some(&title) {
                        write_title(tty, &format!("⧗ Waiting | {}", current.value.project))?;
                        static_title = Some(title);
                    }
                }
                StateKind::Waiting => {
                    let title = (StateKind::Waiting, current.value.project.clone());
                    if static_title.as_ref() != Some(&title) {
                        write_title(
                            tty,
                            &format!("⚠ Action required | {}", current.value.project),
                        )?;
                        static_title = Some(title);
                    }
                }
                StateKind::End => {
                    let title = (StateKind::End, current.value.project.clone());
                    if static_title.as_ref() != Some(&title) {
                        write_title(tty, "")?;
                        static_title = Some(title);
                    }
                }
            }

            if matches!(new_mode, StateKind::Busy | StateKind::Waiting)
                && now.duration_since(last_scan) >= Duration::from_millis(500)
            {
                let (interrupted, position) = transcript_has_interrupt(
                    transcript_path.as_deref(),
                    transcript_start,
                    transcript_position,
                );
                transcript_position = position;
                last_scan = now;
                if interrupted && set_idle_if_unchanged(state_path, &current)? {
                    mode = Some(StateKind::Idle);
                    write_title(tty, &format!("✳ Ready | {}", current.value.project))?;
                    static_title = Some((StateKind::Idle, current.value.project.clone()));
                } else if interrupted {
                    transcript_position = transcript_start;
                }
            }

            // While only shells hold the waiting title, keep probing them: one
            // that starts serving after the turn ended, or that was killed
            // without the wake a completion brings, would otherwise leave the
            // title stuck until the next hook event.
            if new_mode == StateKind::Pending
                && !current.value.pending_beyond_shells
                && !current.value.pending_shells.is_empty()
            {
                match &shell_watch {
                    Some(watch) if watch.raw == current.raw => {}
                    _ => {
                        shell_watch = Some(ShellWatch {
                            raw: current.raw.clone(),
                            shells: current
                                .value
                                .pending_shells
                                .iter()
                                .map(|task_id| PendingShell {
                                    task_id: task_id.clone(),
                                    misses: 0,
                                })
                                .collect(),
                            last_probe: now,
                        });
                    }
                }
                if let Some(watch) = &mut shell_watch
                    && now.duration_since(watch.last_probe) >= SHELL_PROBE_INTERVAL
                {
                    watch.last_probe = now;
                    let task_ids: Vec<String> = watch
                        .shells
                        .iter()
                        .map(|shell| shell.task_id.clone())
                        .collect();
                    let verdicts = probe::tasks(&current.value.pending_session, &task_ids);
                    let mut kept = Vec::new();
                    for (mut shell, verdict) in watch.shells.drain(..).zip(verdicts) {
                        match verdict {
                            probe::ShellProbe::Serving => {}
                            probe::ShellProbe::Gone => {
                                shell.misses += 1;
                                if shell.misses < SHELL_GONE_MISSES {
                                    kept.push(shell);
                                }
                            }
                            probe::ShellProbe::Running | probe::ShellProbe::Unknown => {
                                shell.misses = 0;
                                kept.push(shell);
                            }
                        }
                    }
                    watch.shells = kept;
                    if watch.shells.is_empty() && set_idle_if_unchanged(state_path, &current)? {
                        mode = Some(StateKind::Idle);
                        write_title(tty, &format!("✳ Ready | {}", current.value.project))?;
                        static_title = Some((StateKind::Idle, current.value.project.clone()));
                    }
                }
            } else {
                shell_watch = None;
            }
        }

        if now.duration_since(last_liveness_check) >= Duration::from_secs(1) {
            if process_alive(monitor_pid) {
                ended_dead_since = None;
            } else if mode == Some(StateKind::End) {
                let dead_since = ended_dead_since.get_or_insert(now);
                if now.duration_since(*dead_since) >= Duration::from_secs(1) {
                    let latest = state::read(state_path)?;
                    let handed_off = latest.as_ref().is_some_and(|stored| {
                        stored.value.kind != StateKind::End
                            || stored.value.claude_pid != monitor_pid
                    });
                    if handed_off {
                        ended_dead_since = None;
                    } else {
                        break;
                    }
                }
            } else {
                write_title(tty, "")?;
                break;
            }
            last_liveness_check = now;
        }

        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn clean_title(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character >= ' ' && *character != '\u{7f}')
        .collect()
}

fn write_title(tty: &mut File, title: &str) -> Result<()> {
    tty.write_all(format!("\u{1b}]0;{}\u{7}", clean_title(title)).as_bytes())
        .context("failed to write terminal title")
}

fn process_alive(pid: u32) -> bool {
    if pid > i32::MAX as u32 {
        return false;
    }
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn transcript_has_interrupt(path: Option<&Path>, start: u64, position: u64) -> (bool, u64) {
    let Some(path) = path else {
        return (false, position);
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return (false, position),
    };
    let length = file.metadata().map_or(0, |metadata| metadata.len());
    let position = if position > length { start } else { position };
    let scan_from = start.max(position.saturating_sub(INTERRUPT_MARKER.len() as u64));
    if file.seek(SeekFrom::Start(scan_from)).is_err() {
        return (false, position);
    }
    let mut chunk = Vec::new();
    if file.read_to_end(&mut chunk).is_err() {
        return (false, position);
    }
    let end = scan_from + chunk.len() as u64;
    (
        chunk
            .windows(INTERRUPT_MARKER.len())
            .any(|window| window == INTERRUPT_MARKER),
        end,
    )
}

fn set_idle_if_unchanged(path: &Path, observed: &StoredState) -> Result<bool> {
    let Some(latest) = state::read(path)? else {
        return Ok(false);
    };
    if latest.raw != observed.raw {
        return Ok(false);
    }
    let mut replacement = observed.value.clone();
    replacement.kind = StateKind::Idle;
    replacement.epoch = state::epoch();
    replacement.pending_session = String::new();
    replacement.pending_shells = Vec::new();
    replacement.pending_beyond_shells = false;
    state::write(path, &replacement)?;
    Ok(true)
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

fn remove_own_lock(lock: &mut File, path: &Path) {
    let _ = lock.seek(SeekFrom::Start(0));
    let mut value = String::new();
    let _ = lock.read_to_string(&mut value);
    if value.trim() == std::process::id().to_string() {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_removes_control_characters() {
        assert_eq!(clean_title("one\n\ttwo\u{7f} ✳"), "onetwo ✳");
    }

    #[test]
    fn interrupt_marker_can_cross_reads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        let prefix = b"{\"content\":[{\"type\":\"text\",\"text\":\"[Request";
        fs::write(&path, prefix).unwrap();
        let (_, first_position) = transcript_has_interrupt(Some(&path), 0, 0);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b" interrupted by user for tool use]\"}")
            .unwrap();
        let (found, _) = transcript_has_interrupt(Some(&path), 0, first_position);
        assert!(found);
    }

    #[test]
    fn scan_does_not_match_before_prompt_offset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        fs::write(
            &path,
            b"\"content\":[{\"type\":\"text\",\"text\":\"[Request interrupted by user",
        )
        .unwrap();
        let offset = fs::metadata(&path).unwrap().len();
        fs::write(
            &path,
            [
                fs::read(&path).unwrap(),
                b"\n{\"content\":[{\"type\":\"text\",\"text\":\"done\"}".to_vec(),
            ]
            .concat(),
        )
        .unwrap();
        let (found, _) = transcript_has_interrupt(Some(&path), offset, offset);
        assert!(!found);
    }
}
