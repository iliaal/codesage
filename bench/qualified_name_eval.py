#!/usr/bin/env python3
"""Compare production qualified-name retrieval against existing indexes.

Default: lexical candidates from read-only indexes. --pipeline: both complete
search functions use the same CUDA embedding and reranker, with disposable
SQLite backups. Build CUDA release dependencies first for that mode. The
baseline comes from Git and the candidate from the working tree; neither
query builder nor search pipeline is reimplemented in Python.
"""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import tempfile


def compile_builder(source, directory, name):
    start = source.index("fn token_looks_code_shaped(")
    end = source.index("/// Weight applied to BM25", start)
    program = source[start:end]
    if "fn build_fts_match_query_mode(" in source or "    use std::collections::HashSet;" not in program:
        program = "use std::collections::HashSet;\n" + program
    fallback = "build_fts_match_query_mode(&line, true)" if "fn build_fts_match_query_mode(" in source else 'String::new()'
    if "fn qualified_groups_enabled()" in source:
        flag_start = source.index("fn qualified_groups_enabled()")
        flag_end = source.index("fn query_has_rare_literal_with_groups", flag_start)
        program = source[flag_start:flag_end] + program
        fallback = f"if qualified_groups_enabled() {{ {fallback} }} else {{ String::new() }}"
    program += """
fn main() {
    use std::io::BufRead;
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        println!("{}\\t{}", build_fts_match_query(&line), FALLBACK);
    }
}
""".replace("FALLBACK", fallback)
    rust = directory / f"{name}.rs"
    binary = directory / name
    rust.write_text(program)
    subprocess.run(["rustc", "--edition=2024", "-O", str(rust), "-o", str(binary)], check=True)
    return binary


def benchmark_environment(experimental):
    environment = os.environ.copy()
    environment.pop("CODESAGE_QUALIFIED_GROUPS", None)
    if experimental:
        environment["CODESAGE_QUALIFIED_GROUPS"] = "1"
    return environment


def expressions(binary, cases, experimental):
    result = subprocess.run(
        [str(binary)], input="\n".join(case["query"] for case in cases) + "\n",
        text=True, capture_output=True, check=True, env=benchmark_environment(experimental),
    )
    lines = result.stdout.splitlines()
    if len(lines) != len(cases):
        raise ValueError("builder returned the wrong number of expressions")
    return [line.split("\t") for line in lines]


def rank(connection, table, expressions, expected):
    expression, fallback = expressions
    if not expression:
        return None
    rows = connection.execute(
        f'SELECT file_path FROM "{table}" WHERE "{table}" MATCH ? ORDER BY rank LIMIT 100',
        (expression,),
    ).fetchall()
    if not rows and fallback and fallback != expression:
        rows = connection.execute(
            f'SELECT file_path FROM "{table}" WHERE "{table}" MATCH ? ORDER BY rank LIMIT 100',
            (fallback,),
        ).fetchall()
    return next((i for i, (path,) in enumerate(rows, 1) if path in expected), None)


def pipeline_results(root, baseline, candidate, cases, projects, deps, experimental):
    with tempfile.TemporaryDirectory(prefix="codesage-qualified-pipeline-") as temporary:
        directory = Path(temporary)
        (directory / "qualified-baseline.rs").write_text(baseline)
        (directory / "qualified-candidate.rs").write_text(candidate)
        runner = directory / "pipeline.rs"
        runner.write_text((root / "bench/qualified_name_pipeline.rs").read_text())
        binary = directory / "pipeline"
        command = ["rustc", "--edition=2024", "-C", "opt-level=2", "-L", f"dependency={deps}", str(runner), "-o", str(binary)]
        for name in ["codesage_embed", "codesage_storage", "codesage_protocol", "codesage_parser", "serde_json", "anyhow", "regex", "globset", "tracing"]:
            libraries = list(deps.glob(f"lib{name}-*.rlib"))
            if not libraries:
                raise ValueError(f"missing {name} build artifact; build a CUDA release first")
            library = max(libraries, key=lambda path: path.stat().st_mtime)
            command += ["--extern", f"{name}={library}"]
        for native in (deps.parent / "build").glob("*/out"):
            command += ["-L", f"native={native}"]
        subprocess.run(command, check=True)
        snapshots = {}
        for name in {case["project"] for case in cases}:
            source = Path(projects[name]).resolve() / ".codesage/index.db"
            target = directory / f"{name}.db"
            with sqlite3.connect(source.as_uri() + "?mode=ro", uri=True) as connection:
                with sqlite3.connect(target) as backup:
                    connection.backup(backup)
            snapshots[name] = str(target)
        cases_path = directory / "cases.json"
        projects_path = directory / "projects.json"
        cases_path.write_text(json.dumps(cases))
        projects_path.write_text(json.dumps(snapshots))
        execution = subprocess.run([str(binary), str(cases_path), str(projects_path)], text=True, capture_output=True, check=True, env=benchmark_environment(experimental))
        if "Embedding execution provider: cuda" not in execution.stderr:
            raise ValueError("pipeline benchmark did not verify CUDA execution")
        records = [json.loads(line) for line in execution.stdout.splitlines()]
        if len(records) != len(cases):
            raise ValueError("pipeline returned the wrong number of cases")
        results = []
        for case, record in zip(cases, records, strict=True):
            if record["case"] != case:
                raise ValueError("pipeline case order changed")
            result = dict(case)
            for arm in ["baseline", "candidate"]:
                paths = [row["file_path"] for row in record[arm]]
                result[f"{arm}_rank"] = next((i for i, path in enumerate(paths, 1) if path in case["expected"]), None)
                result[f"{arm}_paths"] = paths
            results.append(result)
        return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, help="Git commit containing the baseline builder")
    parser.add_argument("--cases", type=Path, required=True)
    parser.add_argument("--project", action="append", required=True, metavar="NAME=ABSOLUTE_PATH")
    parser.add_argument("--split", choices=["development", "holdout"], required=True)
    parser.add_argument("--pipeline", action="store_true")
    parser.add_argument("--experimental", action="store_true", help="enable default-off CODESAGE_QUALIFIED_GROUPS=1 in the benchmark child")
    parser.add_argument("--deps", type=Path, help="CUDA release dependency directory (default: target/release/deps)")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    projects = dict(value.split("=", 1) for value in args.project)
    cases = [case for case in json.loads(args.cases.read_text()) if case["split"] == args.split]
    if not cases:
        parser.error("selected split contains no cases")
    for case in cases:
        if not re.fullmatch(r"[A-Za-z0-9_-]+", case["project"]):
            parser.error("project names must contain only letters, digits, underscores, or hyphens")
        if case["project"] not in projects:
            parser.error(f"missing --project for {case['project']}")
    baseline = subprocess.run(
        ["git", "show", f"{args.baseline}:crates/graph/src/search.rs"],
        cwd=root, check=True, capture_output=True, text=True,
    ).stdout
    candidate = (root / "crates/graph/src/search.rs").read_text()
    if args.pipeline:
        results = pipeline_results(root, baseline, candidate, cases, projects, (args.deps or root / "target/release/deps").resolve(), args.experimental)
    else:
        results = lexical_results(baseline, candidate, cases, projects, args.experimental)
    metrics = {}
    for group in sorted({case.get("group", case["project"]) for case in cases}):
        rows = [row for row in results if row.get("group", row["project"]) == group]
        metrics[group] = {"queries": len(rows)}
        for arm in ["baseline", "candidate"]:
            ranks = [row[f"{arm}_rank"] for row in rows]
            metrics[group][arm] = {
                "hit_rate_at_10": sum(rank is not None and rank <= 10 for rank in ranks) / len(rows),
                "discounted_first_hit_at_10": sum(1 / math.log2(rank + 1) for rank in ranks if rank is not None and rank <= 10) / len(rows),
            }
    print(json.dumps({
        "scope": "production search, shared CUDA embedding per query, shared reranker, SQLite backups" if args.pipeline else "lexical candidate ranks, existing indexed chunks, no semantic fusion",
        "metric": "First acceptable file's chunk rank, alternatives count as one target; discounted hit is 1/log2(rank+1), zero past rank10. Not multi-relevance NDCG.",
        "baseline": args.baseline,
        "experimental_groups": args.experimental,
        "candidate_source_sha256": hashlib.sha256(candidate.encode()).hexdigest(),
        "cases_sha256": hashlib.sha256(args.cases.read_bytes()).hexdigest(),
        "split": args.split,
        "metrics": metrics,
        "results": results,
    }, indent=2))


def lexical_results(baseline, candidate, cases, projects, experimental):
    connections = {}
    for name in {case["project"] for case in cases}:
        db = Path(projects[name]).resolve() / ".codesage/index.db"
        connection = sqlite3.connect(db.as_uri() + "?mode=ro", uri=True)
        tables = connection.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND sql LIKE '%USING fts5(%'"
        ).fetchall()
        if len(tables) != 1 or not re.fullmatch(r"[A-Za-z0-9_]+", tables[0][0]):
            raise ValueError(f"{name}: expected exactly one FTS table")
        connections[name] = (connection, tables[0][0])
    with tempfile.TemporaryDirectory(prefix="codesage-qualified-") as temporary:
        directory = Path(temporary)
        before = expressions(compile_builder(baseline, directory, "baseline"), cases, experimental)
        after = expressions(compile_builder(candidate, directory, "candidate"), cases, experimental)
    results = []
    for case, before_expression, after_expression in zip(cases, before, after, strict=True):
        connection, table = connections[case["project"]]
        results.append({
            **case,
            "baseline_expression": before_expression,
            "candidate_expression": after_expression,
            "baseline_rank": rank(connection, table, before_expression, case["expected"]),
            "candidate_rank": rank(connection, table, after_expression, case["expected"]),
        })
    for connection, _ in connections.values():
        connection.close()
    return results


if __name__ == "__main__":
    main()
