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
import re
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

# Tools the agent must never use in either arm. Bash/Edit/Write/etc. would let
# it shell out to `codesage search` (a confound the harness must exclude) or
# modify the benched repo. Read/Glob/Grep retrieval is all the task needs.
DISALLOWED_TOOLS = ["Bash", "Edit", "Write", "NotebookEdit", "WebFetch", "WebSearch"]


def positive_int(value: str) -> int:
    n = int(value)
    if n < 1:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return n


def expected_tool_set(with_codesage: bool) -> set[str]:
    """The only tools the agent should be able to use under this condition."""
    s = set(BASE_TOOLS)
    if with_codesage:
        s.update(CODESAGE_RETRIEVAL_TOOLS)
    return s


def build_command(
    prompt: str,
    *,
    with_codesage: bool,
    max_turns: int,
    append_system_prompt_file: Path | None = None,
) -> list[str]:
    """Build the `claude -p` argv for one task.

    Critically does NOT pass `--dangerously-skip-permissions`: in headless
    print mode that flag lets the model call ANY tool (Bash, every globally
    registered MCP server) regardless of `--allowedTools`, so the with/without-
    codesage arms would not actually differ — the experiment would measure
    nothing. Without it, a tool that isn't allow-listed and needs permission is
    denied (no TTY to prompt on), which is exactly the gating this measurement
    relies on. The `without` arm additionally loads an empty MCP config and
    ignores all global ones (`--strict-mcp-config`), so the codesage server is
    not even offered to the model.
    """
    tools = list(BASE_TOOLS)
    if with_codesage:
        tools.extend(CODESAGE_RETRIEVAL_TOOLS)
    cmd = [
        "claude", "-p", prompt,
        "--allowedTools", ",".join(tools),
        "--disallowedTools", ",".join(DISALLOWED_TOOLS),
        "--output-format", "stream-json",
        "--verbose",
        "--max-turns", str(max_turns),
    ]
    if not with_codesage:
        cmd.extend(["--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}'])
    if append_system_prompt_file is not None:
        # `claude -p` only exposes `--append-system-prompt <prompt>`. Inline the
        # file's contents so the harness fails loudly at read time rather than
        # silently when `claude` rejects an unknown flag and every task records
        # `rc!=0`.
        cmd.extend([
            "--append-system-prompt",
            append_system_prompt_file.read_text(encoding="utf-8"),
        ])
    return cmd


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
    append_system_prompt_file: Path | None = None,
) -> dict[str, Any]:
    """Run one `claude -p` task and return a structured summary.

    Returns {first_tool, tool_calls, used_codesage, duration_s,
             cost_usd, result_text, codesage_count, grep_count, error}.

    `append_system_prompt_file` lets the caller measure whether a
    system-prompt-level steering fragment moves tool selection. Used by
    the §2.3 follow-up that ships the codesage prompt-override (see
    plugins/codesage-tools/bin/codesage-prompt-override).
    """
    cmd = build_command(
        prompt,
        with_codesage=with_codesage,
        max_turns=max_turns,
        append_system_prompt_file=append_system_prompt_file,
    )
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
            "unexpected_tools": [],
            "cost_usd": 0.0,
            "result_text": "",
            "stderr": "",
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
    # Self-check: any tool outside the condition's intended set means the gating
    # leaked and this row's with/without comparison is invalid (the failure mode
    # the old `--dangerously-skip-permissions` invocation hid).
    allowed = expected_tool_set(with_codesage)
    unexpected_tools = sorted({t for t in tool_uses if t and t not in allowed})
    if unexpected_tools:
        print(
            f"  ! unexpected tools used (measurement may be invalid): {unexpected_tools}",
            file=sys.stderr,
        )
    stderr_tail = (r.stderr or "")[-2000:]
    if r.returncode != 0 and stderr_tail.strip():
        # Surface the subprocess stderr immediately so a bad flag or auth
        # failure does not nuke an entire run while looking like "all rc=N".
        print(f"  ! claude rc={r.returncode}: {stderr_tail.strip()}", file=sys.stderr)
    return {
        "error": None if r.returncode == 0 else f"rc={r.returncode}",
        "duration_s": round(duration, 2),
        "first_tool": first_tool,
        "tool_calls": tool_uses,
        "used_codesage": codesage_count > 0,
        "codesage_count": codesage_count,
        "grep_count": grep_count,
        "unexpected_tools": unexpected_tools,
        "cost_usd": cost_usd,
        "result_text": result_text,
        "stderr": stderr_tail,
    }


def score_task(result: dict[str, Any], expected_files: list[str]) -> dict[str, Any]:
    """Did the agent's final answer include any of the expected files?
    Loose formatting is accepted (`./path`, quotes, punctuation), but path
    segments must match exactly so `src/foo.py.bak` and `other/src/foo.py`
    don't count as `src/foo.py`.
    """
    if result.get("error"):
        raise ValueError(f"cannot score failed run: {result['error']}")
    text = result.get("result_text", "")
    hit = any(exp.strip() and path_mentioned(text, exp.strip()) for exp in expected_files)
    return {
        **result,
        "found_expected": hit,
    }


def path_mentioned(text: str, expected: str) -> bool:
    pattern = re.compile(
        r"(?<![A-Za-z0-9_./-])(?:\./)?"
        + re.escape(expected.removeprefix("./"))
        + r"(?![A-Za-z0-9_./-])"
    )
    return pattern.search(text) is not None


def render_scorecard(
    *,
    condition: str,
    project_root: Path,
    corpus_name: str,
    rows: list[dict[str, Any]],
    run_ts: str,
    append_system_prompt_file: Path | None = None,
    label: str | None = None,
) -> str:
    out: list[str] = []
    title = f"Agent tool-selection harness — {condition}"
    if label:
        title += f" ({label})"
    out.append(f"# {title}")
    out.append("")
    out.append(f"- **Project**: `{project_root}`")
    out.append(f"- **Corpus**: `{corpus_name}` — {len(rows)} tasks")
    out.append(f"- **Condition**: {condition}")
    if append_system_prompt_file is not None:
        out.append(f"- **Append-system-prompt**: `{append_system_prompt_file}`")
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
    ap.add_argument("--limit", type=positive_int, default=5,
                    help="How many corpus cases to run (default 5, bench budget).")
    ap.add_argument("--condition", choices=["with", "without", "both"], default="with",
                    help="Which toolset the agent sees. `both` runs each task twice.")
    ap.add_argument("--output-dir", type=Path, default=None)
    ap.add_argument("--max-turns", type=int, default=10)
    ap.add_argument(
        "--append-system-prompt-file",
        type=Path,
        default=None,
        help="Append the contents of FILE to the agent's system prompt on every task. "
             "Used to measure whether system-prompt-level steering moves tool selection "
             "(see plugins/codesage-tools/bin/codesage-prompt-override).",
    )
    ap.add_argument(
        "--label",
        type=str,
        default=None,
        help="Optional suffix on the output filename (e.g. 'override') so reruns "
             "with different conditions don't overwrite each other.",
    )
    args = ap.parse_args()
    if args.append_system_prompt_file is not None and not args.append_system_prompt_file.exists():
        sys.exit(f"--append-system-prompt-file: {args.append_system_prompt_file} does not exist")

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
                append_system_prompt_file=args.append_system_prompt_file,
            )
            if r.get("error"):
                print(
                    f"  ! task failed ({r['error']}); aborting so aggregate scores stay valid",
                    file=sys.stderr,
                )
                return 1
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
            append_system_prompt_file=args.append_system_prompt_file,
            label=args.label,
        )
        all_reports.append((cond, report))
        if args.output_dir:
            args.output_dir.mkdir(parents=True, exist_ok=True)
            suffix = f"-{args.label}" if args.label else ""
            out_path = args.output_dir / f"{args.corpus.stem}-tool-selection-{cond}{suffix}.md"
            out_path.write_text(report, encoding="utf-8")
            print(f"wrote {out_path}", file=sys.stderr)
        print(report)

    return 0


if __name__ == "__main__":
    sys.exit(main())
