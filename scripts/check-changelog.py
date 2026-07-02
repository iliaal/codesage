#!/usr/bin/env python3
"""Validate the `## [Unreleased]` block of CHANGELOG.md against the shared
iliaal/* Keep-a-Changelog convention.

Checks (on the `## [Unreleased]` section only — released blocks are history):
  - every `### ` subsection is one of the canonical sections;
  - subsections appear in the canonical order, with no duplicates;
  - no subsection is empty (each has at least one `- ` bullet);
  - each bullet is terse: no bold lead-in, no justification/explanation
    phrase, and under the length backstop (consolidated semicolon lists are
    fine; multi-sentence paragraphs are not).

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

# Substrings that mark a bullet as explaining or justifying the change rather
# than stating it. The terse convention is: name the command/tool/behavior,
# state the observable effect, stop. These are the tells of a paragraph
# creeping in. Matched case-insensitively. Kept deliberately narrow so a
# consolidated bullet (several sibling fixes joined by `;`) does not trip it.
PROSE_TELLS = (
    ", so ",          # causal tail: "...refs, so a reindex leaves no stale hits"
    "so that ",
    "in order to ",
    "which means ",
    "note that ",
    "was previously",
    "previously, ",
)

# Backstop against paragraph bullets. Consolidated semicolon lists run ~300
# chars; genuine multi-sentence explanations run longer. Not a style ceiling —
# a single terse change should land well under this.
BULLET_MAX_LEN = 400


def _excerpt(bullet: str, width: int = 60) -> str:
    """A short, single-line handle for a bullet in an error message."""
    return bullet if len(bullet) <= width else bullet[: width - 1] + "…"


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
        bullets = [ln.strip()[2:].strip() for ln in lines if ln.lstrip().startswith("- ")]
        if not bullets:
            problems.append(f"section `### {name}` has no `- ` bullets (remove it or fill it)")
        for bullet in bullets:
            problems.extend(f"`### {name}`: {p}" for p in _lint_bullet(bullet))
    return problems


def _lint_bullet(bullet: str) -> list[str]:
    """Terse-style problems for a single bullet body (leading `- ` stripped)."""
    problems: list[str] = []
    if bullet.startswith("**"):
        problems.append(f"bold lead-in — drop the `**...**` prefix — {_excerpt(bullet)}")
    low = bullet.lower()
    for tell in PROSE_TELLS:
        if tell in low:
            problems.append(
                f"reads as explanation not a change (`{tell.strip()}`); "
                f"state the effect and stop — {_excerpt(bullet)}"
            )
            break
    if len(bullet) > BULLET_MAX_LEN:
        problems.append(
            f"{len(bullet)} chars (cap {BULLET_MAX_LEN}); split or tighten — {_excerpt(bullet)}"
        )
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
