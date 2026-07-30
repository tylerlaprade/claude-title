# claude-title

claude-title shows Claude Code's state in the terminal tab title:

- `⠋ Working | project` while Claude works
- `✳ Ready | project` when Claude has finished
- `⚠ Action required | project` when Claude needs approval

The moving mark lets you see which Claude tab is active without opening it.

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

It runs one small daemon per active terminal tab. Claude Code hooks update a
private state file, and the daemon writes the title. The daemon stops when
Claude exits and watches the transcript so pressing Escape restores the ready
title at once.

## License

GPL-3.0-only
