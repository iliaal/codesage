# WAL recovery evidence

Keep `cs-sweep-wal-corruption-recovery-cyc` dormant. Its demand gate is a user
report of an unrecoverable SQLite-WAL startup failure; that trigger remains
unobserved. The malformed-header probes below did not reproduce the bead's
assertion that a corrupt WAL necessarily prevents startup. They do not disprove
every possible WAL failure or justify closing the entire concern as a false positive.

## Independent probe, 2026-09-08

Binary: `target/debug/codesage`, SHA256
`8afa964cb4ce3acf43ef38a78e7beddb05946dafa6cfc93b096f06441cea0a88`.

Four separate temporary projects each contained `probe.py` with
`def durable_wal_probe():` and an indented `return 42`. Each ran `codesage init`
and `codesage index --no-semantic --no-features`. A Python SQLite connection
executed `PRAGMA wal_checkpoint(TRUNCATE)` against `.codesage/index.db`, returned
`(0, 0, 0)`, and closed before the probe wrote `.codesage/index.db-wal`.
`CODESAGE_WATCH=0` prevented background watcher activity.

| WAL contents after checkpoint | Bytes | `codesage find-symbol durable_wal_probe --json` |
| --- | ---: | --- |
| No injected WAL, control | 0 | Exit 0; expected symbol at `probe.py:1` |
| `bytes(4096)` | 4,096 | Exit 0; same symbol |
| `bytes.fromhex('377f0682') + bytes(range(256)) * 32` | 8,196 | Exit 0; same symbol |
| `bytes.fromhex('377f0682002de21800001000')` | 12 | Exit 0; same symbol |

All four commands emitted empty stderr. The temporary projects were removed
after the checks; no existing project database was modified.

## Limits and reversal conditions

The symbol was already durable in the main database. None of these WALs held
a valid committed transaction, so retaining the symbol does not demonstrate
recovery of uncheckpointed commits. Valid magic alone does not establish a
valid header or frame checksum. The earlier bead comment's explanation that
the magic-plus-garbage case reached per-frame checksum validation is not proven
by its successful query result.

These probes do not cover valid WALs with damaged committed frames, main-file
corruption, storage I/O errors, disk-full checkpoints, or concurrent writers.
Do not add automatic WAL deletion or quarantine on this evidence: doing so can
discard transactions or race a writer without addressing the actual failure.

Reopen implementation when an affected user supplies a reproducible startup
failure. Preserve a copy of the database and its WAL/SHM files, capture the
SQLite extended error code, and reproduce with the matching binary before
choosing recovery. Establish whether rebuilding the index restores every
affected kind of state; `index --full` alone must not be assumed to reconstruct
everything carried by a WAL.
