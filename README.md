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
brew install tylerlaprade/tap/claude-title
claude-title install
```

With a Rust toolchain, `cargo install claude-title` works instead, or
`cargo install --path .` from a checkout. Prebuilt binaries are on the
[releases page](https://github.com/tylerlaprade/claude-title/releases), each
verifiable with `gh attestation verify`.

Restart Claude Code afterward. The installer adds its hooks — pinned to the
binary's install path, so rerun `claude-title install` if you move it — and
`CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1` to `~/.claude/settings.json`, touching
nothing else.

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
