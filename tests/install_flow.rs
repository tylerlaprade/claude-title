use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn cli_install_and_uninstall_preserve_prior_configuration() {
    let home = tempfile::tempdir().unwrap();
    let settings = home.path().join(".claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(
        &settings,
        serde_json::to_string_pretty(&json!({
            "env": {
                "CLAUDE_CODE_DISABLE_TERMINAL_TITLE": "1",
                "OTHER": "yes"
            },
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "type": "command",
                        "command": "screenbuddy agent idle"
                    }]
                }]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let first_install = run(home.path(), "install");
    assert!(first_install.contains("Claude Code settings: updated"));
    let second_install = run(home.path(), "install");
    assert!(second_install.contains("Claude Code settings: already set"));

    let installed = read_json(&settings);
    assert_eq!(count_command(&installed, "claude-title hook"), 9);
    assert_eq!(installed["env"]["OTHER"], "yes");
    let receipt = read_json(&home.path().join(".config/claude-title/install.json"));
    assert_eq!(receipt["previous_terminal_title"], "1");

    let first_uninstall = run(home.path(), "uninstall");
    assert!(first_uninstall.contains("Claude Code settings: updated"));
    let removed = read_json(&settings);
    assert_eq!(count_command(&removed, "claude-title hook"), 0);
    assert_eq!(removed["env"]["CLAUDE_CODE_DISABLE_TERMINAL_TITLE"], "1");
    assert_eq!(removed["env"]["OTHER"], "yes");
    assert_eq!(
        removed["hooks"]["Stop"][0]["hooks"][0]["command"],
        "screenbuddy agent idle"
    );
    assert!(
        !home
            .path()
            .join(".config/claude-title/install.json")
            .exists()
    );

    let second_uninstall = run(home.path(), "uninstall");
    assert!(second_uninstall.contains("Claude Code settings: not present"));
}

fn run(home: &Path, command: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_claude-title"))
        .arg(command)
        .env("HOME", home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn count_command(value: &Value, expected: &str) -> usize {
    match value {
        Value::Array(values) => values
            .iter()
            .map(|value| count_command(value, expected))
            .sum(),
        Value::Object(values) => {
            let current = usize::from(
                values
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command == expected),
            );
            current
                + values
                    .values()
                    .map(|value| count_command(value, expected))
                    .sum::<usize>()
        }
        _ => 0,
    }
}
