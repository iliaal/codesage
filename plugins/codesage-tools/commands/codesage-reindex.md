---
name: codesage-reindex
description: Force an incremental codesage re-index (use when git hooks didn't fire). Auto-cleans orphan vec tables if the config model has changed.
argument-hint: "[project-path — defaults to cwd]"
---

# Force incremental CodeSage re-index

Wraps `codesage index` for cases when you want an immediate refresh without waiting for a commit/merge/checkout hook. If the project's `config.toml` embedding model no longer matches the active model in the DB, the script detects this and runs `codesage cleanup` first to drop orphan vec tables from the previous model.

## Step 1: Resolve the target project

`$ARGUMENTS` is the project path. If empty, use the current working directory. Verify it has `.codesage/index.db` — if not, tell the user to run `/codesage-onboard <path>` first and stop.

## Step 2: Detect model mismatch

Run a dry-run cleanup to detect orphan vec tables left behind from a previous model:

```
cd <project> && codesage cleanup --dry-run
```

Output format:

- `Active model:` and `Active table:` header lines reflect what `config.toml` currently says
- One `keep:` line per table that matches the active model
- One `DRY-RUN drop:` line per orphan table from a previous model

If any `DRY-RUN drop:` lines appear, the user switched models in `config.toml` without wiping the DB. Run the real cleanup:

```
cd <project> && codesage cleanup
```

Report which tables were dropped and the DB size reclaimed.

If the dry-run shows only `keep:` lines, skip the cleanup step entirely — no output, no narration.

## Step 3: Pre-count

```
cd <project> && codesage status
```

Capture the chunk count so you can report the delta after reindexing.

## Step 4: Incremental index

```
cd <project> && codesage index
```

Incremental — only changed files get re-processed. On a warm GPU with no changes it's under 5 seconds. On a large delta, minutes.

If the command runs over ~30 seconds, background it and poll.

If the mismatch path ran, the semantic portion will rebuild from scratch for everything (because the cleanup dropped the vec tables). Warn the user up front that this pass is doing real work, not a cheap incremental.

## Step 5: Report

Parse the `codesage index` output and report:

- Files listed / added / deleted / reprocessed / unchanged
- Total chunks after and delta vs. pre-count
- Whether cleanup ran (and which model → which model)
- Any errors

End with one line: `<project>: <N> chunks, <M> files indexed, took <time>s`. If cleanup ran, prefix with `[model switched: old → new]`.

## Notes

- This is a no-op wrapper for `codesage index` plus conditional `codesage cleanup`. The value over raw CLI is structured pre/post reporting and the model-mismatch safety check.
- For a full destructive rebuild, use `/codesage-reset` instead.
- Reindex does NOT touch the global MCP registration.
