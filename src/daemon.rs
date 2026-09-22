use crate::probe;
use crate::state::{self, State, StateKind, StoredState};
use crate::title::TerminalTitle;
use anyhow::{Context, Result};
use fs2::FileExt;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const INTERRUPT_MARKER: &[u8] =
    b"\"content\":[{\"type\":\"text\",\"text\":\"[Request interrupted by user";
const USER_RECORD: &[u8] = b"\"type\":\"user\"";
const SHELL_PROBE_INTERVAL: Duration = Duration::from_secs(2);
const OUTPUT_SETTLE_PATIENCE: Duration = Duration::from_millis(50);
const EXIT_CLEAR_PATIENCE: Duration = Duration::from_secs(2);

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

    let result = TerminalTitle::open(tty_path)
        .and_then(|mut tty| run_loop(&mut tty, state_path, initial_pid));
    lock.set_len(0)?;
    result
}

fn run_loop(tty: &mut TerminalTitle, state_path: &Path, initial_pid: u32) -> Result<()> {
    let mut mode = None;
    let mut static_title: Option<(StateKind, String)> = None;
    let mut frame = 0;
    let mut monitor_pid = initial_pid;
    let mut transcript_path = None;
    let mut transcript_start = 0;
    let mut transcript_position = 0;
    let mut transcript_interrupted = false;
    let mut last_scan = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let mut last_liveness_check = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let mut ended_dead_since = None;
    let mut shell_watch: Option<ShellWatch> = None;
    let executable_at_start = executable_signature();

    loop {
        let now = Instant::now();
        if let Some(current) = state::read(state_path)? {
            monitor_pid = current.value.claude_pid;
            let new_mode = current.value.kind;
            if matches!(new_mode, StateKind::Busy | StateKind::Waiting)
                && (current.value.transcript_path != transcript_path
                    || current.value.transcript_offset != transcript_start)
            {
                transcript_path.clone_from(&current.value.transcript_path);
                transcript_start = current.value.transcript_offset;
                transcript_position = transcript_start;
                transcript_interrupted = false;
            }
            if mode != Some(new_mode) {
                mode = Some(new_mode);
                if new_mode == StateKind::Busy {
                    frame = 0;
                }
            }

            let label = title_label(&current.value);
            match new_mode {
                StateKind::Busy => {
                    static_title = None;
                    if tty.write(&format!("{} Working | {}", FRAMES[frame], label))? {
                        frame = (frame + 1) % FRAMES.len();
                    }
                }
                StateKind::Idle | StateKind::Unknown => {
                    let title = (new_mode, label.to_string());
                    if static_title.as_ref() != Some(&title)
                        && tty.write(&format!("✳ Ready | {label}"))?
                    {
                        static_title = Some(title);
                    }
                }
                StateKind::Pending => {
                    let title = (StateKind::Pending, label.to_string());
                    if static_title.as_ref() != Some(&title)
                        && tty.write(&format!("⧗ Waiting | {label}"))?
                    {
                        static_title = Some(title);
                    }
                }
                StateKind::Waiting => {
                    let title = (StateKind::Waiting, label.to_string());
                    if static_title.as_ref() != Some(&title)
                        && tty.write(&format!("⚠ Action required | {label}"))?
                    {
                        static_title = Some(title);
                    }
                }
                StateKind::End => {
                    let title = (StateKind::End, label.to_string());
                    if static_title.as_ref() != Some(&title) && tty.write("")? {
                        static_title = Some(title);
                    }
                }
            }

            if matches!(new_mode, StateKind::Busy | StateKind::Waiting)
                && now.duration_since(last_scan) >= Duration::from_millis(500)
            {
                let was_interrupted = transcript_interrupted;
                let (interrupted, position) = transcript_interrupt_state(
                    transcript_path.as_deref(),
                    transcript_start,
                    transcript_position,
                    was_interrupted,
                );
                transcript_position = position;
                transcript_interrupted = interrupted;
                last_scan = now;
                if interrupted && !was_interrupted {
                    if set_idle_if_unchanged(state_path, &current)? {
                        let label = title_label(&current.value);
                        mode = Some(StateKind::Idle);
                        if tty.write(&format!("✳ Ready | {label}"))? {
                            static_title = Some((StateKind::Idle, label.to_string()));
                        }
                    } else {
                        transcript_position = transcript_start;
                        transcript_interrupted = false;
                    }
                }
            }

            // Re-probe pending shells: a server that binds late, or a shell
            // killed without a wake, would otherwise stick until the next hook.
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
                            probe::ShellProbe::Endless => {}
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
                        let label = title_label(&current.value);
                        mode = Some(StateKind::Idle);
                        if tty.write(&format!("✳ Ready | {label}"))? {
                            static_title = Some((StateKind::Idle, label.to_string()));
                        }
                    }
                }
            } else {
                shell_watch = None;
            }
        }

        if now.duration_since(last_liveness_check) >= Duration::from_secs(1) {
            // An upgrade replaces the binary at env::current_exe(), giving it
            // a fresh inode; step aside so the next hook fire spawns the new
            // one instead of leaving the tab pinned to old code.
            if executable_at_start.is_some() && executable_signature() != executable_at_start {
                break;
            }
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
                let deadline = Instant::now() + EXIT_CLEAR_PATIENCE;
                while !tty.write("")? && Instant::now() < deadline {
                    thread::sleep(OUTPUT_SETTLE_PATIENCE);
                }
                break;
            }
            last_liveness_check = now;
        }

        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

// Many tabs in one project all read the same project name; when the user has
// run /rename in the Claude Code CLI, that name is what distinguishes them.
fn title_label(state: &State) -> &str {
    state
        .custom_title
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(&state.project)
}

fn executable_signature() -> Option<(u64, u64)> {
    let path = env::current_exe().ok()?;
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

fn process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// Pressing escape with a message queued submits that message before Claude
// Code flushes the abandoned turn, so the interrupt record lands past the
// offset the prompt hook recorded and would read as an interrupt of the new
// turn. The interrupt only stands while it is the newest user record: the
// prompt that follows it, or a later tool result, means the session moved on.
fn transcript_interrupt_state(
    path: Option<&Path>,
    _start: u64,
    position: u64,
    interrupted: bool,
) -> (bool, u64) {
    let Some(path) = path else {
        return (interrupted, position);
    };
    let Ok(mut file) = File::open(path) else {
        return (interrupted, position);
    };
    let length = file.metadata().map_or(0, |metadata| metadata.len());
    let mut position = if position > length { 0 } else { position };
    if file.seek(SeekFrom::Start(position)).is_err() {
        return (interrupted, position);
    }
    let mut reader = BufReader::new(file);
    let mut record = Vec::new();
    let mut interrupted = interrupted;
    loop {
        record.clear();
        let Ok(bytes) = reader.read_until(b'\n', &mut record) else {
            break;
        };
        if bytes == 0 || !record.ends_with(b"\n") {
            break;
        }
        if contains(&record, USER_RECORD) {
            interrupted = contains(&record, INTERRUPT_MARKER);
        }
        position += bytes as u64;
    }
    (interrupted, position)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn set_idle_if_unchanged(path: &Path, observed: &StoredState) -> Result<bool> {
    let _lock = state::lock(path)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    const INTERRUPT_RECORD: &[u8] = br#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
    const PROMPT_RECORD: &[u8] =
        br#"{"type":"user","message":{"role":"user","content":"carry on"}}"#;
    const ASSISTANT_RECORD: &[u8] =
        br#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#;

    fn append(path: &Path, records: &[&[u8]]) {
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

    #[test]
    fn interrupt_record_waits_for_its_newline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        fs::write(&path, INTERRUPT_RECORD).unwrap();
        let (found, position) = transcript_interrupt_state(Some(&path), 0, 0, false);
        assert!(!found);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        let (found, _) = transcript_interrupt_state(Some(&path), 0, position, false);
        assert!(found);
    }

    #[test]
    fn scan_does_not_match_before_prompt_offset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        append(&path, &[INTERRUPT_RECORD]);
        let offset = fs::metadata(&path).unwrap().len();
        append(&path, &[ASSISTANT_RECORD]);
        let (found, _) = transcript_interrupt_state(Some(&path), offset, offset, false);
        assert!(!found);
    }

    #[test]
    fn a_prompt_queued_behind_the_interrupt_resumes_working() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        append(&path, &[PROMPT_RECORD]);
        let offset = fs::metadata(&path).unwrap().len();
        append(&path, &[ASSISTANT_RECORD, INTERRUPT_RECORD, PROMPT_RECORD]);
        let (found, _) = transcript_interrupt_state(Some(&path), offset, offset, false);
        assert!(!found);
    }

    #[test]
    fn an_interrupt_after_the_last_tool_result_stands() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        append(&path, &[PROMPT_RECORD]);
        let offset = fs::metadata(&path).unwrap().len();
        append(&path, &[ASSISTANT_RECORD, PROMPT_RECORD, INTERRUPT_RECORD]);
        let (found, _) = transcript_interrupt_state(Some(&path), offset, offset, false);
        assert!(found);
    }
}
