#!/usr/bin/env python3
"""Regression tests for the bench-script review fixes (June 2026).

Usage:
  python3 bench/test_bench_fixes.py

Bare-assert style — no pytest dependency. Exits 0 on success, 1 on failure.
The scripts have hyphens in their names so they're loaded via importlib.

Each block guards a finding from the /codesage-review run; the comment names
the finding id. A test earns its keep by failing against the pre-fix code.
"""

from __future__ import annotations

import importlib.util
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
failures: list[str] = []


def _load(filename: str, modname: str):
    spec = importlib.util.spec_from_file_location(modname, HERE / filename)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def check(cond: bool, label: str) -> None:
    if not cond:
        failures.append(f"  {label}")


# --------------------------------------------------------------------
# agent-tool-selection-harness.py — fnd_178331ec / fnd_ac7bcade
# The harness must not pass --dangerously-skip-permissions (which would let
# the model use any tool regardless of --allowedTools), must disallow
# exec/write tools, and must strip the global MCP config in the "without" arm.
# --------------------------------------------------------------------
harness = _load("agent-tool-selection-harness.py", "harness")

with_cmd = harness.build_command("find the auth handler", with_codesage=True, max_turns=10)
without_cmd = harness.build_command("find the auth handler", with_codesage=False, max_turns=10)

check(
    "--dangerously-skip-permissions" not in with_cmd,
    "harness: with-arm must NOT skip permissions",
)
check(
    "--dangerously-skip-permissions" not in without_cmd,
    "harness: without-arm must NOT skip permissions",
)
check("--disallowedTools" in with_cmd, "harness: with-arm disallows exec/write tools")
check("--disallowedTools" in without_cmd, "harness: without-arm disallows exec/write tools")
check(
    "--strict-mcp-config" in without_cmd,
    "harness: without-arm strips global MCP config",
)
check(
    "--strict-mcp-config" not in with_cmd,
    "harness: with-arm keeps the global MCP config (codesage available)",
)
# The with-arm allow-list contains the codesage tools; the without-arm does not.
with_allow = with_cmd[with_cmd.index("--allowedTools") + 1]
without_allow = without_cmd[without_cmd.index("--allowedTools") + 1]
check("mcp__codesage__search" in with_allow, "harness: with-arm allow-lists codesage tools")
check(
    "mcp__codesage__search" not in without_allow,
    "harness: without-arm does not allow-list codesage tools",
)
check(
    harness.expected_tool_set(False) == {"Grep", "Read", "Glob"},
    "harness: without-arm expected tool set is base only",
)

# --------------------------------------------------------------------
# generate-llm-corpus.py — fnd_a15786db (negative --num-cases) +
# fnd_136d5663 (unsorted candidates) + YAML quoting hardening
# --------------------------------------------------------------------
gen = _load("generate-llm-corpus.py", "gen")

raised = False
try:
    gen.positive_int("-5")
except Exception:
    raised = True
check(raised, "gen: positive_int rejects -5")

raised = False
try:
    gen.positive_int("0")
except Exception:
    raised = True
check(raised, "gen: positive_int rejects 0")
check(gen.positive_int("3") == 3, "gen: positive_int accepts 3")

with tempfile.TemporaryDirectory() as td:
    root = Path(td)
    body = "\n".join(f"# line {i}" for i in range(40)) + "\n"
    for name in ("zeta.py", "alpha.py", "middle.py"):
        (root / name).write_text(body)
    cands = gen.candidate_files(root)
    check(cands == sorted(cands), "gen: candidate_files returns a sorted list")

check(gen.yaml_sq("a: b # c") == "'a: b # c'", "gen: yaml_sq quotes metacharacters")
check(gen.yaml_sq("it's") == "'it''s'", "gen: yaml_sq doubles single quotes")

# --------------------------------------------------------------------
# extract-eval-cases.py — fnd_32ebb1d9 (YAML injection via path)
# --------------------------------------------------------------------
extract = _load("extract-eval-cases.py", "extract")

with tempfile.TemporaryDirectory() as td:
    root = Path(td)
    # A normal path normalizes; a newline-bearing path is rejected so it can
    # never reach the YAML writer.
    (root / "real.py").write_text("x = 1\n")
    check(
        extract.normalize_path(str(root / "real.py"), str(root)) == "real.py",
        "extract: normal path normalizes",
    )
    check(
        extract.normalize_path(str(root) + "/a\n  - evil.py", str(root)) is None,
        "extract: newline-bearing path is rejected",
    )

check(
    extract._yaml_dq('a: b') == '"a: b"',
    "extract: _yaml_dq wraps in double quotes",
)
check(
    extract._yaml_dq('say "hi"') == '"say \\"hi\\""',
    "extract: _yaml_dq escapes embedded double quotes",
)

# The emitted YAML for a path with a metacharacter round-trips as a string,
# not injected structure.
with tempfile.TemporaryDirectory() as td:
    out = Path(td) / "corpus.yaml"
    extract.write_yaml(
        [{"query": "find the thing", "files": ["src/a: b.py"]}],
        "/proj",
        "proj",
        out,
    )
    text = out.read_text()
    check('"src/a: b.py"' in text, "extract: path with ': ' is double-quoted in output")

# --------------------------------------------------------------------
# concurrency-audit.py — fnd_75310863 (timeout reported as clean)
# --------------------------------------------------------------------
audit = _load("concurrency-audit.py", "audit")

clean_state = {
    "integrity": "ok",
    "orphans": {"symbols_without_file": 0, "refs_without_file": 0},
    "dupes": {"files_same_path": 0},
    "schema_migrations": [],
    "foreign_key_violations": [],
}
# One success + one timeout (returncode None) must be a TIMEOUT verdict, not
# "serialized" — the pre-fix bug gave this a green checkmark.
v = audit.classify_verdict(
    [{"returncode": 0}, {"returncode": None}], clean_state
)
check("TIMEOUT" in v, f"audit: success+timeout is a TIMEOUT verdict (got {v!r})")
# One success + one clean error stays "serialized".
v = audit.classify_verdict([{"returncode": 0}, {"returncode": 1}], clean_state)
check("serialized" in v, f"audit: success+error is serialized (got {v!r})")
# Both succeed → clean.
v = audit.classify_verdict([{"returncode": 0}, {"returncode": 0}], clean_state)
check("clean" in v, f"audit: both-ok is clean (got {v!r})")
# Corruption wins over everything, even a timeout.
corrupt_state = dict(clean_state, integrity="row 5 missing")
v = audit.classify_verdict([{"returncode": 0}, {"returncode": None}], corrupt_state)
check("CORRUPT" in v, f"audit: corruption wins over timeout (got {v!r})")


if failures:
    print(f"FAILED ({len(failures)}):")
    for f in failures:
        print(f)
    sys.exit(1)
print("all bench-fix regression tests passed")
sys.exit(0)
