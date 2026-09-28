# Agent rules

`README.md` owns the product behavior. Derive title state from observable
behavior, not command-name lists or per-user classification files.

Claude Code hook payloads and event timing can change between installed
versions. Before changing the state machine, inspect the current installed
bundle and run the hook-flow tests; old memory notes are not evidence. Keep
version-specific discoveries in code comments or regression tests beside the
assumption they protect.
