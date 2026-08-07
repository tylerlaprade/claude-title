# claude-title

Shows Claude Code's state in the terminal tab title:

- `⠋ Working | project` while Claude works
- `✳ Ready | project` when Claude has finished
- `⧗ Waiting | project` while Claude waits for background tasks
- `⚠ Action required | project` when Claude needs approval

The moving mark shows which Claude tab is active without opening it.
Background shells that run until killed — dev servers, local stacks, log
tails — never hold the waiting title. No configuration.

## Install

```sh
brew install tylerlaprade/tap/claude-title   # or: cargo install claude-title
claude-title install
```

Restart Claude Code. The installer adds hooks pinned to the binary's absolute
path (rerun `claude-title install` if you move it) and sets
`CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1` in `~/.claude/settings.json`.
[Releases](https://github.com/tylerlaprade/claude-title/releases) carry
attested binaries: verify with `gh attestation verify`.

## Uninstall

```sh
claude-title uninstall
brew uninstall claude-title   # or: cargo uninstall claude-title
```

Removes the hooks and restores the previous title setting.

## Support

macOS and Linux terminals with OSC 0 title support; tested on macOS with
Ghostty. Shell probing needs `lsof` (preinstalled on macOS). Container-backed
stacks listen outside the shell's process tree and count as awaited work.

## License

GPL-3.0-only
