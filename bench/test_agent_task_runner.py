#!/usr/bin/env python3
"""Tests for bench/agent-task-runner (recommendations doc §2.14(2)).

Usage:
  python3 bench/test_agent_task_runner.py

Bare-assert style — no pytest dependency. Exits 0 on success, 1 on failure.
The runner has a hyphen and no extension, so it's loaded via SourceFileLoader
(spec_from_file_location returns no loader for extensionless files).

Covers the pure logic that has no business touching `claude`:
  - stream-json parsing (token summation across turns, cost from the result
    event, tool-call classification, cold-start race detection)
  - saved% math incl. the divide-by-zero guard
  - median-of-N aggregation
  - scorecard rendering (sections, nested stamp, honest model caveat)
The actual `claude -p` subprocess and the non-nested guard are integration
concerns exercised by a real (non-nested, budgeted) run, not here.
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
failures: list[str] = []


def _load(filename: str, modname: str):
    loader = importlib.machinery.SourceFileLoader(modname, str(HERE / filename))
    spec = importlib.util.spec_from_loader(modname, loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


def check(cond: bool, label: str) -> None:
    if not cond:
        failures.append(f"  {label}")


def approx(a: float, b: float, tol: float = 1e-9) -> bool:
    return abs(a - b) < tol


m = _load("agent-task-runner", "agent_task_runner")


# --- saved% math ------------------------------------------------------------
check(approx(m.saved_pct(80, 100), 20.0), "saved_pct: 20% cheaper -> +20")
check(approx(m.saved_pct(120, 100), -20.0), "saved_pct: 20% dearer -> -20 (honest)")
check(m.saved_pct(5, 0) is None, "saved_pct: zero baseline -> None (no divide-by-zero)")
check(m.saved_pct(0, 0) is None, "saved_pct: 0/0 -> None")

# --- median-of-N ------------------------------------------------------------
check(m.median_of([{"x": 1.0}, {"x": 3.0}, {"x": 2.0}], "x") == 2.0, "median_of: odd")
check(m.median_of([{"x": 1.0}, {"x": 3.0}], "x") == 2.0, "median_of: even -> mean of mid")
check(m.median_of([], "x") == 0.0, "median_of: empty -> 0.0")


# --- stream-json parsing ----------------------------------------------------
def _stream(*events: dict) -> str:
    return "\n".join(json.dumps(e) for e in events)


good = _stream(
    {"type": "assistant", "message": {
        "content": [{"type": "tool_use", "name": "mcp__codesage__search"},
                    {"type": "tool_use", "name": "Read"}],
        "usage": {"input_tokens": 100, "output_tokens": 20,
                  "cache_read_input_tokens": 5000, "cache_creation_input_tokens": 0}}},
    {"type": "assistant", "message": {
        "content": [{"type": "tool_use", "name": "Grep"}],
        "usage": {"input_tokens": 30, "output_tokens": 10}}},
    {"type": "result", "subtype": "success", "total_cost_usd": 0.1234,
     "result": "src/foo.rs", "is_error": False},
)
p = m.parse_run_output(good, "", 0, 1.5, with_codesage=True)
check(p["error"] is None, "parse: clean run has no error")
check(p["tokens"] == 100 + 20 + 5000 + 30 + 10, "parse: tokens summed across turns incl. cache")
check(approx(p["cost_usd"], 0.1234), "parse: cost from result event")
check(p["tool_calls"] == 3, "parse: counts all tool_use blocks")
check(p["codesage_calls"] == 1, "parse: classifies codesage calls")
check(p["grep_calls"] == 1, "parse: Grep/Glob bucket")
check(p["read_calls"] == 1, "parse: Read bucket")
check(p["result_text"] == "src/foo.rs", "parse: final answer text")
check(p["raced"] is False, "parse: no race markers -> not raced")

# Cold-start race: WITH arm, server not ready -> excluded-class signal.
raced = m.parse_run_output(
    _stream({"type": "result", "subtype": "success", "total_cost_usd": 0.01, "result": ""}),
    "Error: No such tool available: mcp__codesage__search", 0, 0.5, with_codesage=True,
)
check(raced["raced"] is True, "parse: race marker in stderr flags WITH run as raced")
# The same marker in the WITHOUT arm is NOT a race (no codesage server expected).
not_raced = m.parse_run_output("", "No such tool available", 0, 0.5, with_codesage=False)
check(not_raced["raced"] is False, "parse: WITHOUT arm never flagged raced")
# A generic (non-codesage) tool denial in the WITH arm — e.g. the agent tries
# the DISALLOWED Bash to run shell grep on a grep-hostile task — is NOT a
# codesage cold-start race; the agent falls back to Grep/Read and the run is
# valid. Over-matching here silently discarded good data on the first opus run.
bash_denied = m.parse_run_output("", "No such tool available: Bash", 0, 0.5, with_codesage=True)
check(bash_denied["raced"] is False,
      "parse: generic non-codesage tool denial in WITH arm is NOT raced")
check("Bash" in bash_denied["diag"],
      "parse: diag captures the failing line so raced/errored runs are diagnosable")
check(raced["diag"] != "", "parse: a real codesage race carries a diag line")

# Non-zero rc and result-level error both surface as errors.
check(m.parse_run_output("", "boom", 1, 0.1, with_codesage=False)["error"] == "rc=1",
      "parse: non-zero returncode -> error")
err_result = _stream({"type": "result", "subtype": "error_max_turns",
                      "total_cost_usd": 0.2, "result": "", "is_error": True})
check(m.parse_run_output(err_result, "", 0, 0.1, with_codesage=True)["error"] == "result_error",
      "parse: result is_error/subtype -> error")
# Garbage lines between events don't crash the parser.
noisy = "debug: starting\n" + good + "\nnot json\n{bad"
check(m.parse_run_output(noisy, "", 0, 1.0, with_codesage=True)["tool_calls"] == 3,
      "parse: tolerates non-JSON noise lines")


# --- scorecard rendering ----------------------------------------------------
def _arm(cost, tok, tools, dur, used=3, cs=2, cs_used=None):
    return dict(arm="with", runs_total=3, runs_used=used,
                cs_used_runs=used if cs_used is None else cs_used,
                raced=0, errored=0,
                median_cost=cost, median_tokens=tok, median_tools=tools,
                median_duration=dur, median_codesage_calls=cs, result_texts=[])


rows = [
    {"id": "task-a", "with": _arm(0.18, 120000, 4, 8), "without": _arm(0.24, 150000, 6, 11)},
    {"id": "task-b", "with": _arm(0.30, 200000, 5, 14), "without": _arm(0.28, 190000, 5, 12)},
]
sc = m.render_scorecard(project_root="/repo", corpus_name="x.yaml", rows=rows,
                        run_ts="2026-06-20T00:00:00Z", runs=3, model="sonnet",
                        effort="high", nested=True, include_raced=False,
                        codesage_version="codesage 0.12.0")
for needle in ("Cost saved", "Tokens saved", "Tool-calls saved", "Wall-clock saved",
               "task-a", "NESTED RUN", "model-relative", "$0.180→$0.240 (+25%)"):
    check(needle in sc, f"render: contains {needle!r}")
# Overall is the MEDIAN of per-task saved%, so task-b's regression isn't hidden.
check("**Cost saved**: +9%" in sc, "render: overall cost = median(+25,-7)=+9 (honest)")
# A normal (CodeSage-used) run must NOT be stamped invalid, and reports usage.
check("INVALID" not in sc, "render: cs-used run is not stamped invalid")
check("CodeSage used in WITH arm" in sc, "render: overall reports cs-usage line")

# --- cs-usage validity gate -------------------------------------------------
# A WITH arm that never called CodeSage is an A/A comparison; the report MUST
# stamp INVALID rather than present the variance as savings.
unused_rows = [
    {"id": "t", "with": _arm(0.18, 120000, 4, 8, cs=0, cs_used=0),
     "without": _arm(0.24, 150000, 6, 11)},
]
sc_unused = m.render_scorecard(project_root="/repo", corpus_name="x.yaml", rows=unused_rows,
                               run_ts="2026-06-20T00:00:00Z", runs=3, model="sonnet",
                               effort="high", nested=False, include_raced=False,
                               codesage_version="codesage 0.15.0")
check("INVALID" in sc_unused, "render: cs-usage 0 stamps INVALID banner (A/A detected)")
check("0/3 WITH runs" in sc_unused, "render: banner reports the cs-usage count")

# --- steering ---------------------------------------------------------------
check("mcp__codesage" not in m.build_prompt("q"),
      "prompt: neutral by default (no codesage nudge)")
check("mcp__codesage" in m.build_prompt("q", steer_codesage=True),
      "prompt: --steer nudges the WITH arm toward codesage")
check("steering**: off" in sc, "render: default report marks steering off (merit test)")
sc_steer = m.render_scorecard(project_root="/repo", corpus_name="x.yaml", rows=rows,
                              run_ts="2026-06-20T00:00:00Z", runs=3, model="opus",
                              effort="high", nested=False, include_raced=False,
                              codesage_version="codesage 0.15.0", steer=True)
check("steering**: **ON**" in sc_steer, "render: steer=True stamps steering ON (value-when-used test)")

if failures:
    print(f"FAIL ({len(failures)}):")
    print("\n".join(failures))
    sys.exit(1)
print("agent-task-runner: all checks passed")
sys.exit(0)
