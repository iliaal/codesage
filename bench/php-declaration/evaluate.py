#!/usr/bin/env python3
"""Compare opt-in path penalties using both complete production search pipelines."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from qualified_name_eval import pipeline_results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cases", type=Path, required=True)
    parser.add_argument("--project", action="append", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--control", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    projects = dict(value.split("=", 1) for value in args.project)
    cases = json.loads(args.cases.read_text())
    for case in cases:
        case["project"] = Path(case["project"]).name
    if not cases:
        parser.error("cases must not be empty")
    revisions = {}
    for case in cases:
        project = case["project"]
        if project not in projects:
            parser.error(f"missing project {project}")
        project_root = Path(projects[project]).resolve()
        if not case["expected"] or any(not (project_root / path).is_file() for path in case["expected"]):
            parser.error(f"missing expected source for {case['query']}")
        if project not in revisions:
            revisions[project] = subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=project_root,
                check=True, capture_output=True, text=True,
            ).stdout.strip()
    candidate = (root / "crates/graph/src/search.rs").read_text()
    baseline = candidate
    for flag in ["CODESAGE_PLATFORM_DEMOTE", "CODESAGE_PHP_DECLARATION_DEMOTE"]:
        baseline_flag = flag + "_BASELINE_DISABLED"
        if baseline.count(f'"{flag}"') != 1:
            raise ValueError(f"expected exactly one tuning constant for {flag}")
        baseline = baseline.replace(f'"{flag}"', f'"{baseline_flag}"')
        os.environ[baseline_flag] = "0"
        os.environ[flag] = "0" if args.control else "1"
    records = pipeline_results(
        root, baseline, candidate, cases, projects,
        root / "target/release/deps", False,
    )
    groups = {}
    for category in sorted({record["category"] for record in records}):
        rows = [record for record in records if record["category"] == category]
        group = {"cases": len(rows)}
        for arm in ["baseline", "candidate"]:
            ranks = [row[f"{arm}_rank"] for row in rows]
            group[arm] = {
                "hits_at_10": sum(rank is not None for rank in ranks),
                "discounted_first_hit": sum(1 / math.log2(rank + 1) if rank else 0 for rank in ranks) / len(rows),
            }
        groups[category] = group
    if args.control and any(record["baseline_paths"] != record["candidate_paths"] for record in records):
        raise ValueError("disabled candidate changed baseline paths")
    output = {
        "control": args.control,
        "candidate_sha256": hashlib.sha256(candidate.encode()).hexdigest(),
        "baseline_sha256": hashlib.sha256(baseline.encode()).hexdigest(),
        "cases_sha256": hashlib.sha256(args.cases.read_bytes()).hexdigest(),
        "revisions": revisions,
        "groups": groups,
        "records": records,
    }
    args.output.write_text(json.dumps(output, indent=2) + "\n")
    print(json.dumps(groups, indent=2))


if __name__ == "__main__":
    main()
