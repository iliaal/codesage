#!/usr/bin/env python3
"""
Retrospective analysis of CodeSage MCP usage from Claude Code session logs.

Question this tool answers: would rtk-style output compression on codesage
MCP tool responses be worth building? (Recommendations doc §1.5.)

Approach: walk Claude Code session transcripts (`~/.claude/projects/*/*.jsonl`),
pair `tool_use` calls to `mcp__codesage__*` with their `tool_result` payloads,
measure response sizes, and simulate three compression rules on the actual
past responses. The output is a markdown scorecard with per-tool breakdown,
size distribution, simulated savings per rule, and a verdict.

No forward instrumentation, no schema changes. Uses real historical data to
decide whether output shaping is worth engineering or should be shelved.

Usage:
  bench/analyze-codesage-usage.py [--window-days 7] [--projects-root PATH] [--output PATH]

Stdlib only.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import re
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any

# -----------------------------------------------------------------------------
# Extraction
# -----------------------------------------------------------------------------

TOOL_PREFIX = "mcp__codesage__"


def iter_transcripts(root: Path, min_mtime: float) -> list[Path]:
    """All `*.jsonl` under `root/*/` modified at or after `min_mtime`."""
    out: list[Path] = []
    if not root.is_dir():
        return out
    for project in root.iterdir():
        if not project.is_dir():
            continue
        for f in project.glob("*.jsonl"):
            try:
                if f.stat().st_mtime >= min_mtime:
                    out.append(f)
            except OSError:
                continue
    return out


def extract_calls(transcript: Path) -> list[dict[str, Any]]:
    """Emit one record per codesage tool_use/tool_result pair found in the file.

    A record looks like:
        {
          "tool": "search",
          "input_bytes": 184,
          "output_bytes": 5031,
          "output_text": "<raw response text>",
          "transcript": str(transcript),
        }
    """
    pending: dict[str, dict[str, Any]] = {}  # tool_use_id -> partial
    out: list[dict[str, Any]] = []
    try:
        fp = transcript.open("r", encoding="utf-8", errors="replace")
    except OSError:
        return out
    with fp:
        for line in fp:
            line = line.strip()
            if not line or TOOL_PREFIX not in line and '"tool_result"' not in line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            msg = obj.get("message") if isinstance(obj, dict) else None
            content = msg.get("content") if isinstance(msg, dict) else None
            if not isinstance(content, list):
                continue
            for c in content:
                if not isinstance(c, dict):
                    continue
                ctype = c.get("type")
                if ctype == "tool_use":
                    name = c.get("name") or ""
                    if not name.startswith(TOOL_PREFIX):
                        continue
                    tool = name[len(TOOL_PREFIX):]
                    tid = c.get("id")
                    inp = c.get("input") or {}
                    inp_bytes = len(json.dumps(inp))
                    pending[tid] = {
                        "tool": tool,
                        "input_bytes": inp_bytes,
                        "transcript": str(transcript),
                    }
                elif ctype == "tool_result":
                    tid = c.get("tool_use_id")
                    if tid not in pending:
                        continue
                    partial = pending.pop(tid)
                    text = _flatten_result(c.get("content"))
                    partial["output_bytes"] = len(text)
                    partial["output_text"] = text
                    out.append(partial)
    return out


def _flatten_result(content: Any) -> str:
    """tool_result content is either a string or a list[{text: ...}]. Normalize."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: list[str] = []
        for item in content:
            if isinstance(item, dict):
                t = item.get("text")
                if isinstance(t, str):
                    parts.append(t)
        return "\n".join(parts)
    return ""


# -----------------------------------------------------------------------------
# Compression simulations
#
# Each rule takes the raw response text and returns the *compressed* size in
# bytes. If the rule is inapplicable to the shape (e.g. group-by-dir on a
# plain scalar JSON), it returns the original size. Rules never mutate any
# on-disk state; they're pure size arithmetic for estimation.
# -----------------------------------------------------------------------------


def _compress_file_array(items: list[Any]) -> list[Any]:
    """Apply group-by-directory to an array of file-shaped objects. Returns
    a shorter list when any directory has ≥3 items; otherwise returns the
    input unchanged. Pulled out so nested arrays (e.g. `.files[]` inside
    `assess_risk_diff`) can be compressed too, not just top-level arrays.
    """
    if len(items) < 3 or not all(isinstance(it, dict) for it in items):
        return items
    buckets: dict[str, list[dict]] = defaultdict(list)
    for it in items:
        p = it.get("file_path") or it.get("file")
        if not isinstance(p, str):
            return items  # not file-shaped
        d = os.path.dirname(p) or "."
        buckets[d].append(it)
    if all(len(v) < 3 for v in buckets.values()):
        return items  # no directory has enough items to cluster
    compressed: list[Any] = []
    for d, bucket in buckets.items():
        if len(bucket) >= 3:
            compressed.append({
                "directory": d,
                "count": len(bucket),
                "top_files": [
                    (it.get("file_path") or it.get("file")) for it in bucket[:3]
                ],
            })
        else:
            compressed.extend(bucket)
    return compressed


def rule_group_by_directory(text: str) -> int:
    """For JSON with file-shaped arrays (top-level or under keys like `files`,
    `test_gap_files`, `coupled_files`), collapse any directory with ≥3 items
    into `{directory, count, top_files[:3]}`. Targets `search`,
    `find_references`, `find_symbol`, `impact_analysis`, and the `.files[]`
    sub-array inside `assess_risk_diff` / `list_dependencies` output.
    """
    data = _safe_json(text)
    if data is None:
        return len(text)
    if isinstance(data, list):
        return len(json.dumps(_compress_file_array(data)))
    if isinstance(data, dict):
        out: dict[str, Any] = {}
        touched = False
        for k, v in data.items():
            if isinstance(v, list):
                compressed = _compress_file_array(v)
                if compressed is not v:
                    touched = True
                out[k] = compressed
            else:
                out[k] = v
        if not touched:
            return len(text)
        return len(json.dumps(out))
    return len(text)


def rule_dedupe_repeated_strings(text: str) -> int:
    """For JSON arrays of objects that carry a per-item `notes: [str, str, ...]`,
    dedupe strings that repeat verbatim across many items into a common
    legend at the top. `assess_risk_diff` emits things like
    `"hotspot: churn percentile X%"` on many files; a common legend plus
    per-item short codes saves real bytes. Returns original size if the shape
    doesn't match.
    """
    data = _safe_json(text)
    if data is None:
        return len(text)

    def collect_notes(arr: list[Any]) -> tuple[dict[str, int], int]:
        counts: dict[str, int] = defaultdict(int)
        total_items = 0
        for it in arr:
            if not isinstance(it, dict):
                continue
            notes = it.get("notes")
            if not isinstance(notes, list):
                continue
            total_items += 1
            for n in notes:
                if isinstance(n, str):
                    counts[n] += 1
        return counts, total_items

    def apply(arr: list[Any]) -> list[Any] | None:
        counts, total = collect_notes(arr)
        if total < 3:
            return None
        # Only dedupe strings that repeat in ≥50% of items and are ≥30 chars
        legend = [s for s, n in counts.items() if n >= max(2, total // 2) and len(s) >= 30]
        if not legend:
            return None
        codes = {s: f"N{i}" for i, s in enumerate(legend)}
        out: list[Any] = []
        for it in arr:
            if not isinstance(it, dict):
                out.append(it)
                continue
            new = dict(it)
            if isinstance(new.get("notes"), list):
                new["notes"] = [codes.get(n, n) for n in new["notes"]]
            out.append(new)
        return [{"_legend": {v: k for k, v in codes.items()}}, *out]

    if isinstance(data, dict):
        compressed = {}
        touched = False
        for k, v in data.items():
            if isinstance(v, list):
                new = apply(v)
                if new is not None:
                    compressed[k] = new
                    touched = True
                else:
                    compressed[k] = v
            else:
                compressed[k] = v
        if not touched:
            return len(text)
        return len(json.dumps(compressed))
    if isinstance(data, list):
        new = apply(data)
        if new is None:
            return len(text)
        return len(json.dumps(new))
    return len(text)


def rule_collapse_adjacent_refs(text: str) -> int:
    """For JSON arrays of {file, line, ...} rows: collapse consecutive rows
    that share a file and have sequential line numbers into a single row
    `{file, lines: "N-M (count)"}`. Targets `find_references` output where a
    hot symbol shows up on many adjacent lines.
    """
    data = _safe_json(text)
    if not isinstance(data, list) or len(data) < 3:
        return len(text)
    # Items must have file + line
    for item in data:
        if not (isinstance(item, dict) and ("file" in item or "file_path" in item) and "line" in item):
            return len(text)

    def key(item: dict) -> tuple[str, int]:
        return (item.get("file") or item.get("file_path") or "", int(item.get("line") or 0))

    items = sorted(data, key=key)
    compressed: list[Any] = []
    i = 0
    while i < len(items):
        j = i
        f, start_line = key(items[i])
        while j + 1 < len(items):
            fn, ln = key(items[j + 1])
            if fn == f and ln - key(items[j])[1] <= 2:
                j += 1
            else:
                break
        if j > i + 1:  # at least 3 adjacent hits
            compressed.append({
                "file": f,
                "lines": f"{start_line}-{key(items[j])[1]}",
                "count": j - i + 1,
            })
        else:
            compressed.extend(items[i: j + 1])
        i = j + 1
    return len(json.dumps(compressed))


def rule_middle_truncate(text: str, threshold: int = 4096, keep: int = 1024) -> int:
    """For responses >`threshold` bytes, keep the first and last `keep` bytes
    with a `<... N bytes elided ...>` marker in the middle. Targets long
    snippets (`search`, `export_context`).
    """
    n = len(text)
    if n <= threshold:
        return n
    middle_marker = f"<... {n - 2 * keep} bytes elided ...>"
    return keep * 2 + len(middle_marker)


def _safe_json(text: str) -> Any:
    try:
        return json.loads(text)
    except (json.JSONDecodeError, ValueError):
        return None


# -----------------------------------------------------------------------------
# Aggregation
# -----------------------------------------------------------------------------


def summarize(calls: list[dict[str, Any]]) -> dict[str, Any]:
    per_tool: dict[str, list[int]] = defaultdict(list)
    totals = {"calls": 0, "bytes": 0, "errors": 0}
    errors_re = re.compile(r"^(MCP error|Error:|Exit code \d+)")
    compress_savings = {
        "group_by_directory": 0,
        "collapse_adjacent_refs": 0,
        "dedupe_repeated_strings": 0,
        # Structured-best: min across rules that preserve parseable JSON.
        # This is the number the verdict reads — real compressions only.
        "structured_best": 0,
        # Lossy hypothetical cap. Separate from the verdict surface because
        # it would break clients that parse the response.
        "middle_truncate_cap": 0,
    }
    for c in calls:
        tool = c["tool"]
        ob = c["output_bytes"]
        per_tool[tool].append(ob)
        totals["calls"] += 1
        totals["bytes"] += ob
        text = c.get("output_text", "") or ""
        if errors_re.match(text):
            totals["errors"] += 1
            continue  # error responses not subject to compression
        r1 = rule_group_by_directory(text)
        r2 = rule_collapse_adjacent_refs(text)
        r3 = rule_dedupe_repeated_strings(text)
        r_cap = rule_middle_truncate(text)
        compress_savings["group_by_directory"] += max(0, ob - r1)
        compress_savings["collapse_adjacent_refs"] += max(0, ob - r2)
        compress_savings["dedupe_repeated_strings"] += max(0, ob - r3)
        compress_savings["structured_best"] += max(0, ob - min(r1, r2, r3))
        compress_savings["middle_truncate_cap"] += max(0, ob - r_cap)
    return {
        "per_tool": per_tool,
        "totals": totals,
        "compress_savings": compress_savings,
    }


def percentiles(values: list[int]) -> tuple[int, int, int, int]:
    if not values:
        return 0, 0, 0, 0
    s = sorted(values)
    return (
        s[len(s) // 2],
        s[int(len(s) * 0.95)] if len(s) >= 20 else s[-1],
        s[int(len(s) * 0.99)] if len(s) >= 100 else s[-1],
        s[-1],
    )


# -----------------------------------------------------------------------------
# Report
# -----------------------------------------------------------------------------


def render(summary: dict[str, Any], *, window_days: int, transcripts: int, now: str) -> str:
    out: list[str] = []
    per_tool = summary["per_tool"]
    totals = summary["totals"]
    savings = summary["compress_savings"]

    out.append("# CodeSage MCP usage analysis")
    out.append("")
    out.append(f"**Window**: last {window_days} days  ")
    out.append(f"**Transcripts scanned**: {transcripts}  ")
    out.append(f"**Run at**: {now}  ")
    out.append(f"**Calls observed**: {totals['calls']} (error responses: {totals['errors']})  ")
    out.append(f"**Total output bytes**: {totals['bytes']:,} "
               f"(~{totals['bytes'] // 4:,} tokens @ 4 bytes/token)")
    out.append("")
    out.append("Agent-side notice: estimated tokens use the 4-bytes-per-token proxy. "
               "Real tokenization with `tiktoken` would shift numbers by a factor of "
               "~0.85–1.2x depending on content; treat the ratios as informative, not exact.")
    out.append("")

    out.append("## Per-tool breakdown")
    out.append("")
    out.append("| tool | calls | total bytes | p50 | p95 | p99 | max |")
    out.append("|---|---:|---:|---:|---:|---:|---:|")
    # Sort by total bytes descending — compression effort should follow the cost center.
    tools_by_cost = sorted(
        per_tool.items(), key=lambda kv: sum(kv[1]), reverse=True
    )
    for tool, sizes in tools_by_cost:
        p50, p95, p99, mx = percentiles(sizes)
        total = sum(sizes)
        out.append(
            f"| `{tool}` | {len(sizes)} | {total:,} | {p50:,} | {p95:,} | {p99:,} | {mx:,} |"
        )
    out.append("")

    out.append("## Simulated compression savings")
    out.append("")
    out.append("Each rule re-computes the response size *as if* the rule were applied; "
               "original on-disk / on-wire output is untouched. `structured_best` is the "
               "per-call minimum across rules that preserve parseable JSON (this is the "
               "number the verdict reads). `middle_truncate_cap` is a lossy hypothetical "
               "that would break clients — shown as an upper bound, not a real option.")
    out.append("")
    out.append("| rule | lossy? | bytes saved | % of total codesage output |")
    out.append("|---|:---:|---:|---:|")
    compressible_bytes = totals["bytes"]
    lossy = {"middle_truncate_cap"}
    for rule, saved in savings.items():
        pct = 100.0 * saved / compressible_bytes if compressible_bytes else 0.0
        mark = "yes" if rule in lossy else ""
        out.append(f"| `{rule}` | {mark} | {saved:,} | {pct:.1f}% |")
    out.append("")

    # Verdict logic. The only numbers that matter are the combined-best ceiling
    # and the per-tool concentration: if >80% of bytes live in one tool, target
    # that tool specifically; otherwise a horizontal compression layer.
    top_tool_total = sum(tools_by_cost[0][1]) if tools_by_cost else 0
    top_tool_share = (100.0 * top_tool_total / compressible_bytes) if compressible_bytes else 0.0
    combined_pct = (100.0 * savings["structured_best"] / compressible_bytes) if compressible_bytes else 0.0

    out.append("## Verdict")
    out.append("")
    if totals["calls"] < 20:
        out.append(f"**Inconclusive.** Only {totals['calls']} codesage calls in the window. "
                   "Widen `--window-days`, or shelf item 1.5 until the user pattern produces "
                   "more data.")
    elif combined_pct < 10.0:
        out.append(f"**Shelf item 1.5.** Structured-best compression ceiling is "
                   f"{combined_pct:.1f}% of {compressible_bytes:,} codesage output bytes "
                   f"(~{compressible_bytes // 4:,} tokens). That is well below the threshold "
                   "where output shaping would earn its engineering cost. Close the item with "
                   "`[rejected YYYY-MM-DD — retrospective showed <10% ceiling]` in the "
                   "recommendations doc.")
    elif combined_pct < 25.0:
        out.append(f"**Marginal.** Structured-best compression ceiling is {combined_pct:.1f}%. "
                   "Not clearly worth horizontal output shaping, but if one tool dominates "
                   "(see top-tool share below), targeted compression on that tool could "
                   "be worth a small slice.")
    else:
        out.append(f"**Promote to a targeted compression slice.** Structured-best "
                   f"compression ceiling is {combined_pct:.1f}% of codesage output — real, "
                   "non-lossy savings that preserve parseable JSON. Proceed to ship "
                   "compression on the top cost-center tool below, not a horizontal layer. "
                   "Add a `bytes_out` log line on MCP responses in parallel to verify the "
                   "retrospective number against live sessions before expanding.")
    out.append("")
    out.append(f"Top cost-center tool: `{tools_by_cost[0][0] if tools_by_cost else 'n/a'}` "
               f"({top_tool_share:.1f}% of total output bytes).")
    if top_tool_share > 80.0 and tools_by_cost:
        out.append("")
        out.append(f"**Targeted follow-up**: more than 80% of output bytes live in one tool. "
                   f"Any compression work should start there, not as a horizontal output layer.")
    out.append("")
    return "\n".join(out)


# -----------------------------------------------------------------------------
# CLI
# -----------------------------------------------------------------------------


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--window-days", type=int, default=7,
                    help="Only scan transcripts modified within the last N days (default 7).")
    ap.add_argument(
        "--projects-root",
        type=Path,
        default=Path.home() / ".claude" / "projects",
        help="Override the Claude Code project-logs root.",
    )
    ap.add_argument(
        "--output",
        type=Path,
        default=None,
        help="Write the scorecard here instead of stdout. Parent dirs created if needed.",
    )
    args = ap.parse_args()

    min_mtime = (_dt.datetime.now() - _dt.timedelta(days=args.window_days)).timestamp()
    transcripts = iter_transcripts(args.projects_root, min_mtime)
    all_calls: list[dict[str, Any]] = []
    for t in transcripts:
        all_calls.extend(extract_calls(t))

    summary = summarize(all_calls)
    now = _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    report = render(summary, window_days=args.window_days, transcripts=len(transcripts), now=now)

    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(report, encoding="utf-8")
        print(f"wrote {args.output}", file=sys.stderr)
    else:
        sys.stdout.write(report)
    return 0


if __name__ == "__main__":
    sys.exit(main())
