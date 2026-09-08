#!/usr/bin/env python3
"""Measure an isolated callee FTS field through unchanged production search."""

import argparse
from collections import defaultdict
import hashlib
import json
import math
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import tempfile


def freeze_cases(projects, controls):
    cases = []
    for project, root in projects.items():
        with sqlite3.connect(
            (root / ".codesage/index.db").as_uri() + "?mode=ro", uri=True
        ) as db:
            calls = db.execute(
                "SELECT DISTINCT r.to_name, f.path FROM refs r JOIN files f ON f.id=r.from_file_id WHERE r.kind='call'"
            ).fetchall()
        names = defaultdict(set)
        for name, path in calls:
            if not any(
                part in {"tests", "test", "fixtures", "types"}
                for part in Path(path).parts
            ):
                names[name].add(path)
        names = [
            (name, sorted(paths))
            for name, paths in names.items()
            if len(paths) == 1 and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{7,}", name)
        ]
        names.sort(
            key=lambda row: hashlib.sha256((project + "\0" + row[0]).encode()).digest()
        )
        for name, paths in names[:40]:
            cases.append(
                {
                    "project": project,
                    "query": f"code that calls `{name}`",
                    "expected": paths,
                    "group": project + "-caller",
                    "origin": "synthetic unique indexed caller probe",
                }
            )
    cases.extend(
        {**case, "group": case["project"] + "-control"}
        for case in json.loads(controls.read_text())
        if case["project"] in projects
        and case["split"] == "development"
        and "." not in case["group"]
    )
    return cases


def candidate_sidecar(db):
    tables = db.execute(
        "SELECT name FROM sqlite_master WHERE sql LIKE '%USING fts5(%'"
    ).fetchall()
    if len(tables) != 1 or not re.fullmatch(r"[A-Za-z0-9_]+", tables[0][0]):
        raise ValueError("expected one model FTS table")
    table = tables[0][0]
    columns = [row[1] for row in db.execute(f'PRAGMA table_info("{table}")')]
    if columns != ["content", "file_path", "language", "start_line", "end_line"]:
        raise ValueError(f"unexpected production columns: {columns}")
    rows = db.execute(
        f'SELECT rowid, content, file_path, language, start_line, end_line FROM "{table}" ORDER BY rowid'
    ).fetchall()
    calls = defaultdict(list)
    for path, line, name in db.execute(
        "SELECT f.path,r.line,r.to_name FROM refs r JOIN files f ON f.id=r.from_file_id WHERE r.kind='call' ORDER BY f.path,r.line,r.to_name"
    ):
        calls[path].append((line, name))
    populated = []
    added = 0
    absent = 0
    for row in rows:
        names = sorted(
            {name for line, name in calls[row[2]] if row[4] <= line <= row[5]}
        )
        added += bool(names)
        absent += sum(name not in row[1] for name in names)
        populated.append((*row, " ".join(names)))
    db.execute(f'DROP TABLE "{table}_vocab"')
    db.execute(f'DROP TABLE "{table}"')
    db.execute(
        f"""CREATE VIRTUAL TABLE "{table}" USING fts5(content, file_path UNINDEXED, language UNINDEXED, start_line UNINDEXED, end_line UNINDEXED, callees, tokenize = "unicode61 remove_diacritics 1 tokenchars '_'")"""
    )
    db.executemany(
        f'INSERT INTO "{table}"(rowid, content, file_path, language, start_line, end_line, callees) VALUES (?,?,?,?,?,?,?)',
        populated,
    )
    db.execute(f'CREATE VIRTUAL TABLE "{table}_vocab" USING fts5vocab("{table}", row)')
    assert (
        rows
        == db.execute(
            f'SELECT rowid, content, file_path, language, start_line, end_line FROM "{table}" ORDER BY rowid'
        ).fetchall()
    )
    db.commit()
    return {
        "chunks": len(rows),
        "chunks_with_callees": added,
        "callee_names_absent_from_body": absent,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", default="136dba6")
    parser.add_argument(
        "--project", action="append", required=True, metavar="NAME=PATH"
    )
    parser.add_argument("--cases", type=Path, required=True)
    parser.add_argument("--freeze", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--deps", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    projects = {
        name: Path(path).resolve()
        for name, path in (value.split("=", 1) for value in args.project)
    }
    if args.freeze:
        if args.cases.exists():
            raise ValueError("refusing to replace frozen cases")
        args.cases.write_text(
            json.dumps(
                freeze_cases(projects, root / "bench/qualified-name/cases.json"),
                indent=2,
            )
            + "\n"
        )
    cases = json.loads(args.cases.read_text())
    if not cases or any(case["project"] not in projects for case in cases):
        raise ValueError("empty cases or missing project")
    source = subprocess.run(
        ["git", "show", f"{args.baseline}:crates/graph/src/search.rs"],
        cwd=root,
        text=True,
        capture_output=True,
        check=True,
    ).stdout
    deps = (args.deps or root / "target/release/deps").resolve()
    artifacts = {}
    with tempfile.TemporaryDirectory(prefix="codesage-callee-") as temporary:
        directory = Path(temporary)
        (directory / "search.rs").write_text(source)
        (directory / "pipeline.rs").write_text(
            (Path(__file__).parent / "pipeline.rs").read_text()
        )
        binary = directory / "pipeline"
        command = [
            "rustc",
            "--edition=2024",
            "-C",
            "opt-level=2",
            "-L",
            f"dependency={deps}",
            str(directory / "pipeline.rs"),
            "-o",
            str(binary),
        ]
        for name in [
            "codesage_embed",
            "codesage_storage",
            "codesage_protocol",
            "codesage_parser",
            "serde_json",
            "anyhow",
            "regex",
            "globset",
            "tracing",
        ]:
            library = max(
                deps.glob(f"lib{name}-*.rlib"), key=lambda path: path.stat().st_mtime
            )
            artifacts[name] = {
                "file": library.name,
                "sha256": hashlib.file_digest(library.open("rb"), "sha256").hexdigest(),
            }
            command += ["--extern", f"{name}={library}"]
        for native in (deps.parent / "build").glob("*/out"):
            command += ["-L", f"native={native}"]
        subprocess.run(command, check=True)
        snapshots = {}
        indexes = {}
        for name, project in projects.items():
            paths = [
                directory / f"{name}-{arm}.db" for arm in ["baseline", "candidate"]
            ]
            with sqlite3.connect(
                (project / ".codesage/index.db").as_uri() + "?mode=ro", uri=True
            ) as original:
                with sqlite3.connect(paths[0]) as baseline:
                    original.backup(baseline)
                    with sqlite3.connect(paths[1]) as candidate:
                        baseline.backup(candidate)
                        indexes[name] = candidate_sidecar(candidate)
            indexes[name]["baseline_snapshot_sha256"] = hashlib.file_digest(
                paths[0].open("rb"), "sha256"
            ).hexdigest()
            indexes[name]["git_revision"] = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=project,
                capture_output=True,
                text=True,
                check=True,
            ).stdout.strip()
            snapshots[name] = list(map(str, paths))
        (directory / "projects.json").write_text(json.dumps(snapshots))
        environment = os.environ.copy()
        for name in list(environment):
            if name.startswith("CODESAGE_"):
                environment.pop(name)
        result = subprocess.run(
            [str(binary), str(args.cases.resolve()), str(directory / "projects.json")],
            capture_output=True,
            text=True,
            check=True,
            env=environment,
        )
        records = [json.loads(line) for line in result.stdout.splitlines()]
    if len(records) != len(cases) or any(
        record["case"] != case for record, case in zip(records, cases, strict=True)
    ):
        raise ValueError("case/response mismatch")
    metrics = {}
    for group in sorted({case["group"] for case in cases}):
        selected = [record for record in records if record["case"]["group"] == group]
        metrics[group] = {"n": len(selected)}
        for arm in ["baseline", "candidate"]:
            for record in selected:
                record[arm + "_rank"] = next(
                    (
                        i
                        for i, row in enumerate(record[arm], 1)
                        if row["file_path"] in record["case"]["expected"]
                    ),
                    None,
                )
            ranks = [record[arm + "_rank"] for record in selected]
            metrics[group][arm] = {
                "hits": sum(rank is not None for rank in ranks),
                "discounted_hit": sum(
                    1 / math.log2(rank + 1) for rank in ranks if rank is not None
                )
                / len(ranks),
            }
    for record in records:
        for arm in ["baseline", "candidate"]:
            record[arm] = [
                {
                    key: row[key]
                    for key in ["file_path", "start_line", "end_line", "score"]
                }
                for row in record[arm]
            ]
    args.output.write_text(
        json.dumps(
            {
                "baseline": args.baseline,
                "search_sha256": hashlib.sha256(source.encode()).hexdigest(),
                "cases_sha256": hashlib.sha256(args.cases.read_bytes()).hexdigest(),
                "scope": "development evidence; frozen synthetic caller probes plus reused qualified-name development controls; identical production search on paired SQLite backups; body1/callee1 default FTS bm25 weights",
                "artifacts": artifacts,
                "indexes": indexes,
                "metrics": metrics,
                "results": records,
            },
            indent=2,
        )
        + "\n"
    )
    print(json.dumps(metrics, indent=2))


if __name__ == "__main__":
    main()
