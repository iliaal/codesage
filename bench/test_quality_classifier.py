#!/usr/bin/env python3
"""Tests for the question-shape classifier in `analyze-codesage-quality.py`.

Usage:
  python3 bench/test_quality_classifier.py

Bare-assert style — no pytest dependency. Exits 0 on success, 1 on failure.
The analyzer file has a hyphen in its name so we load it via importlib
rather than `import`. The classifier IS the §2.3 retrospective metric;
without these tests the regex list can drift silently.
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path


def _load_analyzer():
    here = Path(__file__).resolve().parent
    spec = importlib.util.spec_from_file_location(
        "analyzer", here / "analyze-codesage-quality.py"
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


m = _load_analyzer()
classify = m.classify_user_question
is_real = m.is_real_user_question

failures: list[str] = []


def expect(actual, expected, label: str) -> None:
    if actual != expected:
        failures.append(f"  {label}: expected {expected!r}, got {actual!r}")


# --------------------------------------------------------------------
# is_real_user_question — should reject non-question system messages
# --------------------------------------------------------------------
expect(is_real(""), False, "empty string is rejected")
expect(is_real("   "), False, "whitespace-only is rejected (no real content)")
# ^ note: current implementation returns True for whitespace because the
# length check is permissive. If this assertion fails, decide whether to
# tighten `is_real_user_question` or relax this expectation.
expect(
    is_real("<task-notification>foo</task-notification>"),
    False,
    "<task-notification> envelope rejected",
)
expect(
    is_real("Some prefix\n<command-name>foo</command-name>\nmore"),
    False,
    "<command-name> block rejected",
)
expect(
    is_real("<wiki-hint>matched</wiki-hint>"),
    False,
    "<wiki-hint> envelope rejected",
)
expect(
    is_real("<system-reminder>tool reminder</system-reminder>"),
    False,
    "<system-reminder> envelope rejected",
)
expect(
    is_real("[Request interrupted by user]"),
    False,
    "interrupt sentinel rejected",
)
expect(
    is_real("---\nname: dual-review\ndescription: foo\n---\n# body"),
    False,
    "frontmatter-style pasted skill body rejected",
)
expect(
    is_real("# Title\n\n## Section\n\nContent body."),
    False,
    "multi-heading markdown body rejected (pasted command body)",
)
expect(
    is_real("a" * 2001),
    False,
    ">2000-char body rejected (too long for a real question)",
)
# Genuine short user question must pass.
expect(
    is_real("where does auth happen in this codebase?"),
    True,
    "real short user question accepted",
)


# --------------------------------------------------------------------
# classify_user_question — bucket assignment
# --------------------------------------------------------------------

# semantic: paraphrase / concept / implementation-request shapes
expect(
    classify("where does auth happen?"),
    "semantic",
    "where-does-X-happen → semantic",
)
expect(
    classify("how does the indexer work?"),
    "semantic",
    "how-does-X-work → semantic",
)
expect(
    classify("find the file that handles config loading"),
    "semantic",
    "find-the-file-that-handles → semantic",
)
expect(
    classify("which file implements the search pipeline"),
    "semantic",
    "which-file-implements → semantic",
)
expect(
    classify("look at the retry flow for embedding errors"),
    "semantic",
    "look-at-the-X-flow → semantic (implementation-request shape)",
)
expect(
    classify("show me where embeddings are loaded"),
    "semantic",
    "show-me-where → semantic",
)

# identifier: backticked symbols, "where is X defined", "find references"
expect(
    classify("where is `EmbeddingConfig` defined?"),
    "identifier",
    "backticked CamelCase identifier → identifier",
)
expect(
    classify("where is do_thing called?"),
    "identifier",
    "where-is-X-called with non-backticked snake_case → identifier",
)
expect(
    classify("find references to AssessRisk"),
    "identifier",
    "find-references-to-X → identifier",
)
expect(
    classify("what calls render_scorecard?"),
    "identifier",
    "what-calls-X → identifier",
)

# literal: TODO, search-for-quoted, error-message
expect(
    classify("look for TODO markers in src/"),
    "literal",
    "TODO mention → literal",
)
expect(
    classify("grep for 'database is locked' in the logs"),
    "literal",
    "grep-for-quoted → literal",
)
expect(
    classify("find the error message about busy timeouts"),
    "literal",
    "error-message → literal",
)

# other: conversational, no clear retrieval shape
expect(classify(""), "other", "empty → other")
expect(classify("yes"), "other", "ack → other")
expect(classify("commit and push"), "other", "imperative → other")
expect(
    classify("let's add C++ support"),
    "other",
    "ambiguous task request → other",
)


# --------------------------------------------------------------------
# Precedence: identifier > literal > semantic > other
# --------------------------------------------------------------------

# Backticked symbol embedded in a paraphrase question — should still
# classify as identifier because the symbol is the stronger signal.
expect(
    classify("where is `EmbeddingConfig` defined? how does it work?"),
    "identifier",
    "identifier wins over semantic when both fire",
)

# Literal TODO embedded in a paraphrase shape — literal wins over semantic.
expect(
    classify("how does the TODO scanning work in our linter?"),
    "literal",
    "literal wins over semantic when both fire",
)


# --------------------------------------------------------------------
# Done
# --------------------------------------------------------------------

if failures:
    print(f"FAIL: {len(failures)} assertions did not match:")
    for f in failures:
        print(f)
    sys.exit(1)

print("OK: all classifier assertions passed.")
