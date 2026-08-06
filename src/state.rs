use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StateKind {
    Busy,
    Idle,
    Pending,
    Waiting,
    End,
    // A daemon can outlive the hook binary that spawned it, so a kind written
    // by a newer hook must parse instead of blinding the daemon to the file.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct State {
    pub kind: StateKind,
    pub epoch: u64,
    pub claude_pid: u32,
    pub project: String,
    pub transcript_path: Option<PathBuf>,
    pub transcript_offset: u64,
    // Only meaningful while kind is Pending; the daemon re-probes these
    // shells and drops Pending when none remain.
    #[serde(default)]
    pub pending_session: String,
    #[serde(default)]
    pub pending_shells: Vec<String>,
    #[serde(default)]
    pub pending_beyond_shells: bool,
}

pub struct StoredState {
    pub raw: Vec<u8>,
    pub value: State,
}

pub struct StatePaths {
    pub state: PathBuf,
    pub lock: PathBuf,
}

pub fn paths_for_tty(tty: &Path) -> Result<StatePaths> {
    let directory = state_directory()?;
    let identifier = tty
        .strip_prefix("/dev")
        .unwrap_or(tty)
        .to_string_lossy()
        .trim_matches('/')
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if identifier.is_empty() {
        bail!("terminal path has no usable name");
    }
    Ok(StatePaths {
        state: directory.join(format!("{identifier}.json")),
        lock: directory.join(format!("{identifier}.lock")),
    })
}

pub fn read(path: &Path) -> Result<Option<StoredState>> {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let value = match serde_json::from_slice(&raw) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    Ok(Some(StoredState { raw, value }))
}

pub fn write(path: &Path, value: &State) -> Result<()> {
    let mut contents = serde_json::to_vec(value)?;
    contents.push(b'\n');
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("failed to open {}", temporary.display()))?;
        file.write_all(&contents)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[must_use]
pub fn epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn state_directory() -> Result<PathBuf> {
    let directory = env::temp_dir().join(format!("claude-title-{}", unsafe { libc::getuid() }));
    fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let metadata = fs::symlink_metadata(&directory)
        .with_context(|| format!("failed to inspect {}", directory.display()))?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::getuid() } {
        bail!("{} is not a private user directory", directory.display());
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(&directory, Permissions::from_mode(0o700))
            .with_context(|| format!("failed to secure {}", directory.display()))?;
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_kind_still_parses() {
        let value: State = serde_json::from_str(
            r#"{"kind":"someday","epoch":1,"claude_pid":2,"project":"p","transcript_path":null,"transcript_offset":0}"#,
        )
        .unwrap();
        assert_eq!(value.kind, StateKind::Unknown);
    }
}
