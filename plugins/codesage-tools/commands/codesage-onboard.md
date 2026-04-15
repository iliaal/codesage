---
name: codesage-onboard
description: Onboard a project to CodeSage (global MCP registration, init, index, git hooks, agent hint)
argument-hint: "<project-path> [--device gpu|cpu] [--codesage-bin PATH] [--no-mcp] [--no-hooks] [--no-hint]"
---

# Onboard a project to CodeSage

Wraps `${CLAUDE_PLUGIN_ROOT}/bin/codesage-onboard` and reports results.

Unlike plugins that register one MCP server per project, CodeSage uses **one global MCP** (`codesage`) that routes every call by the absolute `project` path argument. The first onboard registers that MCP; every subsequent onboard on a different project reuses it.

## Step 1: Validate arguments

Arguments are in `$ARGUMENTS`. The first positional arg must be a path that exists. If it doesn't look like a directory, stop and ask the user what to do.

Default device is `gpu`. The binary errors out loudly if CUDA is requested but unavailable, so don't try to be clever — just pass the user's flag through.

## Step 2: Run the onboarding script

Invoke via Bash:

```
${CLAUDE_PLUGIN_ROOT}/bin/codesage-onboard $ARGUMENTS
```

The script runs six steps: global MCP registration (idempotent), `codesage init`, set device in `config.toml`, `codesage index`, install git hooks (if repo), write `.claude/CLAUDE.md` hint (if missing).

Indexing on CUDA is fast (seconds to a few minutes for most repos). On CPU it's 5-10× slower. If the script looks like it will run longer than ~2 minutes, use `run_in_background: true` and poll the output file.

## Step 3: Parse and report

Report what the script actually did, not what it planned:

- Whether the global `codesage` MCP was newly registered or already present
- Number of chunks indexed and language breakdown from `codesage index` output
- Whether git hooks were installed
- Whether `.claude/CLAUDE.md` was written or already existed
- Any CUDA-fallback warnings from the script output

## Step 4: Verify MCP connectivity

After the script completes, run:

```
claude mcp list
```

Confirm `codesage` appears as `✓ Connected`. If it's `✗ Failed to connect`, the most common causes are (a) the binary path in the registration is wrong, (b) the binary can't find CUDA libs at startup. Report the exact failure verbatim.

## Step 5: Sanity query through the MCP

Prove the routing works end-to-end. Call the `codesage` MCP `search` tool with:

```
project: "<absolute project path>"
query: "<generic term from the project>"
limit: 3
```

Pick a generic query fitting the repo (e.g., "authentication" for a backend, "parser" for a compiler, "migration" for a database-heavy project). Report the top result and its score. A score above 0.5 on a reasonable query is healthy; below 0.4 suggests the index is small or the query missed.

If the MCP call returns an error like "project is not onboarded", re-read the script output — onboarding probably failed partway through.

## Step 6: Summarize for the user

End with a short status block:

- Project: `<path>`
- MCP: `codesage` (connected/failed) — newly registered / already present
- Index: `<N>` chunks, `<langs>`
- Hooks: installed / skipped / N/A
- Hint: written / existed / skipped
- Sanity query: top hit `<file>` @ `<score>`

Then a one-line reminder:

> This project is now available through the `codesage` MCP. Pass `project: "<absolute path>"` on every tool call.

## Notes

- The script is idempotent — re-running on the same project is safe and cheap.
- The global `codesage` MCP registration is added on first onboard and reused thereafter; don't create per-project MCP entries.
- If the user passes `--no-mcp` or `--no-hooks`, skip the matching verification step.
