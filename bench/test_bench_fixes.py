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

import importlib.machinery
import importlib.util
import json
import os
import re
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


# Skipping permissions would bypass --allowedTools and contaminate the comparison.
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

with tempfile.TemporaryDirectory() as td:
    root = Path(td)
    body = "\n".join(f"# line {i}" for i in range(40)) + "\n"
    safe = root / "safe.py"
    unsafe = root / "bad\nname.py"
    safe.write_text(body)
    unsafe.write_text(body)
    cands = [p.name for p in gen.candidate_files(root)]
    check("safe.py" in cands, "gen: candidate_files keeps normal source paths")
    check(
        "bad\nname.py" not in cands,
        "gen: candidate_files rejects control-character paths",
    )

raised = False
try:
    gen.format_corpus_yaml(
        "/proj",
        [{"id": "case", "query": "q", "expected_files": ["bad\nname.py"]}],
    )
except ValueError:
    raised = True
check(raised, "gen: format_corpus_yaml rejects control-character expected paths")

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
check("--disable" in cmd and "shell_tool" in cmd, "gen: codex disables shell tools")
check("-c" in cmd and 'web_search="disabled"' in cmd, "gen: codex disables web search")
check("--cd" in cmd, "gen: codex runs from an isolated working directory")
check("--output-schema" in cmd, "gen: codex constrains the final output schema")
check(
    "env" in captured_gen_call and "AWS_SECRET_ACCESS_KEY" not in env,
    "gen: codex environment is scrubbed",
)
check(
    "OPENAI_API_KEY" not in env,
    "gen: OPENAI_API_KEY is not passed to nested codex",
)
check(
    "--output-last-message" in cmd,
    "gen: codex writes structured last message to a file",
)

with tempfile.TemporaryDirectory() as td:
    out_file = Path(td) / "last.txt"
    out_file.write_text('"find the auth middleware handler"\n', encoding="utf-8")
    noisy_stdout = "\n".join(
        [
            "session id: abc",
            "tokens used",
            "12,345",
            "role: assistant",
            "garbage metadata line",
        ]
    )
    parsed = gen.parse_codex_query_output(noisy_stdout, out_file)
    check(
        parsed == "find the auth middleware handler",
        f"gen: parse_codex_query_output reads schema file, got {parsed!r}",
    )

extract = _load("extract-eval-cases.py", "extract")


def _tool_result(path: Path) -> str:
    return json.dumps({"type": "user", "toolUseResult": {"file_path": str(path)}})

with tempfile.TemporaryDirectory() as td:
    root = Path(td)
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
                _tool_result(root / "Cargo.toml"),
            ]
        )
        + "\n"
    )
    cases = extract.extract_cases(sessions, str(root), min_files=1, max_cases=1)
    check(
        cases and cases[0]["files"] == ["Cargo.toml"],
        f"extract: root-level allowed files are retained in cases (got {cases!r})",
    )

# Exclude CodeSage-derived cases so the benchmark does not grade its own retrieval.
# Naming the root `codesage` tests that cd/git prefixes do not count as CodeSage calls.
with tempfile.TemporaryDirectory() as td:
    root = Path(td) / "codesage"
    sessions = Path(td) / "sessions"
    root.mkdir()
    sessions.mkdir()
    for name in ("a", "b", "c", "d", "e"):
        (root / f"{name}.rs").write_text(f"fn {name}() {{}}\n")

    def _turn(query: str, tool_name: str, tool_input: dict, path: Path) -> list[str]:
        return [
            json.dumps({"type": "user", "message": {"content": query}, "padding": "x" * 3500}),
            json.dumps(
                {
                    "type": "assistant",
                    "message": {"content": [{"type": "tool_use", "name": tool_name, "input": tool_input}]},
                }
            ),
            _tool_result(path),
        ]

    lines = (
        _turn("where is the a handler wired up in this crate", "mcp__codesage__search",
              {"project": str(root), "query": "a handler"}, root / "a.rs")
        + _turn("where is the b handler wired up in this crate", "Bash",
                {"command": "codesage risk b.rs"}, root / "b.rs")
        + _turn("where is the c handler wired up in this crate", "Bash",
                {"command": f"cd {root}; grep -rn c ."}, root / "c.rs")
        + _turn("where is the d handler wired up in this crate", "Bash",
                {"command": f"cd {root}\ncargo build"}, root / "d.rs")
        + _turn("where is the e handler wired up in this crate", "Bash",
                {"command": f"git -C {root} log --oneline"}, root / "e.rs")
    )
    (sessions / "mixed.jsonl").write_text("\n".join(lines) + "\n")

    stats: dict = {}
    cases = extract.extract_cases(sessions, str(root), min_files=1, max_cases=10, stats=stats)
    kept_files = sorted(f for c in cases for f in c["files"])
    check(
        kept_files == ["c.rs", "d.rs", "e.rs"],
        f"extract: codesage-used windows are excluded by default (got {cases!r})",
    )
    check(
        stats.get("excluded_cases") == 2 and stats.get("codesage_sessions") == 1,
        f"extract: contamination stats count excluded windows and sessions (got {stats!r})",
    )
    kept = extract.extract_cases(
        sessions, str(root), min_files=1, max_cases=10, include_codesage=True
    )
    tagged = sorted((c["files"][0], bool(c.get("codesage_used"))) for c in kept)
    check(
        tagged == [("a.rs", True), ("b.rs", True), ("c.rs", False), ("d.rs", False), ("e.rs", False)],
        f"extract: --include-codesage-sessions keeps and tags contaminated cases (got {tagged!r})",
    )

# Compare against clap's enum so new subcommands cannot evade contamination detection.
_VARIANT_RE = re.compile(r"^\s*([A-Z][A-Za-z0-9]*)\s*(?:\{|\(|,)")


def _kebab(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "-", name).lower()


def commands_enum_variants(main_rs: Path) -> set[str]:
    lines = main_rs.read_text(encoding="utf-8").splitlines()
    start = next(i for i, line in enumerate(lines) if line.strip() == "enum Commands {")
    variants: set[str] = set()
    depth = 0
    for offset, line in enumerate(lines[start:]):
        code = line.split("//", 1)[0]
        # Braces inside help strings must not move the depth counter.
        code = re.sub(r'"(?:\\.|[^"\\])*"', '""', code)
        if offset and depth == 1:
            m = _VARIANT_RE.match(code)
            if m:
                variants.add(_kebab(m.group(1)))
        depth += code.count("{") - code.count("}")
        if offset and depth == 0:
            break
    return variants


_main_rs = HERE.parent / "crates/cli/src/main.rs"
_variants = commands_enum_variants(_main_rs)
check(len(_variants) >= 30, f"extract: Commands enum parse found only {len(_variants)} variants")
check(
    _variants == set(extract.CODESAGE_SUBCOMMANDS),
    "extract: CODESAGE_SUBCOMMANDS drifted from the Commands enum "
    f"(missing from set: {sorted(_variants - extract.CODESAGE_SUBCOMMANDS)}, "
    f"stale in set: {sorted(extract.CODESAGE_SUBCOMMANDS - _variants)})",
)

# CODESAGE_BASH_RE: the token after `codesage` (or after `-- `) must be a real
# subcommand, on the same line.
for cmd in (
    "codesage search auth",
    "./target/debug/codesage search auth",
    "cargo run -p codesage -- search auth",
    "git diff --name-only | codesage risk-diff",
):
    check(extract.CODESAGE_BASH_RE.search(cmd), f"extract: bash regex matches {cmd!r}")
for cmd in (
    "codesage-bench-runner --corpus x.yaml",
    "codesage --version",
    "cd /path/codesage; cargo build",
    "cd /path/codesage && cargo test",
    "cd /path/to/codesage\ncargo build",
    "git -C /path/to/codesage log --oneline",
    "ls /path/to/codesage crates",
):
    check(not extract.CODESAGE_BASH_RE.search(cmd), f"extract: bash regex rejects {cmd!r}")

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
                _tool_result(root / "Cargo.toml"),
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

raised = False
try:
    extract.positive_int("0")
except Exception:
    raised = True
check(raised, "extract: positive_int rejects 0")
raised = False
try:
    extract.positive_int("-1")
except Exception:
    raised = True
check(raised, "extract: positive_int rejects -1")
check(extract.positive_int("3") == 3, "extract: positive_int accepts 3")

prefix = "where is the cargo manifest configured for this project"
with tempfile.TemporaryDirectory() as td:
    root = Path(td) / "project"
    sessions = Path(td) / "sessions"
    root.mkdir()
    sessions.mkdir()
    (root / "a.py").write_text("x = 1\n")
    (root / "b.py").write_text("y = 2\n")
    for idx, target in enumerate(("a.py", "b.py")):
        session = sessions / f"dedup-{idx}.jsonl"
        session.write_text(
            "\n".join(
                [
                    json.dumps(
                        {
                            "type": "user",
                            "message": {"content": f"{prefix} variant {idx}"},
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
                                        "input": {"file_path": str(root / target)},
                                    }
                                ]
                            },
                        }
                    ),
                    _tool_result(root / target),
                ]
            )
            + "\n"
        )
    cases = extract.extract_cases(sessions, str(root), min_files=1, max_cases=10)
    check(len(cases) == 2, f"extract: full-query dedup keeps distinct cases (got {len(cases)})")

with tempfile.TemporaryDirectory() as td:
    root = Path(td) / "project"
    sessions = Path(td) / "sessions"
    root.mkdir()
    sessions.mkdir()
    (root / "late.py").write_text("z = 3\n")
    msgs = [
        json.dumps(
            {
                "type": "user",
                "message": {"content": "where is the late accessed file in this project"},
                "padding": "x" * 3500,
            }
        ),
    ]
    for _ in range(44):
        msgs.append(json.dumps({"type": "assistant", "message": {"content": []}}))
    msgs.append(
        json.dumps(
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {
                            "type": "tool_use",
                            "input": {"file_path": str(root / "late.py")},
                        }
                    ]
                },
            }
        )
    )
    msgs.append(_tool_result(root / "late.py"))
    (sessions / "late.jsonl").write_text("\n".join(msgs) + "\n")
    cases = extract.extract_cases(sessions, str(root), min_files=1, max_cases=1)
    check(
        cases and cases[0]["files"] == ["late.py"],
        f"extract: lookahead beyond 40 messages includes late file (got {cases!r})",
    )

with tempfile.TemporaryDirectory() as td:
    root = Path(td) / "project"
    sessions = Path(td) / "sessions"
    root.mkdir()
    sessions.mkdir()
    (root / "dup.py").write_text("x = 1\n")
    (root / "unique.py").write_text("y = 2\n")
    duplicate_query = "where is the duplicate candidate in this project"
    unique_query = "where is the unique candidate in this project"
    now = time.time()
    for idx in range(6):
        session = sessions / f"duplicate-{idx}.jsonl"
        session.write_text(
            "\n".join(
                [
                    json.dumps(
                        {
                            "type": "user",
                            "message": {"content": duplicate_query},
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
                                        "input": {"file_path": str(root / "dup.py")},
                                    }
                                ]
                            },
                        }
                    ),
                    _tool_result(root / "dup.py"),
                ]
            )
            + "\n"
        )
        os.utime(session, (now + idx + 10, now + idx + 10))
    unique = sessions / "unique.jsonl"
    unique.write_text(
        "\n".join(
            [
                json.dumps(
                    {
                        "type": "user",
                        "message": {"content": unique_query},
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
                                    "input": {"file_path": str(root / "unique.py")},
                                }
                            ]
                        },
                    }
                ),
                _tool_result(root / "unique.py"),
            ]
        )
        + "\n"
    )
    os.utime(unique, (now, now))
    cases = extract.extract_cases(sessions, str(root), min_files=1, max_cases=2)
    files = {tuple(c["files"]) for c in cases}
    check(
        len(cases) == 2 and {("dup.py",), ("unique.py",)} == files,
        f"extract: raw duplicate cap does not starve older unique sessions (got {cases!r})",
    )

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

audit = _load("concurrency-audit.py", "audit")

clean_state = {
    "integrity": "ok",
    "orphans": {"symbols_without_file": 0, "refs_without_file": 0},
    "dupes": {"files_same_path": 0},
    "schema_migrations": [],
    "foreign_key_violations": [],
}
v = audit.classify_verdict(
    [{"returncode": 0}, {"returncode": None}], clean_state
)
check("TIMEOUT" in v, f"audit: success+timeout is a TIMEOUT verdict (got {v!r})")
v = audit.classify_verdict(
    [{"returncode": 0}, {"returncode": 1, "stderr_tail": "database is locked"}],
    clean_state,
)
check("serialized" in v, f"audit: success+lock-error is serialized (got {v!r})")
v = audit.classify_verdict([{"returncode": 0}, {"returncode": 0}], clean_state)
check("clean" in v, f"audit: both-ok is clean (got {v!r})")
v = audit.classify_verdict(
    [
        {"returncode": 0},
        {
            "returncode": 0,
            "stderr_tail": "another codesage indexer is running on /proj — skipping",
        },
    ],
    clean_state,
)
check("serialized" in v, f"audit: lockfile skip is serialized (got {v!r})")
corrupt_state = dict(clean_state, integrity="row 5 missing")
v = audit.classify_verdict([{"returncode": 0}, {"returncode": None}], corrupt_state)
check("CORRUPT" in v, f"audit: corruption wins over timeout (got {v!r})")

with tempfile.TemporaryDirectory() as td:
    db_path = Path(td) / "index.db"
    sqlite3.connect(db_path).close()
    try:
        state = audit.integrity_check(db_path)
        ok = (
            state["integrity"] != "ok"
            and "symbols" in state.get("schema_missing", [])
            and state["orphans"]["symbols_without_file"] == 0
        )
    except sqlite3.OperationalError:
        ok = False
    check(ok, "audit: integrity_check reports partial schema instead of raising")

with tempfile.TemporaryDirectory() as td:
    codesage_dir = Path(td)
    for name in ("index.db", "index.db-wal", "index.db-shm"):
        (codesage_dir / name).write_text("audit-created")
    audit.restore_db(codesage_dir, None)
    check(
        not any((codesage_dir / name).exists() for name in ("index.db", "index.db-wal", "index.db-shm")),
        "audit: restore_db removes audit-created database files when there was no original backup",
    )

# PyYAML rejects raw C0 control characters even inside quotes.
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
            loaded = yaml.safe_load(out.read_text())
            ok = loaded["cases"][0]["query"] == "color [31mred[0m output"
        except yaml.YAMLError:
            ok = False
        check(ok, "extract: control-char query round-trips through YAML")

if yaml is not None:
    doc = gen.format_corpus_yaml(
        "/proj",
        [{"id": "llm-001-foo: bar", "query": "q", "expected_files": ["src/a.py"]}],
    )
    try:
        loaded = yaml.safe_load(doc)
        ok = loaded["cases"][0]["id"] == "llm-001-foo: bar"
    except yaml.YAMLError:
        ok = False
    check(ok, "gen: id with ': ' round-trips through YAML")

# Children share one timeout deadline.
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

with tempfile.TemporaryDirectory() as td:
    db_path = Path(td) / "index.db"
    conn = sqlite3.connect(db_path)
    conn.executescript(
        """
        CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT UNIQUE);
        CREATE TABLE symbols (
            id INTEGER PRIMARY KEY,
            file_id INTEGER REFERENCES files(id),
            name TEXT, kind TEXT, line INTEGER
        );
        CREATE TABLE refs (
            id INTEGER PRIMARY KEY,
            from_file_id INTEGER REFERENCES files(id),
            to_file_id INTEGER,
            name TEXT, kind TEXT, line INTEGER
        );
        CREATE TABLE schema_migrations (name TEXT UNIQUE);
        """
    )
    conn.execute("PRAGMA foreign_keys=OFF")
    conn.execute("INSERT INTO symbols (id, file_id, name, kind, line) VALUES (1, 99, 'x', 'fn', 1)")
    conn.commit()
    conn.close()
    state = audit.integrity_check(db_path)
    check(
        len(state.get("foreign_key_violations") or []) > 0,
        "audit: foreign_key_check reports orphan symbols with FK enforcement on",
    )

inconclusive_state = dict(
    clean_state,
    semantic={"status": "inconclusive", "vec_tables": ["chunks_test_384"], "issues": [], "fts_mismatches": []},
)
v = audit.classify_verdict([{"returncode": 0}, {"returncode": 0}], inconclusive_state)
check("INCONCLUSIVE" in v, f"audit: semantic inconclusive is not clean (got {v!r})")

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


# Extensionless scripts need an explicit SourceFileLoader.
_ndcg_spec = importlib.util.spec_from_loader(
    "semble_ndcg_runner",
    importlib.machinery.SourceFileLoader(
        "semble_ndcg_runner", str(HERE / "semble-ndcg-runner")
    ),
)
ndcg_runner = importlib.util.module_from_spec(_ndcg_spec)
_ndcg_spec.loader.exec_module(ndcg_runner)


class _FakeProc:
    def __init__(self, stdout, returncode, stderr=""):
        self.stdout = stdout
        self.returncode = returncode
        self.stderr = stderr


def _run_search(monkey_stdout, returncode=0, raise_timeout=False, stderr="", diagnostics=False):
    """Drive search() against a stubbed subprocess.run."""
    import subprocess as _sp

    real = _sp.run

    def fake(*a, **kw):
        if raise_timeout:
            raise _sp.TimeoutExpired(cmd="x", timeout=1, output=monkey_stdout, stderr=stderr)
        return _FakeProc(monkey_stdout, returncode, stderr)

    ndcg_runner.subprocess.run = fake
    try:
        result = ndcg_runner.search("/bin/true", ".", "q", 10)
        return result if diagnostics else result[:2]
    finally:
        ndcg_runner.subprocess.run = real


GOOD = json.dumps({"results": [{"file_path": "a.rs"}]})

paths, status = _run_search(GOOD, 0)
check((paths, status) == (["a.rs"], "ok"), "ndcg-runner: clean run is ok")

paths, status = _run_search(GOOD, 134)
check(
    (paths, status) == (["a.rs"], "crashed-with-output"),
    "ndcg-runner: exit 134 keeps its results and is flagged",
)

for payload, label in (
    ("{}", "missing results key"),
    (json.dumps({"results": None}), "null results"),
    (json.dumps({"results": [1, 2]}), "non-object rows"),
):
    paths, status = _run_search(payload, 0)
    check(
        (paths, status) == ([], "invalid-output"),
        f"ndcg-runner: {label} is invalid-output, not a clean zero",
    )

paths, status = _run_search("not json", 0)
check((paths, status) == ([], "unparseable"), "ndcg-runner: clean-exit garbage")
paths, status = _run_search("not json", 134)
check((paths, status) == ([], "crashed"), "ndcg-runner: crash with no output")

paths, status = _run_search(GOOD, raise_timeout=True)
check(
    (paths, status) == (["a.rs"], "timeout-with-output"),
    "ndcg-runner: timeout salvages complete stdout",
)
paths, status = _run_search("", raise_timeout=True)
check((paths, status) == ([], "timeout"), "ndcg-runner: timeout with no output")

RERANK_FAILURE = "cross-encoder rerank failed; keeping pre-rerank order"
for timed_out, returncode, expected in (
    (False, 134, "crashed-with-output"),
    (True, 0, "timeout-with-output"),
):
    result = _run_search(GOOD.encode() if timed_out else GOOD, returncode,
                         raise_timeout=timed_out,
                         stderr=RERANK_FAILURE.encode() if timed_out else RERANK_FAILURE,
                         diagnostics=True)
    check(result[:2] == (["a.rs"], expected),
          "ndcg-runner: timeout/crash takes precedence over soft degradation")
    check(result[2]["stderr"] == RERANK_FAILURE
          and result[2]["quality_degradation"] == ["rerank-failed"]
          and result[2]["returncode"] == (None if timed_out else returncode),
          "ndcg-runner: transport failures retain decoded stderr and degradation evidence")

with tempfile.TemporaryDirectory() as td:
    root = Path(td)
    binary = root / "codesage-fixture"
    binary.write_text(
        "#!/usr/bin/env python3\n"
        "import json, os, sys\n"
        "if '--version' in sys.argv:\n"
        "    print('codesage 0.27.0 (fixture)'); sys.exit(0)\n"
        "warnings = {\n"
        f" 'q': 'WARN codesage_graph::search: {RERANK_FAILURE}',\n"
        " 'daemon': 'WARN codesage::query_reranker: daemon reranking failed error=connection reset',\n"
        " 'cpu': 'WARN codesage_embed::model: CODESAGE_ALLOW_CPU_FALLBACK: CUDA was requested but its libraries are not mapped; this session runs on the CPU and fingerprints as a CPU setup',\n"
        " 'transport': 'WARN codesage::query_reranker: reranker input exceeds daemon byte caps; reranking privately',\n"
        " 'embed-connect': 'WARN codesage::daemon_embed: daemon cannot embed for this run; embedding privately',\n"
        " 'embed-private': 'WARN codesage::daemon_embed: daemon request failed; embedding these privately',\n"
        " 'benign': 'INFO codesage::daemon_embed: embedding through the running daemon\\nWARN no [embedding].pooling set; defaulting to mean pooling. If this model expects CLS pooling, set pooling = \"cls\" explicitly.\\nSome nodes were not assigned to the preferred execution providers',\n"
        "}\n"
        "if os.environ.get('RUST_LOG') != 'off':\n"
        "    print(warnings[sys.argv[-1]], file=sys.stderr)\n"
        "print(json.dumps({'results': [{'file_path': 'a.rs'}]}))\n"
    )
    binary.chmod(0o755)
    previous_log = os.environ.get("RUST_LOG")
    os.environ["RUST_LOG"] = "off"
    try:
        result = ndcg_runner.search(str(binary), root, "q", 10)
    finally:
        if previous_log is None:
            os.environ.pop("RUST_LOG", None)
        else:
            os.environ["RUST_LOG"] = previous_log
    check(
        result[:2] == (["a.rs"], "degraded"),
        "ndcg-runner: real exit-zero JSON retains paths but exposes rerank failure despite inherited log suppression",
    )
    for query, status, quality, transport in (
        ("daemon", "degraded", ["daemon-rerank-failed"], []),
        ("cpu", "degraded", ["cpu-fallback"], []),
        ("transport", "transport-fallback", [], ["rerank-byte-cap"]),
        ("embed-connect", "transport-fallback", [], ["embed-connect"]),
        ("embed-private", "transport-fallback", [], ["embed-private"]),
        ("benign", "ok", [], []),
    ):
        paths, actual, evidence = ndcg_runner.search(str(binary), root, query, 10)
        check(paths == ["a.rs"] and actual == status
              and evidence["quality_degradation"] == quality
              and evidence["transport_fallbacks"] == transport
              and evidence["returncode"] == 0 and not evidence["timed_out"]
              and bool(evidence["stderr"]),
              f"ndcg-runner: {query} classification preserves actual process evidence")

    corpus = root / "corpus" / "fixture"
    (corpus / ".codesage").mkdir(parents=True)
    with sqlite3.connect(corpus / ".codesage" / "index.db") as con:
        con.execute("CREATE TABLE semantic_files (file_path TEXT)")
        con.execute("INSERT INTO semantic_files VALUES ('a.rs')")
    annotations = root / "annotations"
    annotations.mkdir()
    (annotations / "fixture.json").write_text(json.dumps([
        {"query": query, "relevant": ["a.rs"]} for query in ("q", "benign", "transport")
    ]))
    repos = root / "repos.json"
    repos.write_text(json.dumps([{"name": "fixture", "language": "rust"}]))
    artifact = root / "score.json"
    proc = ndcg_runner.subprocess.run([
        sys.executable, str(HERE / "semble-ndcg-runner"),
        "--corpus", str(root / "corpus"), "--annotations", str(annotations),
        "--repos", str(repos), "--codesage-bin", str(binary), "--json", str(artifact),
    ], capture_output=True, text=True, timeout=20)
    check(proc.returncode == 0, "ndcg-runner: real CLI writes diagnostic score artifact")
    report = json.loads(artifact.read_text())
    repo = report["per_repo"]["fixture"]
    check(repo["ndcg@10"] == 1.0 and report["by_language"]["rust"]["ndcg@10"] == 1.0,
          "ndcg-runner: diagnostic classification preserves NDCG calculation")
    check(repo["degraded"] == {"degraded": 1, "transport-fallback": 1}
          and report["by_language"]["rust"]["degraded"] == repo["degraded"],
          "ndcg-runner: repository and language cells disclose degradation")
    rows = repo["query_results"]
    check([r["query_index"] for r in rows] == [0, 1, 2]
          and [r["status"] for r in rows] == ["degraded", "ok", "transport-fallback"]
          and RERANK_FAILURE in rows[0]["stderr"]
          and rows[0]["quality_degradation"] == ["rerank-failed"]
          and rows[1]["quality_degradation"] == rows[1]["transport_fallbacks"] == []
          and rows[2]["transport_fallbacks"] == ["rerank-byte-cap"],
          "ndcg-runner: every query, including benign stderr, retains auditable evidence")
    import hashlib
    check(report["provenance"]["runner_sha256"] == hashlib.sha256(
              (HERE / "semble-ndcg-runner").read_bytes()).hexdigest()
          and report["provenance"]["search_log_filter"] == "warn,codesage=info"
          and report["provenance"]["diagnostic_policy"]["quality_warnings"]["rerank-failed"] == RERANK_FAILURE,
          "ndcg-runner: artifact pins actual runner bytes and diagnostic/logging policy")


if failures:
    print(f"FAILED ({len(failures)}):")
    for f in failures:
        print(f)
    sys.exit(1)
print("all bench-fix regression tests passed")
sys.exit(0)
