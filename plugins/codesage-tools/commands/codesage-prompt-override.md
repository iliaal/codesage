---
name: codesage-prompt-override
description: Print a system-prompt fragment that steers Claude Code to prefer CodeSage's MCP tools over Grep for retrieval-shape tasks
---

# CodeSage system-prompt override

Print a short system-prompt fragment users can pipe into `claude --append-system-prompt-file -` or paste into `~/.claude/settings.json` (`appendSystemPrompt`). The fragment names each CodeSage MCP tool by the question shape it answers and limits Grep to non-identifier literals.

Pattern adopted from Serena's `cc-system-prompt-override`. Background: two consecutive measurements (2026-04-24 and 2026-04-30, harness at `bench/agent-tool-selection-harness.py`) showed agents pick CodeSage retrieval tools 0/10 even when whitelisted in `--allowedTools`. CLAUDE.md and tool-description hardening did not move the rate. The system-prompt slot is the next lever — the override fragment lands ahead of every user message and is tagged as system-level instruction, which is more salient than CLAUDE.md content.

## What to do

1. Run `${CLAUDE_PLUGIN_ROOT}/bin/codesage-prompt-override` and capture stdout.

2. Show the user the fragment with three integration options:

   - **One-shot**: pipe into a single Claude session.
     ```
     ${CLAUDE_PLUGIN_ROOT}/bin/codesage-prompt-override | claude -p --append-system-prompt-file - 'your task'
     ```

   - **Per-session alias**: shell function that wraps `claude` for the current shell. Suggest adding to `~/.bashrc` / `~/.zshrc`.
     ```bash
     codesage-claude() {
       "${HOME}/.claude/plugins/cache/iliaal/codesage/codesage-tools/bin/codesage-prompt-override" \
         | claude --append-system-prompt-file - "$@"
     }
     ```
     The cache path uses the plugin's actual install location; if `${CLAUDE_PLUGIN_ROOT}` resolves elsewhere on this machine, prefer that path.

   - **Persistent**: append the fragment to `~/.claude/settings.json` under an `appendSystemPrompt` key. This applies to every `claude` invocation. Show the user the diff before writing.

3. Tell the user this is a measurement-driven change, not a default-on behavior. The 2026-04-30 rerun should be re-run after they've used the override for a session or two so we can quantify the delta. The harness lives at `bench/agent-tool-selection-harness.py` and accepts `--append-system-prompt-file <path>`.

## Notes

- The fragment is ~400 tokens. It will reduce prompt-cache reuse slightly (the cache key changes), but only when the system prompt itself is what's being cached. For most agent sessions the user-message content dominates.
- The fragment is intentionally language-neutral. It does not name specific paths or projects — those belong in each project's `.claude/CLAUDE.md`.
- If the user reports that the override didn't move tool-selection behavior in their own sessions, the next escalation is path (c) — hooks that intercept Grep on identifier-shaped patterns. That's a heavier intervention; do not jump to it without measurement.
