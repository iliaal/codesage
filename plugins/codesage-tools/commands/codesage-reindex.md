---
name: codesage-reindex
description: Refresh a project's index incrementally and report retained totals separately from work performed. Clean orphan model tables after successful indexing.
argument-hint: "[project-path — defaults to cwd]"
---

# Refresh a CodeSage index

Resolve `$ARGUMENTS` to an absolute project path, defaulting to the current directory. Require `.codesage/index.db`; otherwise direct the user to `/codesage-onboard`. Run commands from that project directory.

Before touching `.codesage/`, reject symlinked directories and symlinked or non-regular leaves, including configuration, database companions, and `indexing.lock`. Use `test -L` to detect dangling links as well. Do not follow repository-controlled links when capturing state or running maintenance.

## Capture the baseline

Run `codesage status --json`. Record `files`, `chunks`, and `semantic.model`. These are retained index totals, not the amount processed by the next indexing pass. Preserve any status failure; do not substitute zero for an unavailable baseline.

## Index

Run `codesage index --lock-wait 30`. This uses the project's indexing lock, waiting up to 30 seconds for an existing writer. Exit 75 means contention prevented the pass; report it without claiming an index refresh. Do not remove the lock file or database to bypass contention.

Read both the exit status and the structural/semantic summaries. A successful process can still report failed files. Report named failures, syntax-error recovery counts, and whether further work is needed. If the operation takes more than 30 seconds, use the host's background process handle and poll that same handle.

The index summaries describe this pass:

- `Structural: N files (X skipped, Y failed, Z removed), S symbols, R references` records structural work performed. An optional `D parsed with syntax errors` suffix identifies recovered parses, not failed files.
- `Semantic: N files (X skipped, Y failed, Z removed), C chunks` records semantic work performed. **C is chunks created during this pass, not the retained index total.**
- Feature and trust-boundary summaries describe their own work; preserve their failures and warnings too.

## Clean orphan tables after successful indexing

Only after indexing succeeds without failed files, run `codesage cleanup --dry-run`. Its `Active model:` and `Active table:` identify the configured model, `keep:` identifies its table, and `DRY-RUN drop:` identifies orphan tables.

If orphan tables exist, run `codesage cleanup` and report the dropped tables and reported database size change. Otherwise skip real cleanup. Cleanup also acquires the indexing lock; contention leaves tables untouched. Other failures can occur after some or all drops: preserve the error, report observed successful and failed drops, and inspect remaining tables with `codesage cleanup --dry-run` before describing their state.

Do not clean before indexing a newly configured model: cleanup refuses to drop old tables when no active table exists for that model. A model switch may require embedding every file. Switching back to a previously indexed model can reuse its valid table, so do not promise that every model switch rebuilds everything.

## Read retained totals and report

Run `codesage status --json` again after indexing and any cleanup. Use its `files` and `chunks` in the final summary, and calculate chunk delta as **post-status chunks minus pre-status chunks**. On a no-op pass, zero chunks created can coexist with an unchanged nonzero retained total.

Report per-pass files processed/skipped/failed/removed and chunks created separately from retained totals. Include elapsed time, failures, semantic freshness, and cleanup outcome. If cleanup removed old model tables, explain that the total change includes removal of those tables; it is not evidence of missing current-model coverage. Report exact old/new model names only when observed; `semantic.model` reflects current configuration and does not prove the previous model's identity.

If post-status fails, report the total and delta as unknown and retain the error. Never use the indexing summary as a replacement total.

End with `<project>: <N> retained chunks, <M> indexed files, took <time>s`, using post-status totals. For regeneration after parser, device, or embedding settings changes, use `/codesage-reset`.
