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
import json
import os
import sqlite3
import sys
import tempfile
import time
from pathlib import Path

try:
    import yaml
except ImportError:
    yaml = None

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

class _HarnessRun:
    returncode = 0
    stderr = ""
    stdout = "\n".join(
        [
            json.dumps(
                {
                    "type": "assistant",
                    "message": {
                        "content": [
                            {"type": "tool_use", "name": "mcp__codesage__search"}
                        ]
                    },
                }
            ),
            json.dumps({"type": "result", "result": "src/main.py", "total_cost_usd": 0.01}),
        ]
    )


orig_harness_run = harness.subprocess.run
harness.subprocess.run = lambda *args, **kwargs: _HarnessRun()
try:
    result = harness.run_task(Path("."), "find main", with_codesage=False, max_turns=1)
finally:
    harness.subprocess.run = orig_harness_run
check(
    result["error"],
    "harness: unexpected tool use invalidates an otherwise successful run",
)
check(
    result["unexpected_tools"] == ["mcp__codesage__search"],
    "harness: unexpected tool use is reported",
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

with tempfile.TemporaryDirectory() as td:
    root = Path(td) / "project"
    root.mkdir()
    outside = Path(td) / "secret.py"
    outside.write_text("\n".join(f"# outside {i}" for i in range(40)) + "\n")
    (root / "leak.py").symlink_to(outside)
    cands = [p.name for p in gen.candidate_files(root)]
    check("leak.py" not in cands, "gen: candidate_files rejects symlinked source files")

check(gen.yaml_sq("a: b # c") == "'a: b # c'", "gen: yaml_sq quotes metacharacters")
check(gen.yaml_sq("it's") == "'it''s'", "gen: yaml_sq doubles single quotes")

orig_gen_run = gen.subprocess.run
captured_gen_call: dict[str, object] = {}


def _capture_gen_run(cmd, **kwargs):
    captured_gen_call["cmd"] = cmd
    captured_gen_call.update(kwargs)

    class _Run:
        returncode = 0
        stdout = "A focused query about this file\n"
        stderr = ""

    return _Run()


with tempfile.TemporaryDirectory() as td:
    root = Path(td)
    source = root / "example.py"
    source.write_text("\n".join(f"# line {i}" for i in range(40)) + "\n")
    old_aws_secret = os.environ.get("AWS_SECRET_ACCESS_KEY")
    os.environ["AWS_SECRET_ACCESS_KEY"] = "must-not-leak"
    gen.subprocess.run = _capture_gen_run
    try:
        gen.generate_query(source, root)
    finally:
        gen.subprocess.run = orig_gen_run
        if old_aws_secret is None:
            os.environ.pop("AWS_SECRET_ACCESS_KEY", None)
        else:
            os.environ["AWS_SECRET_ACCESS_KEY"] = old_aws_secret
cmd = captured_gen_call.get("cmd") or []
env = captured_gen_call.get("env") or {}
check("--ignore-user-config" in cmd, "gen: codex ignores user config")
check("--ignore-rules" in cmd, "gen: codex ignores project/user rules")
check("--ephemeral" in cmd, "gen: codex uses ephemeral session storage")
check("--cd" in cmd, "gen: codex runs from an isolated working directory")
check("--output-schema" in cmd, "gen: codex constrains the final output schema")
check(
    "env" in captured_gen_call and "AWS_SECRET_ACCESS_KEY" not in env,
    "gen: codex environment is scrubbed",
)

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

with tempfile.TemporaryDirectory() as td:
    root = Path(td) / "project"
    sessions = Path(td) / "sessions"
    root.mkdir()
    sessions.mkdir()
    (root / "Cargo.toml").write_text("[package]\nname = 'demo'\n")
    session = sessions / "root-file.jsonl"
    session.write_text(
        "\n".join(
            [
                json.dumps(
                    {
                        "type": "user",
                        "message": {
                            "content": "where is the cargo manifest configured for this project"
                        },
                        "padding": "x" * 3500,
                    }
                ),
                json.dumps(
                    {
                        "type": "assistant",
                        "message": {
                            "content": [
                                {
                                    "type": "tool_use",
                                    "input": {"file_path": str(root / "Cargo.toml")},
                                }
                            ]
                        },
                    }
                ),
            ]
        )
        + "\n"
    )
    cases = extract.extract_cases(sessions, str(root), min_files=1, max_cases=1)
    check(
        cases and cases[0]["files"] == ["Cargo.toml"],
        f"extract: root-level allowed files are retained in cases (got {cases!r})",
    )

with tempfile.TemporaryDirectory() as td:
    root = Path(td) / "project"
    sessions = Path(td) / "sessions"
    root.mkdir()
    sessions.mkdir()
    (root / "Cargo.toml").write_text("[package]\nname = 'demo'\n")
    broken = sessions / "broken.jsonl"
    try:
        broken.symlink_to(sessions / "missing.jsonl")
    except OSError:
        broken = None
    session = sessions / "valid.jsonl"
    session.write_text(
        "\n".join(
            [
                json.dumps(
                    {
                        "type": "user",
                        "message": {
                            "content": "where is the cargo manifest configured for this project"
                        },
                        "padding": "x" * 3500,
                    }
                ),
                json.dumps(
                    {
                        "type": "assistant",
                        "message": {
                            "content": [
                                {
                                    "type": "tool_use",
                                    "input": {"file_path": str(root / "Cargo.toml")},
                                }
                            ]
                        },
                    }
                ),
            ]
        )
        + "\n"
    )
    cases = extract.extract_cases(sessions, str(root), min_files=1, max_cases=1)
    check(
        cases and cases[0]["files"] == ["Cargo.toml"],
        f"extract: broken session symlinks are skipped (got {cases!r}, broken={broken})",
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
# One success + one lock error stays "serialized".
v = audit.classify_verdict(
    [{"returncode": 0}, {"returncode": 1, "stderr_tail": "database is locked"}],
    clean_state,
)
check("serialized" in v, f"audit: success+lock-error is serialized (got {v!r})")
# Both succeed → clean.
v = audit.classify_verdict([{"returncode": 0}, {"returncode": 0}], clean_state)
check("clean" in v, f"audit: both-ok is clean (got {v!r})")
# Corruption wins over everything, even a timeout.
corrupt_state = dict(clean_state, integrity="row 5 missing")
v = audit.classify_verdict([{"returncode": 0}, {"returncode": None}], corrupt_state)
check("CORRUPT" in v, f"audit: corruption wins over timeout (got {v!r})")

with tempfile.TemporaryDirectory() as td:
    codesage_dir = Path(td)
    for name in ("index.db", "index.db-wal", "index.db-shm"):
        (codesage_dir / name).write_text("audit-created")
    audit.restore_db(codesage_dir, None)
    check(
        not any((codesage_dir / name).exists() for name in ("index.db", "index.db-wal", "index.db-shm")),
        "audit: restore_db removes audit-created database files when there was no original backup",
    )

# --------------------------------------------------------------------
# Round-2 findings
# --------------------------------------------------------------------

# extract-eval-cases.py — fnd_f243e0e9: control chars in query text must not
# survive into emitted YAML (PyYAML rejects raw C0 control chars even in quotes).
dq = extract._yaml_dq("hello\x1b[31mworld\x00")
check("\x1b" not in dq and "\x00" not in dq, "extract: _yaml_dq strips control chars")
if yaml is not None:
    with tempfile.TemporaryDirectory() as td:
        out = Path(td) / "corpus.yaml"
        extract.write_yaml(
            [{"query": "color \x1b[31mred\x1b[0m output", "files": ["src/a.py"]}],
            "/proj",
            "proj",
            out,
        )
        try:
            loaded = yaml.safe_load(out.read_text())  # pre-fix: raises ReaderError
            ok = loaded["cases"][0]["query"] == "color [31mred[0m output"
        except yaml.YAMLError:
            ok = False
        check(ok, "extract: control-char query round-trips through YAML")

# generate-llm-corpus.py — fnd_4c49b101: the id field embeds the filename stem
# and must be quoted so a stem with ': ' doesn't break the corpus parse.
if yaml is not None:
    doc = gen.format_corpus_yaml(
        "/proj",
        [{"id": "llm-001-foo: bar", "query": "q", "expected_files": ["src/a.py"]}],
    )
    try:
        loaded = yaml.safe_load(doc)  # pre-fix: 'mapping values are not allowed here'
        ok = loaded["cases"][0]["id"] == "llm-001-foo: bar"
    except yaml.YAMLError:
        ok = False
    check(ok, "gen: id with ': ' round-trips through YAML")

# concurrency-audit.py — fnd_c8431abd: two hanging children must be bounded to
# ONE timeout window (shared deadline), not N. Pre-fix each got a fresh timeout
# so two hangs took ~2x. (Also exercises file-redirect, not PIPE.)
hang = [sys.executable, "-c", "import time; time.sleep(30)"]
t0 = time.time()
results = audit.run_parallel([hang, hang], Path("."), timeout_s=2)
elapsed = time.time() - t0
check(
    elapsed < 3.5,
    f"audit: two hanging children bounded to ~1 timeout window, took {elapsed:.1f}s",
)
check(
    all(r["returncode"] is None for r in results),
    "audit: both hanging children marked timed-out",
)

with tempfile.TemporaryDirectory() as td:
    codesage_dir = Path(td)
    db_path = codesage_dir / "index.db"
    conn = sqlite3.connect(db_path)
    conn.execute("CREATE TABLE t(x)")
    conn.commit()
    conn.close()

    first = audit.backup_db(codesage_dir)
    second = audit.backup_db(codesage_dir)
    check(first is not None and second is not None, "audit: backup_db returns backup paths")
    check(first != second, "audit: backup_db creates unique paths for repeated calls")
    check(first.exists() and second.exists(), "audit: unique backup files exist")

# agent-tool-selection-harness.py — fnd_329c8aed / fnd_42e0caa2
raised = False
try:
    harness.positive_int("-1")
except Exception:
    raised = True
check(raised, "harness: positive_int rejects -1")

raised = False
try:
    harness.positive_int("0")
except Exception:
    raised = True
check(raised, "harness: positive_int rejects 0")
check(harness.positive_int("2") == 2, "harness: positive_int accepts 2")

raised = False
try:
    harness.score_task({"error": "timeout", "result_text": "src/main.py"}, ["src/main.py"])
except ValueError:
    raised = True
check(raised, "harness: failed subprocess results cannot be scored")


if failures:
    print(f"FAILED ({len(failures)}):")
    for f in failures:
        print(f)
    sys.exit(1)
print("all bench-fix regression tests passed")
sys.exit(0)
