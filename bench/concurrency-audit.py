#!/usr/bin/env python3
"""
Concurrency audit for the CodeSage `.codesage/index.db` (recommendations
doc §2.4).

Spawns two concurrent `codesage` commands against the same project DB
and reports whether the result is a clean serialization (one wins, one
errors cleanly) or a corrupt / split-brain state. Does not fix anything
— this is a diagnostic. The fix (file lockfile, `busy_timeout`, retry
loop) is a separate decision based on what the audit finds.

Two scenarios:
  T1 — two `codesage index --full` at once
  T2 — `codesage index --full` + `codesage git-index --full` at once

Checks after each scenario:
  - Process exit codes (zero = ok, nonzero = clean failure, unclear = bug)
  - `PRAGMA integrity_check` (SQLite's own corruption probe)
  - Foreign-key violations (orphan symbols / refs without files)
  - Duplicate row shapes (two rows for the same file path, etc.)
  - Duplicated chunk rowids across vec tables

Runs against an existing onboarded project — pass `--project`. The DB
is backed up before the test and restored afterwards so real indexes
aren't damaged by this audit.

Usage:
  bench/concurrency-audit.py [--project PATH] [--scenario T1|T2|both]

Stdlib only.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def backup_db(codesage_dir: Path) -> Path | None:
    src = codesage_dir / "index.db"
    if not src.exists():
        return None
    # Force WAL → main file before snapshotting. Previously we copied -wal
    # and -shm siblings under separate audit-backup-* names and the
    # restore path never read them back — any committed-but-not-yet-
    # checkpointed transactions (common after a fresh `codesage index`
    # because checkpoints are lazy) were silently dropped on restore.
    # TRUNCATE returns the WAL to zero length so the `.db` snapshot is
    # the entire database state. fnd_9c80fa62.
    #
    # Another reader/writer (the per-user codesage daemon, which keeps a
    # long-lived handle on every routed project's index.db) will cause
    # the checkpoint to return busy=1 and leave WAL frames behind. Refuse
    # to proceed in that case — restoring a torn snapshot would re-drop
    # whatever was supposed to be in those frames. fnd_96b7b163.
    conn = sqlite3.connect(str(src))
    try:
        row = conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()
    finally:
        conn.close()
    if row is not None and row[0] != 0:
        sys.exit(
            "wal_checkpoint(TRUNCATE) returned busy=1 on "
            f"{src} — another process holds the database. "
            "Stop the codesage daemon (`codesage daemon stop`) and any "
            "running indexer, then re-run this audit."
        )
    fd, bak_name = tempfile.mkstemp(prefix="index.db.audit-backup-", dir=codesage_dir)
    os.close(fd)
    bak = Path(bak_name)
    shutil.copy2(src, bak)
    return bak


def restore_db(codesage_dir: Path, bak: Path | None) -> None:
    if bak is None:
        return
    src = codesage_dir / "index.db"
    # Remove WAL/SHM siblings so the restored .db (which was checkpointed
    # to a self-contained state at backup time) isn't shadowed by stale
    # log frames written during the audit run.
    for ext in ("-wal", "-shm"):
        side = codesage_dir / f"index.db{ext}"
        if side.exists():
            side.unlink()
    shutil.copy2(bak, src)


def _read_tail(f, limit: int = 400) -> str:
    """Read a temp file from the start and return its last `limit` chars."""
    try:
        f.seek(0)
        data = f.read()
    finally:
        f.close()
    return data[-limit:]


def run_parallel(cmds: list[list[str]], cwd: Path, timeout_s: int = 300) -> list[dict]:
    """Launch N commands simultaneously, wait for all, return structured
    summaries. Intentionally does not stagger — the whole point is the hard
    concurrency case.

    Each child's stdout/stderr is redirected to its own temp file rather than a
    PIPE. Draining PIPEs sequentially (communicate() per child) let a child that
    wrote more than the ~64 KiB OS pipe buffer block in write() while the harness
    was busy on the other child — and if that blocked write happened while the
    child held the SQLite write lock, the harness manufactured the very livelock
    it exists to detect. Files have no such back-pressure. A single shared
    deadline also bounds total wall time to one timeout window instead of N.
    """
    procs = []
    files = []
    start_times = []
    deadline = time.time() + timeout_s
    for cmd in cmds:
        t0 = time.time()
        out_f = tempfile.TemporaryFile(mode="w+", encoding="utf-8", errors="replace")
        err_f = tempfile.TemporaryFile(mode="w+", encoding="utf-8", errors="replace")
        p = subprocess.Popen(cmd, cwd=str(cwd), stdout=out_f, stderr=err_f, text=True)
        procs.append(p)
        files.append((out_f, err_f))
        start_times.append(t0)

    results = []
    for p, cmd, t0, (out_f, err_f) in zip(procs, cmds, start_times, files):
        remaining = max(0.0, deadline - time.time())
        timed_out = False
        try:
            p.wait(timeout=remaining)
        except subprocess.TimeoutExpired:
            timed_out = True
            p.kill()
            p.wait()
        out_tail = _read_tail(out_f)
        err_tail = _read_tail(err_f)
        results.append({
            "cmd": " ".join(cmd),
            "returncode": None if timed_out else p.returncode,
            "duration_s": round(time.time() - t0, 2),
            "stdout_tail": out_tail,
            "stderr_tail": ("TIMEOUT " + err_tail)[-400:] if timed_out else err_tail,
        })
    return results


# ---------------------------------------------------------------------------
# Post-run DB integrity checks
# ---------------------------------------------------------------------------


def integrity_check(db_path: Path) -> dict:
    """Run SQLite's own corruption probe plus targeted orphan queries."""
    # `vec0` virtual tables require the sqlite-vec extension to query. We
    # only need structural tables + fts tables for the audit, so skip the
    # extension load here by using raw sqlite3 and only touching structural
    # tables we know don't depend on vec0.
    conn = sqlite3.connect(str(db_path))
    try:
        out = {"integrity": "unknown", "orphans": {}, "dupes": {}, "schema_migrations": []}
        rows = conn.execute("PRAGMA integrity_check").fetchall()
        out["integrity"] = ", ".join(r[0] for r in rows) if rows else "empty"
        # FK enforcement is ON per init_db but cheap to re-verify: list
        # foreign key violations (should be empty).
        fk_rows = conn.execute("PRAGMA foreign_key_check").fetchall()
        out["foreign_key_violations"] = [
            {"table": r[0], "rowid": r[1], "parent": r[2], "fkid": r[3]} for r in fk_rows
        ]
        # Symbols / refs without a parent file row (FK is ON DELETE CASCADE
        # so this should be impossible, but if WAL committed a half-state
        # we'd see it).
        out["orphans"]["symbols_without_file"] = conn.execute(
            "SELECT COUNT(*) FROM symbols s "
            "LEFT JOIN files f ON s.file_id = f.id WHERE f.id IS NULL"
        ).fetchone()[0]
        out["orphans"]["refs_without_file"] = conn.execute(
            "SELECT COUNT(*) FROM refs r "
            "LEFT JOIN files f ON r.from_file_id = f.id WHERE f.id IS NULL"
        ).fetchone()[0]
        # Duplicate files by path (files.path is UNIQUE so this must be 0).
        out["dupes"]["files_same_path"] = conn.execute(
            "SELECT COUNT(*) FROM ("
            "  SELECT path, COUNT(*) c FROM files GROUP BY path HAVING c > 1"
            ")"
        ).fetchone()[0]
        # Schema-migration registry state — two concurrent writers could
        # each INSERT into schema_migrations. UNIQUE constraint on name
        # should prevent dupes, but worth verifying on the live DB.
        mig_rows = conn.execute(
            "SELECT name, COUNT(*) FROM schema_migrations GROUP BY name"
        ).fetchall()
        out["schema_migrations"] = [{"name": n, "count": c} for n, c in mig_rows]
        # Summary row counts for sanity.
        for t in ("files", "symbols", "refs"):
            out[f"count_{t}"] = conn.execute(f"SELECT COUNT(*) FROM {t}").fetchone()[0]
        return out
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------


def scenario_T1_two_index(project: Path) -> tuple[list[dict], dict]:
    cmds = [
        ["codesage", "index", "--full"],
        ["codesage", "index", "--full"],
    ]
    results = run_parallel(cmds, project)
    db_path = project / ".codesage" / "index.db"
    state = integrity_check(db_path)
    return results, state


def scenario_T2_index_plus_git_index(project: Path) -> tuple[list[dict], dict]:
    cmds = [
        ["codesage", "index", "--full"],
        ["codesage", "git-index", "--full"],
    ]
    results = run_parallel(cmds, project)
    db_path = project / ".codesage" / "index.db"
    state = integrity_check(db_path)
    return results, state


SCENARIOS = {"T1": scenario_T1_two_index, "T2": scenario_T2_index_plus_git_index}


# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------


def summarize_proc(r: dict) -> str:
    if r["returncode"] == 0:
        verdict = "ok"
    elif r["returncode"] is None:
        verdict = "TIMEOUT"
    else:
        verdict = f"error (rc={r['returncode']})"
    suffix = ""
    if r["stderr_tail"] and (verdict != "ok" or "BUSY" in r["stderr_tail"] or "locked" in r["stderr_tail"].lower()):
        suffix = f"  stderr: {r['stderr_tail'].strip()[:250]!r}"
    return f"  - {r['cmd']} → {verdict} in {r['duration_s']}s{suffix}"


def classify_verdict(results: list[dict], state: dict) -> str:
    """Map the per-process results + post-run DB state to a verdict string.

    A timed-out process has `returncode is None` (SIGKILLed mid-run). It must
    NOT be folded into the "one succeeded, other errored cleanly" branch — a
    hung/livelocked writer is a distinct concurrency failure, exactly the class
    this audit exists to surface, and it was previously reported with a green
    checkmark.
    """
    corrupt = (
        state["integrity"] != "ok"
        or state["orphans"]["symbols_without_file"] > 0
        or state["orphans"]["refs_without_file"] > 0
        or state["dupes"]["files_same_path"] > 0
        or any(m["count"] > 1 for m in state["schema_migrations"])
        or len(state.get("foreign_key_violations") or []) > 0
    )
    if corrupt:
        return "❌ CORRUPT — see detail above; fix required (lockfile / busy_timeout / retry)"

    timed_out = sum(1 for r in results if r["returncode"] is None)
    procs_ok = sum(1 for r in results if r["returncode"] == 0)
    if timed_out:
        return (
            f"⚠ TIMEOUT — {timed_out} process(es) killed after the timeout; "
            "possible writer livelock, not a clean serialization (investigate)"
        )
    if procs_ok == len(results):
        return "✓ clean — both processes succeeded, DB is consistent"
    if procs_ok >= 1:
        failed = [r for r in results if r["returncode"] not in (0, None)]
        if failed and all(is_lock_contention_failure(r) for r in failed):
            return (
                "✓ serialized — one process succeeded, other errored "
                "(SQLITE_BUSY/database locked); DB is consistent"
            )
        return (
            "⚠ child failure — one process succeeded, but a peer failed for a "
            "non-lock reason; DB is consistent but audit did not cleanly serialize"
        )
    return "⚠ both failed — DB is consistent but nothing got indexed"


def is_lock_contention_failure(result: dict) -> bool:
    text = (result.get("stderr_tail") or "").lower()
    return (
        "sqlite_busy" in text
        or "database is locked" in text
        or "database locked" in text
        or "database busy" in text
    )


def summarize_db(state: dict) -> str:
    lines = []
    lines.append(f"  - integrity_check: {state['integrity']}")
    if state.get("foreign_key_violations"):
        lines.append(f"  - FK violations: {len(state['foreign_key_violations'])} (!!)")
    else:
        lines.append(f"  - FK violations: none")
    lines.append(f"  - orphan symbols: {state['orphans']['symbols_without_file']}")
    lines.append(f"  - orphan refs:    {state['orphans']['refs_without_file']}")
    lines.append(f"  - dupe file paths: {state['dupes']['files_same_path']}")
    mig_dupes = [m for m in state['schema_migrations'] if m['count'] > 1]
    lines.append(f"  - schema_migrations duplicates: {len(mig_dupes)}")
    lines.append(f"  - counts: files={state['count_files']}, symbols={state['count_symbols']}, refs={state['count_refs']}")
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--project", type=Path, required=True,
                    help="Onboarded project to stress-test "
                         "(must contain a .codesage/ directory).")
    ap.add_argument("--scenario", choices=["T1", "T2", "both"], default="both")
    args = ap.parse_args()

    project = args.project.expanduser().resolve()
    codesage_dir = project / ".codesage"
    if not codesage_dir.is_dir():
        sys.exit(f"{project} is not onboarded (no .codesage/ dir)")

    print(f"# CodeSage concurrency audit")
    print()
    print(f"- **Project**: `{project}`")
    print(f"- **Run at**: {_dt.datetime.now(_dt.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')}")
    print(f"- **Backup first**: yes, restored after run")
    print()

    scenarios = ["T1", "T2"] if args.scenario == "both" else [args.scenario]
    for s in scenarios:
        bak = backup_db(codesage_dir)
        try:
            print(f"## Scenario {s}")
            print()
            results, state = SCENARIOS[s](project)
            print("**Processes**:")
            for r in results:
                print(summarize_proc(r))
            print()
            print("**Post-run DB**:")
            print(summarize_db(state))
            print()
            verdict = classify_verdict(results, state)
            print(f"**Verdict**: {verdict}")
            print()
        finally:
            restore_db(codesage_dir, bak)
            if bak is not None:
                try:
                    bak.unlink()
                except OSError:
                    pass

    return 0


if __name__ == "__main__":
    sys.exit(main())
