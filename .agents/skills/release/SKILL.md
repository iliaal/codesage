---
name: release
description: Cut a CodeSage release for a supplied X.Y.Z version using the repository's prose audit, validation gates, and interactive release script. Use for releasing this CodeSage repository.
---

Read [the release workflow](references/workflow.md) in full and follow it from
the repository root. This tracked workflow is shared by Claude and Codex; do
not maintain a second copy in a local command adapter.

Interpret `$ARGUMENTS` as the version supplied with `$release`, for example
`$release 0.27.0`. It is a prompt argument, not an environment variable. If no
version was supplied, ask for it before starting the release.

Use the available writing skill for the prose audit. Translate Claude tool
references to Codex's native file, shell, and user-input tools, following
`AGENTS.md`. Prefix shell commands with `rtk`; use `rtk proxy` when preserving
gate output or interacting with the release script.

Run the script in a persistent terminal session when confirmation prompts are
needed. Follow the shared workflow's existing-authorization rules and canonical
`sanity-check.sh --cuda` gate. Use `--yes` only when both commit/tag and push are
already authorized, and `--include-approved-prose` only for the approved prose
being included in this release transaction.
