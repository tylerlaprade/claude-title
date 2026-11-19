# claude-title

Show Claude Code's status in the terminal tab title:

- `⠋ Working | project` while Claude works
- `✳ Ready | project` when Claude finishes
- `⧗ Waiting | project` while Claude waits for background tasks
- `⚠ Action required | project` when Claude needs approval

A name you give the session with `/rename` comes before the status:
`⠋ name | Working | project`.

The spinner animates so a busy tab stands out in the tab bar. Background
shells that run until killed (dev servers, local stacks, log tails) and the
watchers Claude Code keeps on published artifacts do not hold the waiting
title. No configuration.

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
existing Ghostty process and never reopen the app after you quit it. Rex
(control API revision 11 or later) shows the title as the tab's label, set
through the Rex server's socket, so title updates stay out of terminal output
there too. A Rex label is the session's name alone, or the project when the
session has none, because Rex marks the tab's status itself and each label
change costs the Rex app about 40 ms of CPU. Rex draws those marks, a
spinner on a working tab among them, from the program status its terminal
received (OSC 7501).
Claude Code reports that status when its terminal titles are on and Rex
answered its startup query; otherwise claude-title reports it, with one short
write into the terminal per status change: working, blocked while Claude needs
approval, done when a turn finishes, idle after an interrupt. Those writes
broke no line of a 400-line reply during 3,012 of them, but output that keeps
the terminal's queue full, which Claude Code's drawing did not, can be split
by one. A Rex tab split into panes keeps its own label, a name you give the
tab while Claude Code runs there stays, and the tab's earlier label returns
when the session ends. A Rex session that cannot reach its server shows no
title rather than write one into the terminal. Other terminals use OSC 0 with a
best-effort output queue check. Shell probing needs
`lsof` (preinstalled on macOS). A
`docker compose up` stack listens outside the shell's process tree, so it
holds the waiting title.

Hooks serialize state changes per terminal; background subagent hooks do not
overwrite the main session's title, except that a subagent's permission prompt
shows Action required until it is answered. Claude Code reports no event at the
moment you approve a prompt, so the title changes when the approved tool
finishes. Transcript name lookup reads only appended
records after the first scan. Process probes and Ghostty and Rex replies have
bounded waits. The daemon retries a failed connection while its owning session
remains alive, so restoring a session does not require another prompt to
initialize its title. Detached workers with an inherited Ghostty or Rex
environment do not open title connections. Startup and transport errors are retained beside the per-terminal
state in the temporary `claude-title-<uid>` directory as `<tty>.log`.

## License

GPL-3.0-only
