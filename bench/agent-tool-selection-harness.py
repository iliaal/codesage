#!/usr/bin/env python3
"""
Active tool-selection harness for recommendations doc §2.3.

The retrospective half (`bench/analyze-codesage-quality.py` with the
§2.3 metric added) counts how often the agent *did* pick CodeSage vs
Grep in real sessions. This is the active half: drive Claude through a
fixed, unbiased find-a-file task with both toolsets exposed, and record
which tool it reached for first. Same question from the other side —
when the agent is handed both options and a concrete retrieval task,
does it use CodeSage?

Runs each corpus case as its own `claude -p` session with
`--output-format stream-json`. Parses the tool_use events out of the
stream. Two conditions per task:

  WITH codesage:   allowed = Grep,Read,Glob + mcp__codesage__{search,find_symbol,find_references,list_dependencies}
  WITHOUT codesage: allowed = Grep,Read,Glob only

Metrics:
  - first_tool: which tool fired first
  - total_calls: how many tool_uses across the session
  - used_codesage: did any codesage retrieval tool get called
  - found_expected_in_answer: did the agent's final text name an expected file

Output is a markdown scorecard comparable to the other bench runners.
Needs `claude` CLI on PATH. No API key setup — uses whatever auth the
CLI already has. Expect ~$0.20-0.30 per task in cache-read cost.

Usage:
  bench/agent-tool-selection-harness.py <corpus.yaml> [--limit 5] [--condition with|without|both]

Stdlib + pyyaml.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

try:
    import yaml
except ImportError:
    sys.exit("pyyaml required: pip install pyyaml")


# Retrieval-class CodeSage tools. Excludes risk/coupling because they
# answer a different question; this harness measures retrieval picks only.
CODESAGE_RETRIEVAL_TOOLS = [
    "mcp__codesage__search",
    "mcp__codesage__find_symbol",
    "mcp__codesage__find_references",
    "mcp__codesage__list_dependencies",
    "mcp__codesage__impact_analysis",
    "mcp__codesage__export_context",
]

BASE_TOOLS = ["Grep", "Read", "Glob"]


def build_prompt(query: str) -> str:
    """Convert a commit-subject query into a neutral find-a-file task.

    The retrospective analyzer established that agents reflexively pick
    Grep on identifier-shaped patterns. This prompt deliberately does
    not hint at which path holds the answer, so the agent has to pick a
    tool based on the tools' self-described strengths. Answer format is
    one line for easy parsing.
    """
    return (
        f"Find the file in this codebase that implements / addresses the "
        f"following change, described in commit-message style:\n\n"
        f"  {query}\n\n"
        f"Respond with only the relative file path(s), one per line. "
        f"Do not explain. Use whichever tools you think are appropriate."
    )


def run_task(
    project_root: Path,
    prompt: str,
    with_codesage: bool,
    max_turns: int = 10,
    timeout_s: int = 180,
) -> dict[str, Any]:
    """Run one `claude -p` task and return a structured summary.

    Returns {first_tool, tool_calls, used_codesage, duration_s,
             cost_usd, result_text, codesage_count, grep_count, error}.
    """
    tools = list(BASE_TOOLS)
    if with_codesage:
        tools.extend(CODESAGE_RETRIEVAL_TOOLS)
    cmd = [
        "claude", "-p", prompt,
        "--allowedTools", ",".join(tools),
        "--dangerously-skip-permissions",
        "--output-format", "stream-json",
        "--verbose",
        "--max-turns", str(max_turns),
    ]
    t0 = time.time()
    try:
        r = subprocess.run(
            cmd,
            cwd=str(project_root),
            capture_output=True,
            text=True,
            timeout=timeout_s,
        )
    except subprocess.TimeoutExpired:
        return {
            "error": "timeout",
            "duration_s": timeout_s,
            "first_tool": None,
            "tool_calls": [],
            "used_codesage": False,
            "codesage_count": 0,
            "grep_count": 0,
            "cost_usd": 0.0,
            "result_text": "",
        }
    duration = time.time() - t0

    # Parse stream-json lines. Each line is one event; tool_use blocks
    # show up inside `message.content`; the final `result` event carries
    # cost + final text.
    tool_uses: list[str] = []
    result_text = ""
    cost_usd = 0.0
    for line in r.stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if obj.get("type") == "assistant":
            content = (obj.get("message") or {}).get("content") or []
            for c in content:
                if isinstance(c, dict) and c.get("type") == "tool_use":
                    tool_uses.append(c.get("name") or "")
        elif obj.get("type") == "result":
            result_text = (obj.get("result") or "").strip()
            cost_usd = float(obj.get("total_cost_usd") or 0.0)

    first_tool = tool_uses[0] if tool_uses else None
    codesage_count = sum(1 for t in tool_uses if t.startswith("mcp__codesage__"))
    grep_count = sum(1 for t in tool_uses if t == "Grep")
    return {
        "error": None if r.returncode == 0 else f"rc={r.returncode}",
        "duration_s": round(duration, 2),
        "first_tool": first_tool,
        "tool_calls": tool_uses,
        "used_codesage": codesage_count > 0,
        "codesage_count": codesage_count,
        "grep_count": grep_count,
        "cost_usd": cost_usd,
        "result_text": result_text,
    }


def score_task(result: dict[str, Any], expected_files: list[str]) -> dict[str, Any]:
    """Did the agent's final answer include any of the expected files?
    Loose match: we accept if the expected relative path appears
    anywhere in the result text (tolerates leading `./`, quoting, etc.).
    """
    text = result.get("result_text", "")
    hit = any(exp.strip() and exp in text for exp in expected_files)
    return {
        **result,
        "found_expected": hit,
    }


def render_scorecard(
    *,
    condition: str,
    project_root: Path,
    corpus_name: str,
    rows: list[dict[str, Any]],
    run_ts: str,
) -> str:
    out: list[str] = []
    out.append(f"# Agent tool-selection harness — {condition}")
    out.append("")
    out.append(f"- **Project**: `{project_root}`")
    out.append(f"- **Corpus**: `{corpus_name}` — {len(rows)} tasks")
    out.append(f"- **Condition**: {condition}")
    out.append(f"- **Run at**: {run_ts}")
    total_cost = sum(r.get("cost_usd") or 0.0 for r in rows)
    total_duration = sum(r.get("duration_s") or 0.0 for r in rows)
    out.append(f"- **Total cost**: ${total_cost:.2f}")
    out.append(f"- **Total wall-clock**: {total_duration:.0f}s")
    out.append("")

    out.append("## Per-task results")
    out.append("")
    out.append("| id | first tool | total calls | codesage calls | grep calls | found expected | cost |")
    out.append("|---|---|---:|---:|---:|:---:|---:|")
    for r in rows:
        first = r.get("first_tool") or "—"
        if first.startswith("mcp__codesage__"):
            first_show = f"**codesage:{first[len('mcp__codesage__'):]}**"
        else:
            first_show = first
        found = "✓" if r.get("found_expected") else "✗"
        out.append(
            f"| {r['id']} | {first_show} | {len(r.get('tool_calls', []))} | "
            f"{r.get('codesage_count', 0)} | {r.get('grep_count', 0)} | {found} | "
            f"${r.get('cost_usd', 0.0):.2f} |"
        )
    out.append("")

    out.append("## Aggregate")
    out.append("")
    n = len(rows) or 1
    picked_cs_first = sum(1 for r in rows if (r.get("first_tool") or "").startswith("mcp__codesage__"))
    used_cs = sum(1 for r in rows if r.get("used_codesage"))
    found = sum(1 for r in rows if r.get("found_expected"))
    mean_calls = sum(len(r.get("tool_calls", [])) for r in rows) / n
    out.append(f"- **First-tool-is-CodeSage**: {picked_cs_first}/{n} ({100.0 * picked_cs_first / n:.0f}%)")
    out.append(f"- **Any CodeSage tool used in session**: {used_cs}/{n} ({100.0 * used_cs / n:.0f}%)")
    out.append(f"- **Found expected file in answer**: {found}/{n} ({100.0 * found / n:.0f}%)")
    out.append(f"- **Mean tool calls per task**: {mean_calls:.1f}")
    out.append("")
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("corpus", type=Path)
    ap.add_argument("--limit", type=int, default=5,
                    help="How many corpus cases to run (default 5, bench budget).")
    ap.add_argument("--condition", choices=["with", "without", "both"], default="with",
                    help="Which toolset the agent sees. `both` runs each task twice.")
    ap.add_argument("--output-dir", type=Path, default=None)
    ap.add_argument("--max-turns", type=int, default=10)
    args = ap.parse_args()

    corpus = yaml.safe_load(args.corpus.read_text())
    project_root = Path(corpus["project_root"]).expanduser().resolve()
    cases = corpus["cases"][: args.limit]
    run_ts = _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    conditions = ["with", "without"] if args.condition == "both" else [args.condition]
    all_reports: list[tuple[str, str]] = []

    for cond in conditions:
        rows = []
        for case in cases:
            prompt = build_prompt(case["query"])
            print(f"[{cond}] {case['id']}: {case['query'][:70]!r}", file=sys.stderr)
            r = run_task(
                project_root,
                prompt,
                with_codesage=(cond == "with"),
                max_turns=args.max_turns,
            )
            scored = score_task(r, case["expected_files"])
            scored["id"] = case["id"]
            rows.append(scored)
            print(
                f"  first={scored.get('first_tool')} "
                f"calls={len(scored.get('tool_calls', []))} "
                f"cs={scored.get('codesage_count', 0)} "
                f"grep={scored.get('grep_count', 0)} "
                f"found={scored.get('found_expected')} "
                f"cost=${scored.get('cost_usd', 0.0):.2f}",
                file=sys.stderr,
            )
        report = render_scorecard(
            condition=cond,
            project_root=project_root,
            corpus_name=args.corpus.name,
            rows=rows,
            run_ts=run_ts,
        )
        all_reports.append((cond, report))
        if args.output_dir:
            args.output_dir.mkdir(parents=True, exist_ok=True)
            out_path = args.output_dir / f"{args.corpus.stem}-tool-selection-{cond}.md"
            out_path.write_text(report, encoding="utf-8")
            print(f"wrote {out_path}", file=sys.stderr)
        print(report)

    return 0


if __name__ == "__main__":
    sys.exit(main())
