use crate::state::{State, StateKind};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) enum TerminalTitle {
    // Nothing but Claude Code may write into the terminal it draws in: a
    // title written beside its output can land inside a character or an
    // escape sequence. A hook hands Claude Code the title and it writes the
    // title itself, in order with everything else it draws.
    ThroughClaude(PathBuf),
    #[cfg(target_os = "macos")]
    Ghostty(crate::ghostty::Connection),
    Rex(Box<crate::rex::Connection>),
}

enum Route {
    ThroughClaude,
    Ghostty,
    Rex,
}

fn route() -> Route {
    match std::env::var("TERM_PROGRAM").as_deref() {
        Ok("ghostty") if cfg!(target_os = "macos") => Route::Ghostty,
        Ok("rex") => Route::Rex,
        _ => Route::ThroughClaude,
    }
}

pub(crate) fn written_by_claude() -> bool {
    matches!(route(), Route::ThroughClaude)
}

// Claude Code watches this file once SessionStart names it, and runs the
// FileChanged hook when the daemon changes it.
pub(crate) fn nudge_path(state: &Path) -> PathBuf {
    state.with_extension("title")
}

pub(crate) fn title_sequence(title: &str) -> String {
    format!("\u{1b}]0;{}\u{7}", clean_title(title))
}

#[derive(Debug)]
pub(crate) struct TerminalDetached;

impl std::fmt::Display for TerminalDetached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session no longer belongs to its terminal")
    }
}

impl std::error::Error for TerminalDetached {}

impl TerminalTitle {
    pub(crate) fn open(tty: &Path, state: &Path, owner: u32) -> Result<Self> {
        match route() {
            Route::ThroughClaude => Ok(Self::ThroughClaude(nudge_path(state))),
            Route::Ghostty => {
                let ghostty =
                    crate::probe::terminal_ancestor(owner, "ghostty")?.ok_or(TerminalDetached)?;
                #[cfg(target_os = "macos")]
                return crate::ghostty::Connection::open(tty, ghostty).map(Self::Ghostty);
                #[cfg(not(target_os = "macos"))]
                unreachable!("{tty:?} {ghostty:?}: Ghostty is driven only on macOS")
            }
            Route::Rex => crate::rex::Connection::open(owner, &state.with_extension("rex.json"))
                .map(|connection| Self::Rex(Box::new(connection))),
        }
    }

    // Rex marks a tab itself from the program status Claude Code reported: a
    // spinner while working, a symbol while blocked, a dot when done.
    pub(crate) fn marks_status(&self) -> bool {
        match self {
            Self::ThroughClaude(_) => false,
            #[cfg(target_os = "macos")]
            Self::Ghostty(_) => false,
            Self::Rex(connection) => connection.claude_reports(),
        }
    }

    // Every Rex label change costs the Rex app about 40 ms of CPU, and a title
    // Claude Code writes costs a hook run, so only Ghostty's title steps.
    pub(crate) fn animates_title(&self) -> bool {
        match self {
            Self::ThroughClaude(_) | Self::Rex(_) => false,
            #[cfg(target_os = "macos")]
            Self::Ghostty(_) => true,
        }
    }

    pub(crate) fn poll(&mut self) -> Result<()> {
        match self {
            Self::ThroughClaude(_) => Ok(()),
            #[cfg(target_os = "macos")]
            Self::Ghostty(_) => Ok(()),
            Self::Rex(connection) => connection.poll(),
        }
    }

    // The hook that changed the state already handed Claude Code its title.
    pub(crate) fn write(&mut self, title: &str) -> Result<()> {
        let title = clean_title(title);
        match self {
            Self::ThroughClaude(_) => Ok(()),
            #[cfg(target_os = "macos")]
            Self::Ghostty(connection) => connection.write(&title),
            Self::Rex(connection) => connection.write(&title),
        }
    }

    // A change the daemon found itself, such as an interrupt, has no hook
    // behind it.
    pub(crate) fn write_between_hooks(&mut self, title: &str) -> Result<()> {
        match self {
            Self::ThroughClaude(nudge) => {
                let moment = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                fs::write(&*nudge, moment.to_string())
                    .with_context(|| format!("failed to write {}", nudge.display()))
            }
            #[cfg(target_os = "macos")]
            Self::Ghostty(_) => self.write(title),
            Self::Rex(_) => self.write(title),
        }
    }
}

// A name the user assigned with /rename outranks the status; the project is
// context and trails. Claude Code 2.1.296 also writes a generated custom-title
// when forking a conversation, and the record carries no source, so a fork's
// generated name reads as assigned.
pub(crate) fn title_text(
    status_marked: bool,
    state: &State,
    indicator: Option<&str>,
    status: &str,
) -> String {
    let project = &state.project;
    let name = state
        .custom_title
        .as_deref()
        .filter(|name| !name.is_empty());
    if status_marked {
        return name.unwrap_or(project).to_owned();
    }
    let fields = match name {
        Some(name) => format!("{name} | {status} | {project}"),
        None => format!("{status} | {project}"),
    };
    match indicator {
        Some(indicator) => format!("{indicator} {fields}"),
        None => fields,
    }
}

pub(crate) fn state_title(status_marked: bool, state: &State, frame: Option<&str>) -> String {
    match state.kind {
        StateKind::Busy => title_text(status_marked, state, frame, "Working"),
        StateKind::Idle | StateKind::Unknown => {
            title_text(status_marked, state, Some("✳"), "Ready")
        }
        StateKind::Pending => title_text(status_marked, state, Some("⧗"), "Waiting"),
        StateKind::Waiting => title_text(status_marked, state, Some("⚠"), "Action required"),
        StateKind::End => String::new(),
    }
}

fn clean_title(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character >= ' ' && *character != '\u{7f}')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_removes_control_characters() {
        assert_eq!(clean_title("one\n\ttwo\u{7f} ✳"), "onetwo ✳");
    }

    fn session(custom_title: Option<&str>) -> State {
        State {
            kind: StateKind::Busy,
            activity: StateKind::Busy,
            epoch: 0,
            claude_pid: 1,
            project: "app".to_owned(),
            custom_title: custom_title.map(str::to_owned),
            title_scan: crate::session_title::SessionTitle::default(),
            transcript_path: None,
            transcript_offset: 0,
            dialogs: Vec::new(),
            resolved_dialog: false,
            pending_session: String::new(),
            pending_shells: Vec::new(),
            pending_beyond_shells: false,
        }
    }

    #[test]
    fn a_terminal_that_marks_status_is_titled_with_the_name_alone() {
        assert_eq!(state_title(true, &session(Some("Fix")), None), "Fix");
        assert_eq!(state_title(true, &session(None), None), "app");
        assert_eq!(
            state_title(false, &session(Some("Fix")), Some("⠋")),
            "⠋ Fix | Working | app"
        );
        assert_eq!(state_title(false, &session(None), None), "Working | app");
    }

    #[test]
    fn a_title_for_claude_to_write_is_one_whole_sequence() {
        assert_eq!(
            title_sequence("✳ Ready\u{1b}]0;x\u{7} | app"),
            "\u{1b}]0;✳ Ready]0;x | app\u{7}"
        );
    }
}
