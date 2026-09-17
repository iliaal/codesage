#!/usr/bin/env python3
"""Impact-oracle regressions. Run: python3 bench/test_impact_oracle.py"""
from __future__ import annotations

import importlib.util
import json
import re
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("impact_oracle", HERE / "impact-oracle.py")
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)
failures: list[str] = []


def check(condition: bool, label: str) -> None:
    if not condition:
        failures.append(label)


# Each expression places executable Target after braces that are not code.
# LiteralOnly must never become part of the code-only reference oracle.
expressions = {
    "quoted closing brace": 'format("} LiteralOnly", Target)',
    "quoted opening brace": "format('{ LiteralOnly', Target)",
    "escaped quote": r'format("\"} LiteralOnly", Target)',
    "block comment": 'format(/* } { LiteralOnly */ Target)',
    "line comment": 'format(// } { LiteralOnly\nTarget)',
    "regex closing brace": r'format(/}LiteralOnly/, Target)',
    "regex character class": r'format(/[{}]LiteralOnly/, Target)',
    "regex escape": r'format(/\}LiteralOnly/, Target)',
    "nested template": 'format(`} LiteralOnly ${format("}", Target)}`, 1)',
    "nested object": 'format({nested: {value: Target}}, "} LiteralOnly")',
}
for label, expression in expressions.items():
    source = f'const result = `LiteralOnly ${{{expression}}} LiteralOnly`; After();\n'
    stripped = oracle.strip_js(source)
    check(re.search(r"\bTarget\b", stripped) is not None, f"{label}: executable reference survives")
    check("LiteralOnly" not in stripped, f"{label}: literal-only references excluded")
    check("After()" in stripped, f"{label}: scanning resumes after template")
    check(len(stripped) == len(source), f"{label}: source positions preserved")
    check(
        [i for i, ch in enumerate(stripped) if ch == "\n"]
        == [i for i, ch in enumerate(source) if ch == "\n"],
        f"{label}: line positions preserved",
    )

# An object-closing brace is code, unlike the surrounding template text.
source = '`LiteralOnly ${({value: Target}).value / divisor} LiteralOnly`'
stripped = oracle.strip_js(source)
check("Target" in stripped and "/ divisor" in stripped, "object braces and division remain code")
check("LiteralOnly" not in stripped, "template tail remains literal after object expression")

# Unsupported text must not be usable as code-only truth through the dispatcher.
try:
    oracle.strip_for("comment.py", '# Target\ntext = "Target"\n')
except ValueError:
    pass
else:
    check(False, "unsupported language cannot return raw text as code")


def write_index(repo: Path, sources: dict[str, str]) -> None:
    (repo / ".codesage").mkdir()
    with sqlite3.connect(repo / ".codesage" / "index.db") as con:
        con.execute("CREATE TABLE files (path TEXT NOT NULL)")
        con.executemany("INSERT INTO files VALUES (?)", [(path,) for path in sources])
    for path, text in sources.items():
        (repo / path).write_text(text, encoding="utf-8")


def run_oracle(repo: Path, binary: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(HERE / "impact-oracle.py"), "--repo", str(repo),
         "--binary", str(binary), "--symbols", "Target", "--no-index",
         "--json", str(repo / "report.json")],
        capture_output=True, text=True,
    )


with tempfile.TemporaryDirectory(prefix="impact-oracle-unsupported-") as td:
    repo = Path(td)
    write_index(repo, {
        "code.js": "Target();\n",
        "comment.py": "# Target\n",
        "literal.rs": 'const LABEL: &str = "Target";\n',
        "comment.go": "package main\n// Target\n",
    })
    result = run_oracle(repo, repo / "unused-binary")
    check(result.returncode == 2, "mixed-language CLI rejects unsupported indexed files")
    check(all(path in result.stderr for path in ("comment.py", "literal.rs", "comment.go")),
          "language rejection identifies unsupported indexed files")
    check(result.stdout == "", "language rejection emits no per-symbol or TOTAL scores")
    check(not (repo / "report.json").exists(), "language rejection writes no misleading JSON scores")

with tempfile.TemporaryDirectory(prefix="impact-oracle-supported-") as td:
    repo = Path(td)
    write_index(repo, {
        "definition.js": "class Target {}\n",
        "use.ts": 'const value = `${format("}", Target)}`;\n',
        "use.php": "<?php Target();\n",
        "noise.js": '// Target\nconst title = "Target";\n',
        "noise.php": '<?php /* Target */ $title = "Target";\n',
    })
    # Only the external CodeSage boundary is simulated; the CLI reads real source
    # files and SQLite index and computes its own text-derived reference sets.
    binary = repo / "codesage-fixture"
    binary.write_text(
        f"#!{sys.executable}\n"
        "import json, sys\n"
        "if sys.argv[1] == 'find-symbol':\n"
        "    paths = ['definition.js']\n"
        "elif sys.argv[1] == 'impact':\n"
        "    paths = ['use.ts', 'use.php', 'noise.js']\n"
        "else:\n"
        "    sys.exit(1)\n"
        "print(json.dumps({'results': [{'file_path': p} for p in paths]}))\n",
        encoding="utf-8",
    )
    binary.chmod(0o755)
    result = run_oracle(repo, binary)
    check(result.returncode == 0, f"supported-language scoring succeeds: {result.stderr}")
    if result.returncode == 0:
        report = json.loads((repo / "report.json").read_text(encoding="utf-8"))
        symbol = report["Target"]
        check(symbol["code_truth"] == ["use.php", "use.ts"],
              "code-only truth includes executable interpolation and PHP calls only")
        check(symbol["raw_truth"] == ["noise.js", "noise.php", "use.php", "use.ts"],
              "raw truth retains literal mentions but excludes defining file")
        check(symbol["precision"] == 2 / 3 and symbol["recall"] == 1,
              "supported code-only precision and recall reflect executable references")
        check(symbol["raw_precision"] == 1 and symbol["raw_recall"] == 3 / 4,
              "raw scoring remains distinct from code-only scoring")
        check(report["TOTAL"]["code_truth"] == 2 and report["TOTAL"]["code_hit"] == 2
              and report["TOTAL"]["precision"] == 2 / 3 and report["TOTAL"]["recall"] == 1,
              "TOTAL scores use supported code-only reference sets")

if failures:
    print("FAILED:\n" + "\n".join(f"  {failure}" for failure in failures))
    sys.exit(1)
print("all impact-oracle regression tests passed")
