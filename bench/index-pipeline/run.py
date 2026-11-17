#!/usr/bin/env python3
"""Time `codesage index --full --no-semantic` for two binaries, interleaved,
and check that both write the same structural rows.

Usage: run.py BASE_BINARY HEAD_BINARY PROJECT_DIR [ROUNDS]
PROJECT_DIR must already hold a CodeSage index (`codesage init`).
"""
import hashlib
import sqlite3
import statistics
import subprocess
import sys
import time
from pathlib import Path

# Rowids and timestamps differ between any two passes; python_bindings rows
# for one file have no stable rowid order, so they are compared sorted.
TABLES = [
    ("files", {"indexed_at", "boundaries_derived_at"}, "id"),
    ("symbols", {"id"}, "id"),
    ("refs", {"id"}, "id"),
    ("symbol_fingerprints", set(), "rowid"),
    ("python_bindings", set(), "file_id, kind, to_name, line, col, target"),
    ("file_trust_boundaries", {"id"}, "rowid"),
]


def digest(db_path):
    conn = sqlite3.connect(db_path)
    out = {}
    for table, skip, order in TABLES:
        cols = [r[1] for r in conn.execute(f"PRAGMA table_info({table})") if r[1] not in skip]
        h = hashlib.sha256()
        rows = conn.execute(f"SELECT {', '.join(cols)} FROM {table} ORDER BY {order}")
        n = sum(1 for row in rows if h.update(repr(row).encode()) is None)
        out[table] = (n, h.hexdigest()[:16])
    conn.close()
    return out


def main():
    base, head, project = (str(Path(arg).resolve()) for arg in sys.argv[1:4])
    rounds = int(sys.argv[4]) if len(sys.argv) > 4 else 9
    db = Path(project) / ".codesage" / "index.db"
    times = {"base": [], "head": []}
    digests = {"base": set(), "head": set()}
    for i in range(rounds):
        order = [("base", base), ("head", head)]
        for name, binary in order if i % 2 == 0 else order[::-1]:
            start = time.perf_counter()
            subprocess.run([binary, "index", "--full", "--no-semantic"], cwd=project,
                           check=True, capture_output=True)
            times[name].append(time.perf_counter() - start)
            digests[name].add(repr(sorted(digest(db).items())))
    for name, samples in times.items():
        s = sorted(samples)
        print(f"{name}: median {statistics.median(s):.3f} s, min {s[0]:.3f}, max {s[-1]:.3f}, n={len(s)}")
    change = statistics.median(times["head"]) / statistics.median(times["base"]) - 1
    print(f"time {change:+.1%}")
    same = len(digests["base"] | digests["head"]) == 1
    print("rows identical:" if same else "ROWS DIFFER:", *sorted(digests["base"] | digests["head"]))
    sys.exit(0 if same else 1)


main()
