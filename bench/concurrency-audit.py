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
import re
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
    # Backup lives outside .codesage/ (system temp dir): a crash between
    # backup and the finally-unlink in main() must not leave a permanent
    # full-size copy inside the project dir.
    fd, bak_name = tempfile.mkstemp(prefix="index.db.audit-backup-")
    os.close(fd)
    bak = Path(bak_name)
    shutil.copy2(src, bak)
    return bak


def restore_db(codesage_dir: Path, bak: Path | None) -> None:
    src = codesage_dir / "index.db"
    # Re-check for live writers before touching -wal/-shm: unlinking a live
    # writer's WAL tears its uncheckpointed frames. A non-empty -wal means
    # someone (usually the codesage daemon) wrote during the audit run —
    # checkpoint first; on busy refuse instead of unlinking.
    wal = codesage_dir / "index.db-wal"
    try:
        wal_nonempty = wal.exists() and wal.stat().st_size > 0
    except OSError:
        wal_nonempty = True
    if wal_nonempty and src.exists():
        # A torn or non-database file (e.g. a half-written audit artifact)
        # has no live writer to protect — checkpoint errors mean "nothing
        # to checkpoint", so fall through to the unlink path below.
        try:
            conn = sqlite3.connect(str(src))
            try:
                row = conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()
            finally:
                conn.close()
        except sqlite3.Error:
            row = None
        if row is not None and row[0] != 0:
            sys.exit(
                "wal_checkpoint(TRUNCATE) returned busy=1 on "
                f"{src} — another process holds the database. "
                "Stop the codesage daemon (`codesage daemon stop`) and any "
                "running indexer, then re-run this audit. "
                "Refusing to unlink the live -wal."
            )
    # Remove WAL/SHM siblings so the restored .db (which was checkpointed
    # to a self-contained state at backup time) isn't shadowed by stale
    # log frames written during the audit run.
    for ext in ("-wal", "-shm"):
        side = codesage_dir / f"index.db{ext}"
        if side.exists():
            side.unlink()
    if bak is None:
        if src.exists():
            src.unlink()
        return
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

REQUIRED_TABLES = ("files", "symbols", "refs", "schema_migrations")


def _is_safe_table(name: str) -> bool:
    # sqlite_master names reach SQL text below; quote-wrap alone does not
    # make an arbitrary identifier safe (embedded quotes break out).
    return re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name) is not None


def semantic_chunk_check(conn: sqlite3.Connection) -> dict:
    """Best-effort semantic/vector consistency without loading sqlite-vec."""
    out: dict = {
        "status": "ok",
        "vec_tables": [],
        "issues": [],
        "fts_mismatches": [],
    }
    vec_rows = conn.execute(
        "SELECT name FROM sqlite_master "
        "WHERE type='table' AND sql LIKE '%vec0%'"
    ).fetchall()
    for (table_name,) in vec_rows:
        if not _is_safe_table(table_name):
            out["issues"].append(f"skipping unexpected table name: {table_name!r}")
            continue
        out["vec_tables"].append(table_name)
        fts_name = f"{table_name}_fts"
        fts_row = conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name=?",
            (fts_name,),
        ).fetchone()
        fts_count = None
        if fts_row:
            if not _is_safe_table(fts_name):
                out["issues"].append(f"skipping unexpected table name: {fts_name!r}")
            else:
                try:
                    fts_count = conn.execute(
                        f'SELECT COUNT(*) FROM "{fts_name}"'
                    ).fetchone()[0]
                except sqlite3.OperationalError as e:
                    out["issues"].append(f"fts query failed for {fts_name}: {e}")
        try:
            vec_count = conn.execute(
                f'SELECT COUNT(*) FROM "{table_name}"'
            ).fetchone()[0]
            if fts_count is not None and vec_count != fts_count:
                out["fts_mismatches"].append(
                    {"table": table_name, "vec": vec_count, "fts": fts_count}
                )
        except sqlite3.OperationalError:
            out["status"] = "inconclusive"
            if fts_count is not None and fts_count > 0:
                out["issues"].append(
                    f"cannot verify vec table {table_name} "
                    f"(sqlite-vec not loaded; fts has {fts_count} rows)"
                )
    if out["fts_mismatches"]:
        out["status"] = "mismatch"
    return out


def integrity_check(db_path: Path) -> dict:
    """Run SQLite's own corruption probe plus targeted orphan queries."""
    conn = sqlite3.connect(str(db_path))
    try:
        conn.execute("PRAGMA foreign_keys=ON")
        out = {
            "integrity": "unknown",
            "orphans": {
                "symbols_without_file": 0,
                "refs_without_file": 0,
            },
            "dupes": {
                "files_same_path": 0,
            },
            "schema_migrations": [],
            "schema_missing": [],
            "query_errors": [],
            "foreign_key_violations": [],
            "semantic": {},
        }

        def count_query(label: str, sql: str) -> int:
            try:
                return conn.execute(sql).fetchone()[0]
            except sqlite3.OperationalError as e:
                out["query_errors"].append({"check": label, "error": str(e)})
                if out["integrity"] == "ok":
                    out["integrity"] = f"query failed: {label}: {e}"
                return 0

        rows = conn.execute("PRAGMA integrity_check").fetchall()
        out["integrity"] = ", ".join(r[0] for r in rows) if rows else "empty"

        existing_tables = {
            row[0]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            ).fetchall()
        }
        missing = [table for table in REQUIRED_TABLES if table not in existing_tables]
        out["schema_missing"] = missing
        if missing:
            if out["integrity"] == "ok":
                out["integrity"] = (
                    "schema incomplete: missing " + ", ".join(missing)
                )
            out["semantic"] = semantic_chunk_check(conn)
            return out

        # FK enforcement is ON per init_db but cheap to re-verify: list
        # foreign key violations (should be empty).
        fk_rows = conn.execute("PRAGMA foreign_key_check").fetchall()
        out["foreign_key_violations"] = [
            {"table": r[0], "rowid": r[1], "parent": r[2], "fkid": r[3]} for r in fk_rows
        ]
        # Symbols / refs without a parent file row (FK is ON DELETE CASCADE
        # so this should be impossible, but if WAL committed a half-state
        # we'd see it).
        out["orphans"]["symbols_without_file"] = count_query(
            "symbols_without_file",
            "SELECT COUNT(*) FROM symbols s "
            "LEFT JOIN files f ON s.file_id = f.id WHERE f.id IS NULL"
        )
        out["orphans"]["refs_without_file"] = count_query(
            "refs_without_file",
            "SELECT COUNT(*) FROM refs r "
            "LEFT JOIN files f ON r.from_file_id = f.id WHERE f.id IS NULL"
        )
        # Duplicate files by path (files.path is UNIQUE so this must be 0).
        out["dupes"]["files_same_path"] = count_query(
            "files_same_path",
            "SELECT COUNT(*) FROM ("
            "  SELECT path, COUNT(*) c FROM files GROUP BY path HAVING c > 1"
            ")"
        )
        # Schema-migration registry state — two concurrent writers could
        # each INSERT into schema_migrations. UNIQUE constraint on name
        # should prevent dupes, but worth verifying on the live DB.
        try:
            mig_rows = conn.execute(
                "SELECT name, COUNT(*) FROM schema_migrations GROUP BY name"
            ).fetchall()
        except sqlite3.OperationalError as e:
            out["query_errors"].append({"check": "schema_migrations", "error": str(e)})
            mig_rows = []
        out["schema_migrations"] = [{"name": n, "count": c} for n, c in mig_rows]
        # Summary row counts for sanity.
        for t in ("files", "symbols", "refs"):
            out[f"count_{t}"] = count_query(f"count_{t}", f"SELECT COUNT(*) FROM {t}")
        out["semantic"] = semantic_chunk_check(conn)
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
    semantic = state.get("semantic") or {}
    corrupt = (
        state["integrity"] != "ok"
        or state["orphans"]["symbols_without_file"] > 0
        or state["orphans"]["refs_without_file"] > 0
        or state["dupes"]["files_same_path"] > 0
        or any(m["count"] > 1 for m in state["schema_migrations"])
        or len(state.get("foreign_key_violations") or []) > 0
        or semantic.get("status") == "mismatch"
        or bool(semantic.get("fts_mismatches"))
    )
    if corrupt:
        return "❌ CORRUPT — see detail above; fix required (lockfile / busy_timeout / retry)"

    timed_out = sum(1 for r in results if r["returncode"] is None)
    procs_ok = sum(1 for r in results if r["returncode"] == 0)
    lockfile_skips = sum(1 for r in results if is_lockfile_skip(r))
    if timed_out:
        return (
            f"⚠ TIMEOUT — {timed_out} process(es) killed after the timeout; "
            "possible writer livelock, not a clean serialization (investigate)"
        )

    if semantic.get("status") == "inconclusive":
        return (
            "⚠ INCONCLUSIVE — vec tables present but sqlite-vec is not loaded; "
            "structural checks passed but semantic/vector state was not verified"
        )

    if procs_ok == len(results):
        if lockfile_skips:
            return (
                f"✓ serialized — {lockfile_skips} process(es) exited 0 after "
                "lockfile skip; DB is consistent"
            )
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


def is_lockfile_skip(result: dict) -> bool:
    text = (result.get("stderr_tail") or "").lower()
    return "another codesage indexer is running" in text and "skipping" in text


def summarize_db(state: dict) -> str:
    lines = []
    lines.append(f"  - integrity_check: {state.get('integrity', 'unknown')}")
    if state.get("schema_missing"):
        lines.append(f"  - missing schema tables: {', '.join(state['schema_missing'])}")
    if state.get("query_errors"):
        lines.append(f"  - query errors: {len(state['query_errors'])} (!!)")
    if state.get("foreign_key_violations"):
        lines.append(f"  - FK violations: {len(state['foreign_key_violations'])} (!!)")
    else:
        lines.append(f"  - FK violations: none")
    orphans = state.get("orphans") or {}
    dupes = state.get("dupes") or {}
    lines.append(f"  - orphan symbols: {orphans.get('symbols_without_file', 0)}")
    lines.append(f"  - orphan refs:    {orphans.get('refs_without_file', 0)}")
    lines.append(f"  - dupe file paths: {dupes.get('files_same_path', 0)}")
    schema_migrations = state.get("schema_migrations") or []
    mig_dupes = [m for m in schema_migrations if m.get('count', 0) > 1]
    lines.append(f"  - schema_migrations duplicates: {len(mig_dupes)}")
    counts = [
        f"{table}={state[f'count_{table}']}"
        for table in ("files", "symbols", "refs")
        if f"count_{table}" in state
    ]
    if counts:
        lines.append(f"  - counts: {', '.join(counts)}")
    else:
        lines.append("  - counts: unavailable")
    semantic = state.get("semantic") or {}
    if semantic:
        lines.append(f"  - semantic check: {semantic.get('status', 'unknown')}")
        if semantic.get("vec_tables"):
            lines.append(f"  - vec tables: {len(semantic['vec_tables'])}")
        if semantic.get("fts_mismatches"):
            lines.append(f"  - vec/fts mismatches: {len(semantic['fts_mismatches'])} (!!)")
        if semantic.get("issues"):
            lines.append(f"  - semantic notes: {len(semantic['issues'])}")
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
