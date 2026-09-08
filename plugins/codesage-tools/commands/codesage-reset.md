---
name: codesage-reset
description: Fully regenerate a project's CodeSage index under its writer lock, with a separate offline recovery procedure for an unreadable database.
argument-hint: "[project-path — defaults to cwd] [--yes]"
---

# Rebuild a CodeSage index

Use a full rebuild after parser upgrades, embedding model or pooling changes, or CPU/GPU device changes. It reparses source and regenerates embeddings without unlinking the live SQLite database. For an ordinary incremental refresh, use `/codesage-reindex`.

## Resolve the project and authority

Resolve `$ARGUMENTS` to an absolute project path, defaulting to the current directory. Require an existing `.codesage` directory and configuration. Run commands from that project directory.

Before touching any `.codesage/` path, reject symlinked directories and symlinked or non-regular leaves. Check with `test -L`, which detects dangling links too. Include the database, its `-wal` and `-shm` companions, `indexing.lock`, configuration, and watcher markers. Use freshly generated backup or temporary names.

An explicit request to reset this project or `--yes` authorizes the ordinary full rebuild; do not ask again. If authority is missing, describe the target and full embedding cost before requesting it. Do not promise a fixed duration.

## Capture the baseline

Run `codesage status --json` and record retained `files` and `chunks`, plus semantic freshness. If status fails, preserve the exact error and classify it before proceeding. An embedding/configuration error does not prove database corruption. Use the offline recovery section only for a confirmed unreadable or incompatible database that cannot be rebuilt in place.

## Rebuild in place

Run:

```sh
codesage index --full --lock-wait 30
```

The command acquires the project's writer lock before opening the database. It waits up to 30 seconds for another writer, including the watcher. Exit 75 means contention prevented the rebuild; report that outcome and retry only after the holder finishes. Never delete the database or lock file to bypass contention.

Keep the same process handle when waiting for a long-running rebuild. Read the structural and semantic failed-file counts as well as the exit status; a zero exit status with failed files is a partial rebuild.

## Verify

Run `codesage status --json` after the rebuild. Report retained files/chunks and their changes from the baseline, semantic freshness, elapsed time, and any failed files. The indexing summary's chunks count is work performed in this pass, not the retained total.

Run one project-relevant semantic query using the absolute project path. Report whether it succeeded and its top result. Investigate unexpected coverage loss; a chunk-count change alone does not prove missing coverage after parser or model changes.

A full rebuild retains other model tables and auxiliary database state. Use `codesage cleanup --dry-run` followed by `codesage cleanup` after a successful rebuild if orphan model tables should be removed.

## Offline recovery for an unreadable database

This is a maintenance operation, not an online reset. Obtain authority for the affected clients and downtime before stopping them; an index reset request does not authorize disrupting other projects hosted by the shared daemon.

1. Establish a maintenance window in which no process can open this project's index. Pause agent sessions, hooks, scheduled indexing, and other automation that can restart clients. Record whether `.codesage/watch.disabled` already exists, then run `codesage watch stop <absolute-project>`. Stop project-specific foreground watchers, direct MCP servers, and standalone CLI readers/writers. Close all affected daemon clients before running `codesage daemon stop`; a new shim can otherwise start the daemon again. A snapshot of open file descriptors is useful evidence, but cannot establish that future openers are prevented.
2. Acquire an exclusive lock on the existing `.codesage/indexing.lock` with a bounded wait, using an OS lock compatible with CodeSage's `flock` on Unix. On Linux, `flock --exclusive --timeout 30 <absolute-project>/.codesage/indexing.lock <maintenance-command>` holds it for the maintenance command. Revalidate directory/leaf types before opening it. Do not unlink or replace the lock file.
3. Inside that locked maintenance command, create a uniquely named private backup directory under `.codesage` with `mktemp -d`. Move `index.db` and every existing `index.db-wal` and `index.db-shm` into that same directory, preserving names. Do not delete any member or move only the main database: committed state may still be in the WAL. If a move fails, stop, report every original and backup location, and keep maintenance active until the set is reconciled.
4. Release the maintenance lock, while keeping all clients and automation quiescent. Run `codesage index --full --lock-wait 30`, which acquires its own writer lock. Do not launch it while a separate process still holds the maintenance lock. Keep the backup until the rebuilt index has passed the status/query checks above. The backup is recovery evidence, not a promise that a corrupt database can be restored to a usable state.
5. Resume clients and automation only after verification. If the watcher was enabled before maintenance, run `codesage watch start <absolute-project>`; otherwise preserve the existing disabled state. Report the backup path and rebuilt totals. If rebuilding fails, retain the backup and maintenance state and report the error; never silently overwrite either database.

If the maintenance precondition cannot be established, leave the database intact and name the clients or automation that prevent recovery. Do not substitute a process scan or writer lock for that precondition.

Rebuild and recovery preserve the project's configuration and global MCP registration.
