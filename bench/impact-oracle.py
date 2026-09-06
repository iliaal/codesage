#!/usr/bin/env python3
"""
Score `codesage impact <symbol>` against a text-derived reference oracle.

For each symbol the oracle is the set of indexed source files, other than the
symbol's own defining file(s), whose code mentions the symbol as a whole word.
Two variants are scored side by side:

  raw        -- mention anywhere in the file (comments and strings included)
  code-only  -- mention after comments and string literals are stripped

`code-only` is the figure to quote: a rollup banner that says `[Axios v1]` or a
test title containing `CancelToken` is not a dependency. `raw` is kept so the
noise is visible rather than silently dropped.

The file universe is the indexer's own `files` table, so exclude patterns are
honoured and the oracle cannot count a file codesage never saw. Precision and
recall are computed per symbol and micro-averaged (summed hits over summed
sizes) in the TOTAL row, matching the method recorded on bead cs-z82.

Usage:
  impact-oracle.py --repo /tmp/semble-bench-repos/axios \
      --binary target/debug/codesage \
      --symbols Axios,AxiosHeaders,AxiosError,CanceledError,CancelToken \
      [--depth 2] [--no-index] [--show-diff] [--json out.json]

`--no-index` skips `codesage init` / `codesage index --no-semantic` and scores
the index already on disk. Stdlib only.
"""

from __future__ import annotations

import argparse
import json
import re
import sqlite3
import subprocess
import sys
from pathlib import Path

JS_EXTS = {".js", ".cjs", ".mjs", ".jsx", ".ts", ".tsx", ".cts", ".mts"}
PHP_EXTS = {".php"}

# A `/` after one of these (ignoring whitespace) opens a regex literal rather
# than dividing. Standard lexer heuristic; wrong only on `)` / `]` ambiguity,
# which is rare in the shapes that matter here (`/['"]/` in a call argument).
# `<` is deliberately absent: a regex directly after `<` does not occur in
# practice, while every JSX/TSX closing tag `</div>` would otherwise open one
# and blank the rest of the line.
_REGEX_PRECEDERS = set("(,=:[!&|?{};+-*%>~^")
_REGEX_PRECEDER_WORDS = {"return", "typeof", "case", "in", "of", "delete", "void", "throw", "new"}


def _prev_significant(out: list[str]) -> str:
    """Last non-whitespace token emitted so far: one char, or a trailing word."""
    i = len(out) - 1
    while i >= 0 and out[i].isspace():
        i -= 1
    if i < 0:
        return ""
    if out[i].isalnum() or out[i] == "_":
        j = i
        while j >= 0 and (out[j].isalnum() or out[j] == "_"):
            j -= 1
        return "".join(out[j + 1 : i + 1])
    return out[i]


def strip_js(src: str) -> str:
    """Blank out comments, string literals, template literals, and regex
    literals. Length and line structure are preserved so no other position
    shifts; the blanked regions become spaces."""
    out: list[str] = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        two = src[i : i + 2]
        if two == "//":
            while i < n and src[i] != "\n":
                out.append(" ")
                i += 1
            continue
        if two == "/*":
            while i < n and src[i : i + 2] != "*/":
                out.append("\n" if src[i] == "\n" else " ")
                i += 1
            out.extend("  ")
            i += 2
            continue
        if c in "'\"":
            quote = c
            out.append(" ")
            i += 1
            while i < n and src[i] != quote and src[i] != "\n":
                if src[i] == "\\":
                    out.append(" ")
                    i += 1
                if i < n:
                    out.append(" ")
                    i += 1
            if i < n and src[i] == quote:
                out.append(" ")
                i += 1
            continue
        if c == "`":
            out.append(" ")
            i += 1
            while i < n and src[i] != "`":
                if src[i] == "\\":
                    out.append(" ")
                    i += 1
                    if i < n:
                        out.append("\n" if src[i] == "\n" else " ")
                        i += 1
                    continue
                if src[i : i + 2] == "${":
                    # Interpolated expression: keep its code, but strings inside
                    # it are handled by recursion on the balanced slice.
                    depth = 1
                    j = i + 2
                    while j < n and depth:
                        if src[j] == "{":
                            depth += 1
                        elif src[j] == "}":
                            depth -= 1
                        j += 1
                    out.append("  ")
                    out.append(strip_js(src[i + 2 : j - 1]))
                    out.append(" ")
                    i = j
                    continue
                out.append("\n" if src[i] == "\n" else " ")
                i += 1
            if i < n:
                out.append(" ")
                i += 1
            continue
        if c == "/":
            prev = _prev_significant(out)
            if prev == "" or prev in _REGEX_PRECEDERS or prev in _REGEX_PRECEDER_WORDS:
                out.append(" ")
                i += 1
                in_class = False
                while i < n and src[i] != "\n":
                    ch = src[i]
                    if ch == "\\":
                        out.append("  ")
                        i += 2
                        continue
                    if ch == "[":
                        in_class = True
                    elif ch == "]":
                        in_class = False
                    elif ch == "/" and not in_class:
                        out.append(" ")
                        i += 1
                        break
                    out.append(" ")
                    i += 1
                continue
        out.append(c)
        i += 1
    return "".join(out)


def strip_php(src: str) -> str:
    out: list[str] = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        two = src[i : i + 2]
        if two == "//" or (c == "#" and src[i : i + 2] != "#["):
            while i < n and src[i] != "\n":
                out.append(" ")
                i += 1
            continue
        if two == "/*":
            while i < n and src[i : i + 2] != "*/":
                out.append("\n" if src[i] == "\n" else " ")
                i += 1
            out.extend("  ")
            i += 2
            continue
        if c in "'\"":
            quote = c
            out.append(" ")
            i += 1
            while i < n and src[i] != quote:
                if src[i] == "\\":
                    out.append(" ")
                    i += 1
                if i < n:
                    out.append("\n" if src[i] == "\n" else " ")
                    i += 1
            if i < n:
                out.append(" ")
                i += 1
            continue
        if src[i : i + 3] == "<<<":
            m = re.match(r"<<<[ \t]*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1[ \t]*\n", src[i:])
            if m:
                ident = m.group(2)
                end = re.compile(r"^[ \t]*" + re.escape(ident) + r"\b", re.M)
                em = end.search(src, i + m.end())
                stop = em.start() if em else n
                for ch in src[i:stop]:
                    out.append("\n" if ch == "\n" else " ")
                i = stop
                continue
        out.append(c)
        i += 1
    return "".join(out)


def strip_for(path: str, text: str) -> str:
    ext = Path(path).suffix
    if ext in JS_EXTS:
        return strip_js(text)
    if ext in PHP_EXTS:
        return strip_php(text)
    return text


def run(binary: str, repo: Path, *args: str) -> str:
    proc = subprocess.run(
        [binary, *args],
        cwd=repo,
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        sys.exit(f"{binary} {' '.join(args)} failed ({proc.returncode}):\n{proc.stderr}")
    return proc.stdout


def indexed_files(repo: Path) -> list[str]:
    db = repo / ".codesage" / "index.db"
    if not db.exists():
        sys.exit(f"no index at {db}; drop --no-index or run `codesage index --no-semantic`")
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        return [row[0] for row in con.execute("SELECT path FROM files")]
    finally:
        con.close()


def definition_files(binary: str, repo: Path, symbol: str) -> set[str]:
    out = json.loads(run(binary, repo, "find-symbol", symbol, "--json"))
    return {r["file_path"] for r in out.get("results", [])}


def impact_files(binary: str, repo: Path, symbol: str, depth: int) -> list[str]:
    out = json.loads(run(binary, repo, "impact", symbol, "--symbol", "--depth", str(depth), "--json"))
    return [r["file_path"] for r in out.get("results", [])]


def prf(hit: int, returned: int, truth: int) -> tuple[float, float, float]:
    p = hit / returned if returned else 0.0
    r = hit / truth if truth else 0.0
    f = 2 * p * r / (p + r) if (p + r) else 0.0
    return p, r, f


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", required=True, type=Path)
    ap.add_argument("--binary", required=True)
    ap.add_argument("--symbols", required=True, help="comma-separated symbol names")
    ap.add_argument("--depth", type=int, default=2)
    ap.add_argument("--no-index", action="store_true", help="score the existing index without rebuilding it")
    ap.add_argument("--show-diff", action="store_true", help="list missed and extra files per symbol")
    ap.add_argument("--json", type=Path, help="write per-symbol sets and scores here")
    args = ap.parse_args()

    repo: Path = args.repo.resolve()
    binary = str(Path(args.binary).resolve())
    symbols = [s.strip() for s in args.symbols.split(",") if s.strip()]

    if not args.no_index:
        if not (repo / ".codesage" / "config.toml").exists():
            run(binary, repo, "init")
        sys.stderr.write(run(binary, repo, "index", "--no-semantic"))

    files = indexed_files(repo)
    raw_text: dict[str, str] = {}
    code_text: dict[str, str] = {}
    for rel in files:
        p = repo / rel
        try:
            text = p.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        raw_text[rel] = text
        code_text[rel] = strip_for(rel, text)

    report: dict[str, dict] = {}
    tot = {"raw_truth": 0, "code_truth": 0, "returned": 0, "raw_hit": 0, "code_hit": 0}
    header = f"{'symbol':<22} {'raw':>4} {'code':>4} {'ret':>4} {'hit':>4}  {'P':>5} {'R':>5} {'F1':>5}   {'rawP':>5} {'rawR':>5}"
    print(header)
    print("-" * len(header))
    for sym in symbols:
        pat = re.compile(r"\b" + re.escape(sym) + r"\b")
        defs = definition_files(binary, repo, sym)
        raw_truth = {f for f, t in raw_text.items() if f not in defs and pat.search(t)}
        code_truth = {f for f, t in code_text.items() if f not in defs and pat.search(t)}
        returned = set(impact_files(binary, repo, sym, args.depth))
        raw_hit = returned & raw_truth
        code_hit = returned & code_truth
        p, r, f = prf(len(code_hit), len(returned), len(code_truth))
        rp, rr, _ = prf(len(raw_hit), len(returned), len(raw_truth))
        print(
            f"{sym:<22} {len(raw_truth):>4} {len(code_truth):>4} {len(returned):>4} {len(code_hit):>4}"
            f"  {p:>5.2f} {r:>5.2f} {f:>5.2f}   {rp:>5.2f} {rr:>5.2f}"
        )
        if args.show_diff:
            for m in sorted(code_truth - returned):
                print(f"    missed  {m}")
            for e in sorted(returned - code_truth):
                tag = "raw-only" if e in raw_truth else "extra"
                print(f"    {tag:<7} {e}")
        report[sym] = {
            "definitions": sorted(defs),
            "raw_truth": sorted(raw_truth),
            "code_truth": sorted(code_truth),
            "returned": sorted(returned),
            "precision": p,
            "recall": r,
            "f1": f,
            "raw_precision": rp,
            "raw_recall": rr,
        }
        tot["raw_truth"] += len(raw_truth)
        tot["code_truth"] += len(code_truth)
        tot["returned"] += len(returned)
        tot["raw_hit"] += len(raw_hit)
        tot["code_hit"] += len(code_hit)

    p, r, f = prf(tot["code_hit"], tot["returned"], tot["code_truth"])
    rp, rr, _ = prf(tot["raw_hit"], tot["returned"], tot["raw_truth"])
    print("-" * len(header))
    print(
        f"{'TOTAL':<22} {tot['raw_truth']:>4} {tot['code_truth']:>4} {tot['returned']:>4} {tot['code_hit']:>4}"
        f"  {p:>5.2f} {r:>5.2f} {f:>5.2f}   {rp:>5.2f} {rr:>5.2f}"
    )
    report["TOTAL"] = {**tot, "precision": p, "recall": r, "f1": f, "raw_precision": rp, "raw_recall": rr}
    if args.json:
        args.json.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
