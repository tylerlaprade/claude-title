use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const COMMAND: &str = "claude-title hook";
const OWN_COMMANDS: [&str; 7] = [
    COMMAND,
    "claude-title ensure",
    "claude-title busy",
    "claude-title busy prompt",
    "claude-title idle",
    "claude-title waiting",
    "claude-title end",
];
// Completing one of the matched tools is what resolves its dialog; other tools
// finishing must not clear the waiting state their sibling's open dialog set.
// Matchers rely on Claude Code's exact-name list syntax; characters outside
// [a-zA-Z0-9_|, -] silently switch matching to an unanchored regex.
const HOOKS: [(&str, Option<&str>); 9] = [
    ("SessionStart", Some("startup|resume|clear")),
    ("UserPromptSubmit", None),
    ("PreToolUse", None),
    (
        "PostToolUse",
        Some("AskUserQuestion|EnterPlanMode|ExitPlanMode"),
    ),
    (
        "PostToolUseFailure",
        Some("AskUserQuestion|EnterPlanMode|ExitPlanMode"),
    ),
    ("Stop", None),
    ("StopFailure", None),
    ("Notification", Some("permission_prompt")),
    ("SessionEnd", None),
];

#[derive(Deserialize, Serialize)]
struct InstallReceipt {
    previous_terminal_title: Option<Value>,
}

enum TerminalTitleAction {
    Preserve,
    Restore(Option<Value>),
}

pub fn install_default() -> Result<()> {
    let changed = install_managed(&settings_path()?, &receipt_path()?)?;
    println!("claude-title installed");
    println!(
        "Claude Code settings: {}",
        if changed { "updated" } else { "already set" }
    );
    Ok(())
}

pub fn uninstall_default() -> Result<()> {
    let changed = uninstall_managed(&settings_path()?, &receipt_path()?)?;
    println!("claude-title uninstalled");
    println!(
        "Claude Code settings: {}",
        if changed { "updated" } else { "not present" }
    );
    Ok(())
}

pub fn install(path: &Path) -> Result<bool> {
    let mut root = read_settings(path)?;
    let before = root.clone();
    remove_owned_hooks(&mut root)?;
    let root_object = object_mut(&mut root, "Claude settings root")?;
    let env_value = root_object
        .entry("env".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    object_mut(env_value, "Claude env")?.insert(
        "CLAUDE_CODE_DISABLE_TERMINAL_TITLE".to_string(),
        Value::String("1".to_string()),
    );
    for (event, matcher) in HOOKS {
        add_hook(&mut root, event, matcher)?;
    }
    let changed = root != before;
    if changed {
        write_settings(path, &root)?;
    }
    Ok(changed)
}

pub fn uninstall(path: &Path) -> Result<bool> {
    uninstall_with_title_action(path, TerminalTitleAction::Restore(None))
}

fn install_managed(settings_path: &Path, receipt_path: &Path) -> Result<bool> {
    if read_receipt(receipt_path)?.is_none() {
        let root = read_settings(settings_path)?;
        let receipt = InstallReceipt {
            previous_terminal_title: terminal_title_setting(&root),
        };
        write_receipt(receipt_path, &receipt)?;
    }
    install(settings_path)
}

fn uninstall_managed(settings_path: &Path, receipt_path: &Path) -> Result<bool> {
    let receipt = read_receipt(receipt_path)?;
    let action = receipt.map_or(TerminalTitleAction::Preserve, |receipt| {
        TerminalTitleAction::Restore(receipt.previous_terminal_title)
    });
    let changed = uninstall_with_title_action(settings_path, action)?;
    remove_receipt(receipt_path)?;
    Ok(changed)
}

fn uninstall_with_title_action(path: &Path, title_action: TerminalTitleAction) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut root = read_settings(path)?;
    let before = root.clone();
    remove_owned_hooks(&mut root)?;
    if root
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty)
    {
        root.as_object_mut().unwrap().remove("hooks");
    }
    if let Some(root_object) = root.as_object_mut()
        && let Some(env_value) = root_object.get_mut("env")
        && let Some(env_object) = env_value.as_object_mut()
        && env_object
            .get("CLAUDE_CODE_DISABLE_TERMINAL_TITLE")
            .is_some_and(|value| value == "1")
        && let TerminalTitleAction::Restore(previous_terminal_title) = title_action
    {
        match previous_terminal_title {
            Some(value) => {
                env_object.insert("CLAUDE_CODE_DISABLE_TERMINAL_TITLE".to_string(), value);
            }
            None => {
                env_object.remove("CLAUDE_CODE_DISABLE_TERMINAL_TITLE");
            }
        }
        if env_object.is_empty() {
            root_object.remove("env");
        }
    }
    let changed = root != before;
    if changed {
        write_settings(path, &root)?;
    }
    Ok(changed)
}

fn settings_path() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".claude/settings.json"))
}

fn receipt_path() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("claude-title")
        .join("install.json"))
}

fn terminal_title_setting(root: &Value) -> Option<Value> {
    root.get("env")?
        .get("CLAUDE_CODE_DISABLE_TERMINAL_TITLE")
        .cloned()
}

fn read_receipt(path: &Path) -> Result<Option<InstallReceipt>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))
        .map(Some)
}

fn write_receipt(path: &Path, receipt: &InstallReceipt) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut contents = serde_json::to_vec_pretty(receipt)?;
    contents.push(b'\n');
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(&contents)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn remove_receipt(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to remove {}", path.display()));
        }
    }
    if let Some(parent) = path.parent()
        && parent
            .read_dir()
            .with_context(|| format!("failed to read {}", parent.display()))?
            .next()
            .is_none()
    {
        fs::remove_dir(parent).with_context(|| format!("failed to remove {}", parent.display()))?;
    }
    Ok(())
}

fn read_settings(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
}

// Write through any symlink and replace atomically: a crash mid-write must
// never leave the user's Claude settings truncated.
fn write_settings(path: &Path, root: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut contents = serde_json::to_string_pretty(root)?;
    contents.push('\n');
    let target = settings_target(path)?;
    let mode = match fs::metadata(&target) {
        Ok(metadata) => metadata.permissions().mode(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0o600,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", target.display()));
        }
    };
    let temporary = target.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(mode)
            .open(&temporary)
            .with_context(|| format!("failed to open {}", temporary.display()))?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| format!("failed to secure {}", temporary.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        drop(file);
        fs::rename(&temporary, &target)
            .with_context(|| format!("failed to replace {}", target.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn settings_target(path: &Path) -> Result<PathBuf> {
    let mut target = path.to_path_buf();
    for _ in 0..40 {
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = fs::read_link(&target)
                    .with_context(|| format!("failed to read {}", target.display()))?;
                target = if link.is_absolute() {
                    link
                } else {
                    target.parent().unwrap_or(Path::new(".")).join(link)
                };
            }
            Ok(_) => return Ok(target),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(target),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", target.display()));
            }
        }
    }
    bail!("too many symbolic links in {}", path.display())
}

fn remove_owned_hooks(root: &mut Value) -> Result<()> {
    let Some(root_object) = root.as_object_mut() else {
        bail!("Claude settings root must be an object");
    };
    let Some(hooks_value) = root_object.get_mut("hooks") else {
        return Ok(());
    };
    let hooks_object = object_mut(hooks_value, "Claude hooks")?;
    hooks_object.retain(|_, groups| {
        let Some(groups) = groups.as_array_mut() else {
            return true;
        };
        groups.retain_mut(|group| {
            let Some(group_object) = group.as_object_mut() else {
                return true;
            };
            let Some(commands) = group_object.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            commands.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| OWN_COMMANDS.contains(&command))
            });
            !commands.is_empty()
        });
        !groups.is_empty()
    });
    Ok(())
}

fn add_hook(root: &mut Value, event: &str, matcher: Option<&str>) -> Result<()> {
    let root_object = object_mut(root, "Claude settings root")?;
    let hooks_value = root_object
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_object = object_mut(hooks_value, "Claude hooks")?;
    let groups_value = hooks_object
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(groups) = groups_value.as_array_mut() else {
        bail!("Claude {event} hooks must be an array");
    };
    let mut group = Map::new();
    if let Some(matcher) = matcher {
        group.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    group.insert(
        "hooks".to_string(),
        Value::Array(vec![json!({
            "type": "command",
            "command": COMMAND
        })]),
    );
    groups.push(Value::Object(group));
    Ok(())
}

fn object_mut<'a>(value: &'a mut Value, name: &str) -> Result<&'a mut Map<String, Value>> {
    value
        .as_object_mut()
        .with_context(|| format!("{name} must be an object"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn install_preserves_other_hooks_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(
            &path,
            r#"{
  "env": {
    "OTHER": "yes"
  },
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "screenbuddy agent idle"
          },
          {
            "type": "command",
            "command": "claude-title idle"
          }
        ]
      }
    ]
  }
}
"#,
        )
        .unwrap();
        assert!(install(&path).unwrap());
        assert!(!install(&path).unwrap());
        let root: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(root["env"]["OTHER"], "yes");
        assert_eq!(root["env"]["CLAUDE_CODE_DISABLE_TERMINAL_TITLE"], "1");
        assert_eq!(
            root["hooks"]["Stop"][0]["hooks"][0]["command"],
            "screenbuddy agent idle"
        );
        for (event, matcher) in HOOKS {
            let groups = root["hooks"][event].as_array().unwrap();
            let own = groups
                .iter()
                .find(|group| group["hooks"][0]["command"] == COMMAND)
                .unwrap();
            assert_eq!(own.get("matcher").and_then(Value::as_str), matcher);
        }
    }

    #[test]
    fn uninstall_removes_only_owned_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        install(&path).unwrap();
        let mut root: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        root["env"]["OTHER"] = Value::String("yes".to_string());
        root["hooks"]["Stop"].as_array_mut().unwrap().push(json!({
            "hooks": [{
                "type": "command",
                "command": "session-guard register"
            }]
        }));
        write_settings(&path, &root).unwrap();
        assert!(uninstall(&path).unwrap());
        assert!(!uninstall(&path).unwrap());
        let root: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(root["env"]["OTHER"], "yes");
        assert!(
            root["env"]
                .get("CLAUDE_CODE_DISABLE_TERMINAL_TITLE")
                .is_none()
        );
        assert_eq!(
            root["hooks"]["Stop"][0]["hooks"][0]["command"],
            "session-guard register"
        );
    }

    #[test]
    fn install_preserves_existing_key_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(
            &path,
            r#"{
  "z": 1,
  "env": {
    "OTHER": "yes"
  },
  "hooks": {},
  "a": 2
}
"#,
        )
        .unwrap();
        install(&path).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        let z = contents.find("\"z\"").unwrap();
        let env = contents.find("\"env\"").unwrap();
        let hooks = contents.find("\"hooks\"").unwrap();
        let a = contents.find("\"a\"").unwrap();
        assert!(z < env && env < hooks && hooks < a);
    }

    #[test]
    fn install_keeps_a_settings_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.json");
        let path = directory.path().join("settings.json");
        fs::write(&target, "{}\n").unwrap();
        symlink(&target, &path).unwrap();
        install(&path).unwrap();
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let root: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
        assert_eq!(root["env"]["CLAUDE_CODE_DISABLE_TERMINAL_TITLE"], "1");
    }

    #[test]
    fn install_keeps_a_dangling_settings_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.json");
        let path = directory.path().join("settings.json");
        symlink("target.json", &path).unwrap();
        install(&path).unwrap();
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let root: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
        assert_eq!(root["env"]["CLAUDE_CODE_DISABLE_TERMINAL_TITLE"], "1");
    }

    #[test]
    fn install_preserves_settings_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, "{}\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        install(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn managed_uninstall_restores_an_absent_setting() {
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("settings.json");
        let receipt = directory.path().join("config/install.json");
        fs::write(&settings, r#"{"env":{"OTHER":"yes"}}"#).unwrap();
        assert!(install_managed(&settings, &receipt).unwrap());
        assert!(!install_managed(&settings, &receipt).unwrap());
        assert!(receipt.exists());
        assert!(uninstall_managed(&settings, &receipt).unwrap());
        let root = read_settings(&settings).unwrap();
        assert_eq!(root["env"]["OTHER"], "yes");
        assert!(
            root["env"]
                .get("CLAUDE_CODE_DISABLE_TERMINAL_TITLE")
                .is_none()
        );
        assert!(!receipt.exists());
        assert!(!receipt.parent().unwrap().exists());
    }

    #[test]
    fn managed_uninstall_restores_an_existing_setting() {
        for previous in ["0", "1"] {
            let directory = tempfile::tempdir().unwrap();
            let settings = directory.path().join("settings.json");
            let receipt = directory.path().join("config/install.json");
            fs::write(
                &settings,
                format!(r#"{{"env":{{"CLAUDE_CODE_DISABLE_TERMINAL_TITLE":"{previous}"}}}}"#),
            )
            .unwrap();
            install_managed(&settings, &receipt).unwrap();
            assert_eq!(
                read_settings(&settings).unwrap()["env"]["CLAUDE_CODE_DISABLE_TERMINAL_TITLE"],
                "1"
            );
            uninstall_managed(&settings, &receipt).unwrap();
            assert_eq!(
                read_settings(&settings).unwrap()["env"]["CLAUDE_CODE_DISABLE_TERMINAL_TITLE"],
                previous
            );
        }
    }

    #[test]
    fn managed_uninstall_preserves_a_later_user_change() {
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("settings.json");
        let receipt = directory.path().join("config/install.json");
        install_managed(&settings, &receipt).unwrap();
        let mut root = read_settings(&settings).unwrap();
        root["env"]["CLAUDE_CODE_DISABLE_TERMINAL_TITLE"] = Value::String("0".to_string());
        write_settings(&settings, &root).unwrap();
        uninstall_managed(&settings, &receipt).unwrap();
        assert_eq!(
            read_settings(&settings).unwrap()["env"]["CLAUDE_CODE_DISABLE_TERMINAL_TITLE"],
            "0"
        );
    }
}
