#!/usr/bin/env python3
"""Validate the `## [Unreleased]` block of CHANGELOG.md against the shared
iliaal/* Keep-a-Changelog convention.

Checks (on the `## [Unreleased]` section only — released blocks are history):
  - every `### ` subsection is one of the canonical sections;
  - subsections appear in the canonical order, with no duplicates;
  - no subsection is empty (each has at least one `- ` bullet).

Canonical order: Added, Changed, Deprecated, Removed, Fixed, Security.

Exit 0 when clean, 1 with a list of problems otherwise. An absent or empty
`[Unreleased]` block is not an error here — that is a release-time concern
handled by scripts/release.sh, which refuses to cut an empty release.

Usage: python3 scripts/check-changelog.py [path/to/CHANGELOG.md]
"""

from __future__ import annotations

import pathlib
import re
import sys

CANONICAL = ["Added", "Changed", "Deprecated", "Removed", "Fixed", "Security"]
RANK = {name: i for i, name in enumerate(CANONICAL)}


def extract_unreleased(text: str) -> str | None:
    """Return the body between `## [Unreleased]` and the next `## ` heading."""
    m = re.search(r"^## \[Unreleased\]\s*\n(.*?)(?=^## )", text, re.DOTALL | re.MULTILINE)
    if m:
        return m.group(1)
    # `[Unreleased]` may be the final `## ` block (no released versions yet).
    m = re.search(r"^## \[Unreleased\]\s*\n(.*)\Z", text, re.DOTALL | re.MULTILINE)
    return m.group(1) if m else None


def lint(body: str) -> list[str]:
    """Return a list of problem strings; empty means the block is valid."""
    problems: list[str] = []
    # Split the body into (section-name, lines) pairs.
    sections: list[tuple[str, list[str]]] = []
    current: tuple[str, list[str]] | None = None
    for line in body.splitlines():
        h = re.match(r"^###\s+(.*?)\s*$", line)
        if h:
            current = (h.group(1), [])
            sections.append(current)
        elif current is not None:
            current[1].append(line)

    seen: set[str] = set()
    last_rank = -1
    for name, lines in sections:
        if name not in RANK:
            problems.append(
                f"unknown section `### {name}` "
                f"(allowed: {', '.join(CANONICAL)})"
            )
            continue
        if name in seen:
            problems.append(f"duplicate section `### {name}`")
            continue
        seen.add(name)
        if RANK[name] <= last_rank:
            after = CANONICAL[last_rank]
            problems.append(
                f"section `### {name}` is out of order "
                f"(must come before `### {after}`; "
                f"canonical order: {' -> '.join(CANONICAL)})"
            )
        last_rank = max(last_rank, RANK[name])
        if not any(ln.lstrip().startswith("- ") for ln in lines):
            problems.append(f"section `### {name}` has no `- ` bullets (remove it or fill it)")
    return problems


def main(argv: list[str]) -> int:
    path = pathlib.Path(argv[1]) if len(argv) > 1 else pathlib.Path("CHANGELOG.md")
    if not path.is_file():
        print(f"check-changelog: {path} not found", file=sys.stderr)
        return 1
    body = extract_unreleased(path.read_text())
    if body is None:
        print("check-changelog: no `## [Unreleased]` section in CHANGELOG.md", file=sys.stderr)
        return 1
    problems = lint(body)
    if problems:
        print("check-changelog: [Unreleased] section problems:", file=sys.stderr)
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        return 1
    print("check-changelog: [Unreleased] OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
