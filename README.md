# claude-title

claude-title shows Claude Code's state in the terminal tab title:

- `⠋ Working | project` while Claude works
- `✳ Ready | project` when Claude has finished
- `⧗ Waiting | project` while Claude waits for background tasks
- `⚠ Action required | project` when Claude needs approval

The moving mark lets you see which Claude tab is active without opening it.

`⧗ Waiting` appears when Claude ends its turn while background work that will
wake it — shells, subagents, workflows, monitors, MCP tasks — is still
running. A background shell that serves rather than finishes (a dev server, a
local stack) never wakes the session, so it must not count: claude-title
finds each shell's processes through the task output file they hold open and
leaves out any tree with a listening TCP socket. While the waiting title
shows, the daemon keeps probing, so a server that binds its port late, or a
shell killed from the task list, still returns the tab to ready by itself.

## Install

```sh
cargo install claude-title
claude-title install
```

Restart Claude Code after installation. The `claude-title` binary must remain
in `PATH`.

The installer adds nine Claude Code hooks and sets
`CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1` in `~/.claude/settings.json`. It keeps
all unrelated settings and hooks. It records the previous title setting in
`~/.config/claude-title/install.json`.

To install from a checkout before the crate is published:

```sh
cargo install --path .
claude-title install
```

## Uninstall

```sh
claude-title uninstall
cargo uninstall claude-title
```

The first command removes the hooks and restores the title setting that was
present before installation.

## Support

claude-title targets macOS and Linux terminals that support the standard OSC 0
title sequence. It is built and tested on macOS with Ghostty.

Shell probing needs `lsof`, which macOS ships; install it on Linux. Without
it, shells hold the waiting title until the next event. Two known limits: a
container-backed stack (`docker compose up`) listens outside the shell's
process tree, so it reads as awaited work, and a command that detaches from
its own output with a top-level `exec` redirect reads as ended.

It runs one small daemon per active terminal tab. Claude Code hooks update a
private state file, and the daemon writes the title. The daemon stops when
Claude exits and watches the transcript so pressing Escape restores the ready
title at once.

## License

GPL-3.0-only
