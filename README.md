# claude-title

Show Claude Code's status in the terminal tab title:

- `⠋ Working | project` while Claude works
- `✳ Ready | project` when Claude finishes
- `⧗ Waiting | project` while Claude waits for background tasks
- `⚠ Action required | project` when Claude needs approval

The spinner animates so a busy tab stands out in the tab bar. Background
shells that run until killed (dev servers, local stacks, log tails) do not
hold the waiting title. No configuration.

## Install

```sh
# macOS (recommended)
brew install tylerlaprade/tap/claude-title

# or
cargo install claude-title

claude-title install
```

Restart Claude Code. The installer adds hooks pinned to the binary's absolute
path (rerun `claude-title install` if you move it) and sets
`CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1` in `~/.claude/settings.json`.
[Releases](https://github.com/tylerlaprade/claude-title/releases) carry
attested binaries; verify with `gh attestation verify`.

## Uninstall

```sh
claude-title uninstall
brew uninstall claude-title   # if installed with Homebrew
cargo uninstall claude-title  # if installed with Cargo
```

Removes the hooks and restores the previous title setting.

## Support

macOS Ghostty 1.3.1 or later uses its native AppleScript API so title updates
cannot split terminal output. AppleScript support must be enabled in Ghostty;
macOS may request Automation access on first use. Title updates target an
existing Ghostty process and never reopen the app after you quit it. Other
terminals use OSC 0 with a best-effort output queue check. Shell probing needs
`lsof` (preinstalled on macOS). A
`docker compose up` stack listens outside the shell's process tree, so it
holds the waiting title.

Hooks serialize state changes per terminal; background subagent hooks do not
overwrite the main session's title. Transcript name lookup reads only appended
records after the first scan. Process probes and Ghostty replies have bounded
waits. The daemon retries a failed connection while its owning session remains
alive, so restoring a session does not require another prompt to initialize its
title. Detached workers with an inherited Ghostty environment do not open title
connections. Startup and transport errors are retained beside the per-terminal
state in the temporary `claude-title-<uid>` directory as `<tty>.log`.

## License

GPL-3.0-only
