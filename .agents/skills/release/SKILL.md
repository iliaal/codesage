---
name: release
description: Cut a CodeSage release for a supplied X.Y.Z version using the repository's prose audit, validation gates, and interactive release script. Use for releasing this CodeSage repository.
---

Read [the release command](../../../.claude/commands/release.md) in full and
follow its workflow from the repository root. The command file is the shared
workflow for Claude and Codex; do not maintain a second copy here.

Interpret `$ARGUMENTS` as the version supplied with `$release`, for example
`$release 0.27.0`. It is a prompt argument, not an environment variable. If no
version was supplied, ask for it before starting the release.

Use the available writing skill for the prose audit. Translate Claude tool
references to Codex's native file, shell, and user-input tools, following
`AGENTS.md`. Prefix shell commands with `rtk`; use `rtk proxy` when preserving
gate output or interacting with the release script.

Run the interactive script in a persistent terminal session so its confirmation
prompts remain usable. Present each commit/tag or push prompt to the user and
relay their answer; never supply an automatic yes. Follow all four validation
commands listed in step 3 before invoking the script.
