# State-file crash safety

This audit covers the writers tracked by `cs-a3p`. SQLite transactions remain
responsible for indexed data; these files hold coordination, snapshots, and
telemetry outside the database.

| Writer | Failure consequence and disposition | Coordination |
|---|---|---|
| `.codesage/hook-state` (`commands/hooks.rs`) | Keep the short direct stamp write. A missing or partial stamp cannot equal the complete current HEAD-plus-worktree stamp, so the next hook indexes again. A complete matching stamp describes an already successful structural/semantic and git-history pass. Losing the trailing newline is harmless because the shell comparison removes it. | The hook's `hook-index.lock` directory provides single-flight execution; existing symlink and regular-file checks remain. |
| `.codesage/feature-map.state` (`commands/index.rs`) | Keep the direct decimal fingerprint write. Empty/invalid content misses the cache; a proper decimal prefix differs from the full fingerprint being written and forces mapping again. Recording occurs only after successful mapping. | The project indexing lock serializes indexing; existing no-follow write/read guards remain. |
| `.codesage/watch.status` (`statewatcher.rs`) | Replace atomically. A torn JSON record otherwise hides a live watcher or its parked-file count until the next update. Readers now see either complete version. A dead writer's PID is still rejected by the existing liveness check. | The watcher owns its status; replacement does not acquire or replace its lifetime lock. |
| `.codesage/sessions/<id>.json` (`graph/session.rs`) | Replace atomically using a unique temporary. The previous fixed temporary let simultaneous starts for one session truncate or rename each other's writes. Complete snapshots now use last-successful-rename semantics. | Concurrent writes have independent temporary inodes; existing session ID, parent-directory, file-kind, size, and age checks remain. |
| Runtime `brief-<session>.json` (`brief_gate.rs`) | Replace atomically using a unique temporary. Failed writes retain the previous budget record. Malformed existing state still fails closed rather than granting a new budget. State reads also refuse symlinks. | The separate per-session lock inode survives replacement. The budget is persisted before reporting `Served`. |
| Persistent `brief-fires.jsonl` (`brief_gate.rs`) | Keep append semantics and one previous generation. Insert a newline if the old append stopped mid-record, so the next valid fire survives. The scorer skips malformed JSON lines and replaces invalid UTF-8 while reading each generation. | `brief-fires.lock` serializes rotation and append across processes. Append opens refuse symlinks and non-regular files; the existing private-directory requirement remains. |
| `.codesage/drift.log` (`graph/drift.rs`) | Keep append semantics; insert the same missing record boundary. Rotation atomically retains up to 10,000 valid JSON records from the last 8 MiB rather than discarding an oversized log wholesale. A fragment crossing the bounded-read start is discarded. | `drift.lock` serializes rotation and append across processes. No-follow opens and exclusive temporary creation protect the log and rotation. |

`graph::state_file` creates temporary files exclusively in the destination
directory. Replacement preserves an existing regular file's permissions; new
files use mode `0600`. It syncs the completed temporary before rename and, on
Unix, syncs the containing directory afterward. Append writers sync file data.
These steps request durability from the filesystem; they cannot guarantee a
storage device's power-loss behavior. An error after rename can mean the new
complete state is visible even though directory syncing failed.

Ordinary errors remove unpublished temporaries. A killed process can leave a
uniquely named `.codesage-state-*` file; readers ignore it and future writes
never reuse it. Existing final-path symlinks and non-regular files are refused.
Callers retain their ancestor-directory trust checks; these operations do not
establish isolation from a malicious process running as the same user.

Verification covers failed partial replacement, complete concurrent snapshots
and replacements, preserved permissions, symlink refusal, malformed append
tails, valid records after oversized invalid-UTF-8 content, and simultaneous
drift rotation/appends. Direct cache-marker writes retain their existing
recomputation tests and reader contracts.
