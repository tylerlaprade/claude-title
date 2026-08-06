# claude-title

claude-title shows Claude Code's state in the terminal tab title:

- `⠋ Working | project` while Claude works
- `✳ Ready | project` when Claude has finished
- `⧗ Waiting | project` while Claude waits for background tasks
- `⚠ Action required | project` when Claude needs approval

The moving mark lets you see which Claude tab is active without opening it.
Background shells that run until killed rather than to completion — dev
servers, local stacks, log tails — never hold the waiting title: claude-title
watches what actually runs and listens, so there is nothing to configure.

## Install

```sh
cargo install claude-title
claude-title install
```

Restart Claude Code afterward, and keep the `claude-title` binary in `PATH`.
The installer adds its hooks and `CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1` to
`~/.claude/settings.json`, touching nothing else.

From a checkout, replace the first command with `cargo install --path .`.

## Uninstall

```sh
claude-title uninstall
cargo uninstall claude-title
```

This removes the hooks and restores your previous title setting.

## Support

macOS and Linux terminals that speak the standard OSC 0 title sequence;
built and tested on macOS with Ghostty. Shell probing needs `lsof`, which
macOS ships. Containers listen outside the shell's process tree, so a
`docker compose up` stack counts as awaited work.

## License

GPL-3.0-only
