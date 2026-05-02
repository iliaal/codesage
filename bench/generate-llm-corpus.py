#!/usr/bin/env python3
"""
Generate LLM-as-judge eval cases for a CodeSage corpus.

For each sampled source file in the project, ask `codex exec` to invent one
natural-language search query that would lead a developer to this file. Emit
the query + the file path in CodeSage's existing corpus YAML format
(project_root + cases[id, query, expected_files]).

This is the "Semble methodology" adapted: source-grounded queries that
reflect how an agent would actually search, instead of git-commit subjects.
A future pass can add a verify step (separate codex call to filter false
positives) and category labels (semantic / architecture / symbol).

Usage:
    bench/generate-llm-corpus.py --project-root <path> [-n 20] [--seed 42]
                                  [--out corpus.yaml]
"""

from __future__ import annotations

import argparse
import json
import random
import re
import shlex
import subprocess
import sys
from pathlib import Path

# Skip these — already excluded from the structural index, also useless as
# query targets (the model has no signal in them).
SKIP_DIR_PATTERNS = re.compile(
    r"(^|/)("
    r"node_modules|vendor|target|build|dist|out|"
    r"__pycache__|\.next|\.cache|\.git|"
    r"tests?|__tests__|benches|examples?"
    r")(/|$)"
)
SKIP_FILE_SUFFIXES = (
    ".min.js", ".min.css", ".d.ts", ".lock", ".sum",
    ".pyc", ".pyo", ".class", ".o", ".so",
)
ALLOWED_SUFFIXES = (
    ".py", ".rs", ".ts", ".tsx", ".js", ".jsx", ".go",
    ".rb", ".php", ".java", ".kt", ".scala", ".swift",
    ".c", ".cc", ".cpp", ".h", ".hpp", ".cs",
    ".ex", ".exs", ".erl", ".lua",
)

MIN_LINES = 30
MAX_LINES = 500


def candidate_files(project_root: Path) -> list[Path]:
    out: list[Path] = []
    for path in project_root.rglob("*"):
        if not path.is_file():
            continue
        rel = path.relative_to(project_root).as_posix()
        if SKIP_DIR_PATTERNS.search(rel):
            continue
        if path.suffix not in ALLOWED_SUFFIXES:
            continue
        if path.name.endswith(SKIP_FILE_SUFFIXES):
            continue
        try:
            line_count = sum(1 for _ in path.open("r", encoding="utf-8", errors="ignore"))
        except OSError:
            continue
        if line_count < MIN_LINES or line_count > MAX_LINES:
            continue
        out.append(path)
    return out


PROMPT_TEMPLATE = """\
You are helping build a code-search benchmark. Given the file below, invent
ONE natural-language search query an engineer might type when navigating an
unfamiliar codebase to find this file or its functionality.

Constraints:
- 4-15 words.
- Sound like a search box query, NOT a git commit subject. No leading
  `feat:` / `fix:` / `chore:` etc.
- Do NOT include the file name, path, or any verbatim symbol from the file.
- Describe WHAT the code does (behaviour / responsibility), not just
  keywords from it.
- Output the query on a single line, with no surrounding quotes or
  preamble. Nothing else.

File: {rel_path}

```{lang}
{content}
```
"""


def lang_for(suffix: str) -> str:
    return {
        ".py": "python", ".rs": "rust", ".ts": "typescript", ".tsx": "tsx",
        ".js": "javascript", ".jsx": "jsx", ".go": "go", ".rb": "ruby",
        ".php": "php", ".java": "java", ".kt": "kotlin", ".scala": "scala",
        ".swift": "swift", ".c": "c", ".cc": "cpp", ".cpp": "cpp",
        ".h": "c", ".hpp": "cpp", ".cs": "csharp",
    }.get(suffix, "")


def generate_query(file_path: Path, project_root: Path) -> str | None:
    rel = file_path.relative_to(project_root).as_posix()
    try:
        content = file_path.read_text(encoding="utf-8", errors="ignore")
    except OSError:
        return None

    # Truncate giant files defensively even though we already line-filter.
    if len(content) > 20_000:
        content = content[:20_000] + "\n... (truncated)\n"

    prompt = PROMPT_TEMPLATE.format(
        rel_path=rel, lang=lang_for(file_path.suffix), content=content
    )

    proc = subprocess.run(
        ["codex", "exec", "--skip-git-repo-check"],
        input=prompt,
        text=True,
        capture_output=True,
        timeout=180,
    )
    if proc.returncode != 0:
        print(f"  ! codex exec rc={proc.returncode}: {proc.stderr.strip()[:200]}",
              file=sys.stderr)
        return None

    # codex exec output format: session id line, separator, user/codex sections,
    # tokens used, then the raw final response. The final non-empty line in the
    # last `codex` block is the model's actual answer.
    lines = [ln.rstrip() for ln in proc.stdout.splitlines() if ln.strip()]
    if not lines:
        return None
    # Walk backwards from the end, skipping tokens-used / numeric noise, to
    # find the first plausibly-sentence line. Codex emits the final response
    # twice (in the codex section AND after the tokens-used footer); the
    # post-footer copy is the last line.
    for line in reversed(lines):
        if re.search(r"^\d+(,\d{3})*$|^tokens used$|^session id:", line):
            continue
        return line.strip().strip('"').strip("'").rstrip(".")
    return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--project-root", required=True, type=Path)
    ap.add_argument("-n", "--num-cases", type=int, default=20)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--out", type=Path,
                    help="output yaml path (default: stdout)")
    args = ap.parse_args()

    project_root = args.project_root.resolve()
    if not project_root.is_dir():
        sys.exit(f"not a directory: {project_root}")

    candidates = candidate_files(project_root)
    if not candidates:
        sys.exit(f"no candidate source files found under {project_root}")

    rng = random.Random(args.seed)
    rng.shuffle(candidates)
    sampled = candidates[: args.num_cases]
    print(f"sampling {len(sampled)} files from {len(candidates)} candidates",
          file=sys.stderr)

    cases: list[dict] = []
    for i, file_path in enumerate(sampled, start=1):
        rel = file_path.relative_to(project_root).as_posix()
        print(f"[{i}/{len(sampled)}] {rel}", file=sys.stderr)
        query = generate_query(file_path, project_root)
        if not query:
            print("  ! no query produced, skipping", file=sys.stderr)
            continue
        cases.append({
            "id": f"llm-{i:03d}-{file_path.stem}",
            "query": query,
            "expected_files": [rel],
        })
        print(f"  q: {query}", file=sys.stderr)

    payload = {"project_root": str(project_root), "cases": cases}

    # Hand-roll YAML so we don't pull in pyyaml just for output (the bench
    # runner needs it, but generation should work in lighter environments).
    out_lines: list[str] = []
    out_lines.append(f"project_root: {payload['project_root']}")
    out_lines.append("cases:")
    for case in cases:
        # Quote query if it contains characters YAML would mishandle.
        q = case["query"].replace("'", "''")
        out_lines.append(f"  - id: {case['id']}")
        out_lines.append(f"    query: '{q}'")
        out_lines.append("    expected_files:")
        for f in case["expected_files"]:
            out_lines.append(f"      - {f}")
    serialized = "\n".join(out_lines) + "\n"

    if args.out:
        args.out.write_text(serialized)
        print(f"wrote {len(cases)} cases to {args.out}", file=sys.stderr)
    else:
        sys.stdout.write(serialized)
    return 0


if __name__ == "__main__":
    sys.exit(main())
