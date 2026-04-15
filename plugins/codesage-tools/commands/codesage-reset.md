---
name: codesage-reset
description: Drop a project's CodeSage index and rebuild from scratch (use after settings changes, device switches, or corruption)
argument-hint: "[project-path — defaults to cwd] [--yes]"
---

# Reset and rebuild a CodeSage index

Drops `.codesage/index.db` and performs a full re-index. Use after:

- Editing `.codesage/config.toml` in a way that breaks the existing index (model switch, pooling change)
- Switching embedding device (CPU ↔ GPU — embeddings produced on different devices are not bit-identical)
- Index corruption or weird state
- Upgrading CodeSage to a version with incompatible storage

For routine refresh use `/codesage-reindex`. Reset is destructive and pays the full embedding cost.

## Step 1: Resolve and confirm

`$ARGUMENTS` — project path (default cwd). Verify it has `.codesage/index.db`; if not, stop and tell the user to run `/codesage-onboard` first.

Before touching anything, **confirm with the user**: "About to reset CodeSage index for `<project>`. This deletes `.codesage/index.db` and rebuilds from scratch — on CUDA this typically takes under a few minutes for most repos. Proceed?" Wait for explicit yes.

Skip confirmation only if `--yes` is in `$ARGUMENTS`.

## Step 2: Capture pre-reset state

```
cd <project> && codesage status
```

Record chunk count, file count, and language breakdown. This is the baseline for the post-reset diff.

## Step 3: Drop the index

CodeSage has no `reset` subcommand. Delete the DB directly:

```
rm -f <project>/.codesage/index.db
```

Report what was removed.

## Step 4: Re-index

```
cd <project> && codesage index
```

Full rebuild. Background it if it takes more than ~30 seconds; stream the output file and report progress every minute or so.

## Step 5: Verify and report

After index completes:

- `cd <project> && codesage status` and compare against the pre-reset baseline
- Chunk count should be similar; a drop > 20% is a red flag — surface it
- Run one sanity query through the `codesage` MCP with `project: "<absolute path>"` and a generic term from the project ("authentication", "main", "config"). Report top hit and score.
- Report: elapsed time, old vs. new chunk count, sanity-query top hit.

## Notes

- Device switch is the most common reason to reset — re-read `.codesage/config.toml` if the user just toggled `device = "cpu"` ↔ `device = "gpu"`.
- Reset does NOT touch the global `codesage` MCP registration.
- Git hooks installed by `/codesage-onboard` will auto-refresh after normal commits. Reset should only be needed for the scenarios listed above.
- For a lighter path after a model switch (without wiping the whole DB), use `/codesage-reindex` — it detects model mismatch and runs `codesage cleanup` to drop orphan vec tables.
