#!/usr/bin/env python3
"""
Generate self-hosting eval corpora from an onboarded project's own index.

Two zero-label generators, both emitting the YAML shape that
`bench/codesage-bench-runner` consumes (`project_root`, `cases: [{id, query,
expected_files, source}]`), so the existing runner scores them unchanged.

  cochange    Replay recent commits. Each kept commit contributes one case:
              query = commit subject, expected_files = the changed indexed
              source files minus one seed. The seed is the changed file whose
              path stem or parent-directory name occurs in the subject scope
              or body (case-insensitive, word-bounded, longest match wins,
              ties broken by lexicographic path); when no file matches, the
              file with the most `symbols` rows (ties broken by lexicographic
              max). The seed is dropped so the case is not answered by the
              file the subject names. Commits whose conventional-commit type
              is style/chore/ci/build/deps/release are skipped, docs commits
              are skipped unless a non-markdown gold file remains, and
              subjects with fewer than 3 content words after stopword
              stripping are skipped (`--include-all-types` disables the type
              filter only). Gold is capped at `--max-files - 1` (default 9)
              because recall@k cannot exceed k/|gold|.
  known-item  Sample function/method/class/struct/trait/interface/enum symbols
              whose source carries a doc comment of >= 6 words: for Python the
              docstring first, otherwise the contiguous comment block right
              above the definition (attributes/decorators skipped). `#` counts
              as a comment only for python/php/ruby/shell/bash/perl, `--` only for
              DASH_COMMENT_LANGUAGES (empty today); a `/* */` block counts only
              when every line starts with `/*`, `*`, or `*/`; license headers
              are dropped; Rust symbols inside `#[cfg(test)] mod` blocks are
              skipped (best effort). Symbols are deduplicated to MIN(line_start)
              per (file, name). Each symbol yields a doc case (first sentence,
              stopwords stripped, 4..12 tokens) and, when the name is defined
              in exactly one file, a bare-name case; names defined in 2..3
              files keep only the doc case, names defined in more than 3
              files are skipped. Doc cases whose query contains every token of
              the symbol name as a whole word are tagged
              `known-item:doc-leaky`.
              Gold is the defining file (`--gold defining`) or the referencing
              files (`--gold references`): files of the same language holding
              a `refs` row for the symbol. A row recorded with a qualifier
              counts when the qualifier is `super`, `crate`, `Self` (`self`
              for Rust only), the defining file's stem, a directory segment
              of its path (also as the last `_` component of a crate path
              such as `codesage_graph`), or a symbol defined in that file.
              A row recorded bare is classified from the source text at its
              line/col: (a) a receiver call (`x.foo()`, `x->foo()`) counts
              only when the definition's owner type (from `qualified_name`,
              generics stripped) is named by the candidate file: imported,
              extended, trait-used, type-hinted, instantiated, or defined
              there (a grandparent's method stays dropped); (b) a call
              qualified in source but recorded bare (`A::foo()`, `A\\foo()`)
              recovers the qualifier and uses the rule above; (c) a truly
              bare call needs an import row naming the symbol, or for
              C/C++/Go the candidate may sit in the defining file's directory
              or hold an include/import row naming it, and is dropped when
              the file binds the name to a local callable (closure, nested
              fn, def, lambda, arrow, const/static, all invisible to the
              extractor). Files whose only rows are `import` / `include` are
              dropped (re-export lines carry no behaviour). Glob imports are
              recorded as the module path (`use a::b::*` -> `a::b`) or not
              at all (`use super::*`), so a caller that reaches the symbol
              only through one is conservatively dropped (cs-zz6). The
              measured kept/dropped counts per language and row class are
              written to the corpus header. References mode requires exactly
              one defining file (exact-name rows cannot tell homonyms apart)
              and skips symbols with 0 or more than 10 referencing files.
              Each references-mode case carries `defining_file`;
              bench/codesage-bench-runner drops that file from the returned
              list before ranking, otherwise name queries would carry a
              structural first-hit floor of 2 (the defining file ranks first
              and is excluded from gold). Known gap: a Python `alias.func()`
              call on a free function classifies as a receiver call with no
              owner type and is dropped; on click that shape is 62% (23 of
              37) of the corpus-relevant receiver drops.

known-item with `--gold defining` is a saturation smoke test: the doc phrase
lives inside the gold chunk, so recall@k sits near 1.0 on a healthy index. Its
decision metrics are miss rate and first-hit rank (catastrophic-regression
detector: empty table, wrong model, broken chunking), not recall. `--gold
references` is the non-saturating arm.

Usage:
  self-eval.py --mode cochange|known-item|both --project <root> [--out DIR]
               [--commits 80] [--min-files 2] [--max-files 10]
               [--include-all-types] [--sample 150] [--seed 42]
               [--gold defining|references] [--include-tests]
"""

from __future__ import annotations

import argparse
import functools
import hashlib
import random
import re
import sqlite3
import subprocess
import sys
import datetime as _dt
from dataclasses import dataclass, field
from pathlib import Path

try:
    import tomllib
except ImportError:
    sys.exit("self-eval.py needs Python >= 3.11 (tomllib); found "
             f"{sys.version_info.major}.{sys.version_info.minor}")

try:
    import yaml
except ImportError:
    sys.exit("pyyaml required: pip install pyyaml")

MIN_COMMENT_WORDS = 6
MIN_DOC_QUERY_TOKENS = 4
MAX_DOC_QUERY_WORDS = 12
MAX_DEFINITION_FILES = 3
MAX_REFERENCE_FILES = 10
MIN_SUBJECT_CONTENT_WORDS = 3
DEFAULT_MAX_FILES = 10
GIT_TIMEOUT_SECS = 60
KNOWN_ITEM_KINDS = ("function", "method", "class", "struct", "trait", "interface", "enum")
SKIPPED_COMMIT_TYPES = frozenset({"style", "chore", "ci", "build", "deps", "release"})
DOCS_COMMIT_TYPE = "docs"
MARKDOWN_SUFFIXES = (".md", ".markdown", ".rst", ".txt")
HASH_COMMENT_LANGUAGES = frozenset({"python", "php", "ruby", "shell", "bash", "perl"})
# None of CodeSage's languages use `--` comments; add "lua" / "sql" here if indexed.
DASH_COMMENT_LANGUAGES: frozenset[str] = frozenset()
LICENSE_PREFIXES = ("copyright", "spdx", "licensed")
REEXPORT_REF_KINDS = frozenset({"import", "include"})
RELATIVE_PATH_QUALIFIERS = frozenset({"super", "crate", "Self"})
# Languages where a bare call may also be accepted by directory neighbourhood
# or an include/import row naming the defining file (no name-level imports).
NEIGHBOURHOOD_LANGUAGES = frozenset({"c", "cpp", "go"})
# Languages where a bare type row (`extends Message`, `new Message`) resolves
# by directory: PSR-4 maps namespace to directory, Go and C/C++ share a package
# or translation-unit neighbourhood. Rust, Python and JS/TS require an import.
SAME_DIR_TYPE_LANGUAGES = frozenset({"php", "go", "c", "cpp"})
RECEIVER_SUFFIXES = (".", "->")
# Row classes in precedence order: the first accepted class decides "kept",
# the first present class decides which class a dropped file is charged to.
# Every class can be dropped, type-ref included (a foreign-qualified type row
# or a bare one with neither same-directory nor import evidence).
REF_ROW_CLASSES = ("qualified", "type-ref", "source-qualified", "receiver", "bare", "import-only")
# Row kinds by which a candidate file names a type: importing, extending, using
# a trait, type-hinting, or instantiating it. Any of these makes the type a
# plausible receiver owner for a method call in that file.
OWNER_EVIDENCE_KINDS = ("import", "include", "inheritance", "trait_use", "type_hint", "instantiation")
# Non-call rows that name the symbol itself (`extends Base`, `new Base`, a type
# hint). Qualified ones run `qualifier_accepted`; bare ones need an import row
# naming the type, or the defining file's directory for
# SAME_DIR_TYPE_LANGUAGES (`type_ref_accepted`).
TYPE_REF_KINDS = frozenset({"inheritance", "trait_use", "type_hint", "instantiation"})
# Method names that builtin containers / protocols also expose. A receiver
# call to one of these cannot be attributed to a user type by file-level
# owner evidence alone (the file may import the owner for unrelated reasons
# while `x.values()` targets a dict), so receiver-class acceptance is off.
BUILTIN_METHOD_NAMES: dict[str, frozenset[str]] = {
    "python": frozenset("""
        values items keys update get pop append extend insert remove clear copy sort index count
        join split strip format replace add read write close flush seek startswith endswith encode
        decode lower upper next send
    """.split()),
    "javascript": frozenset("""
        map filter forEach reduce push pop shift slice splice join then catch set get has add delete
        keys values entries toString
    """.split()),
    "rust": frozenset("""
        map filter collect iter into_iter next unwrap unwrap_or expect ok_or and_then or_else is_some
        is_none is_ok is_err as_ref as_mut clone len is_empty push pop insert remove get contains
    """.split()),
}
BUILTIN_METHOD_NAMES["typescript"] = BUILTIN_METHOD_NAMES["javascript"]

CONTAMINATION_NOTE = (
    "known-item cases reward name matching; treat as a secondary metric"
)
SATURATION_NOTE = (
    "known-item (--gold defining) is a saturation smoke test, not a ranking "
    "discriminator: decision metrics are miss rate and first-hit rank, not recall"
)
GOLD_CAP_NOTE = (
    "recall@k <= k/|gold|; gold is capped at 9 files for cochange (--max-files) "
    "and 10 for known-item references (MAX_REFERENCE_FILES)"
)

# Mirrors FileCategory::classify in crates/protocol/src/lib.rs for the Test
# arm only. Keep the two lists in step when either changes.
_TEST_DIR_SEGMENTS = ("test", "tests", "__tests__", "spec")
_TEST_SUFFIXES_LOWER = (
    ".test.ts", ".test.tsx", ".test.js", ".test.jsx",
    ".spec.ts", ".spec.tsx", ".spec.js", ".spec.jsx",
    "_test.py", "_test.go", ".phpt",
)
_TEST_SUFFIXES_EXACT = ("Test.php", "Test.java", "Tests.java")

_STOPWORDS = frozenset("""
a an the and or but if then else when while for to of in on at by with from
into onto over under about as is are was were be been being do does did
done has have had having it its this that these those there here not no
so than too very can could should would will shall may might must we you
they he she i our your their his her them us me my
""".split())

_CONVENTIONAL_PREFIX = re.compile(r"^([A-Za-z]+)(?:\(([^)]*)\))?!?:\s*")
_WORD = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_'\-]*")
_ALNUM_RUN = re.compile(r"[A-Za-z0-9]+")
_CAMEL_SPLIT = re.compile(r"(?<=[a-z0-9])(?=[A-Z])|(?<=[A-Z])(?=[A-Z][a-z])")
_QUALIFIER_SEP = re.compile(r"::|[./\\]")
_SOURCE_QUALIFIER = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)\s*(?:::|\\)\s*$")
_INCLUDE_SEGMENT_SEP = re.compile(r"::|[/\\.]")
_TRAILING_QUALIFIER_SEP = re.compile(r"(?:::|[./\\])$")


@dataclass(frozen=True)
class Case:
    id: str
    query: str
    expected_files: list[str]
    source: str
    defining_file: str | None = None

    def as_dict(self) -> dict:
        out = {
            "id": self.id,
            "query": self.query,
            "expected_files": list(self.expected_files),
            "source": self.source,
        }
        if self.defining_file is not None:
            out["defining_file"] = self.defining_file
        return out


@dataclass
class CochangeStats:
    scanned: int = 0
    in_scope: int = 0
    kept: int = 0
    skipped_type: int = 0
    skipped_short_subject: int = 0
    seed_by_subject: int = 0
    seed_by_symbols: int = 0


@dataclass
class KnownItemStats:
    eligible: int = 0
    ambiguous: int = 0
    doc_only: int = 0
    multi_definition: int = 0
    no_references: int = 0
    sampled: int = 0
    leaky: int = 0
    gate: "RefGateStats" = field(default_factory=lambda: RefGateStats())


@dataclass
class RefGateStats:
    """Per (language, row class) kept/dropped file counts for the references gate."""

    counts: dict[tuple[str, str, str], int] = field(default_factory=dict)

    def bump(self, language: str, cls: str, outcome: str) -> None:
        key = (language, cls, outcome)
        self.counts[key] = self.counts.get(key, 0) + 1

    def bump_verdict(self, language: str, verdict: "RefVerdict") -> None:
        self.bump(language, verdict.cls, "kept" if verdict.accepted else "dropped")
        if verdict.same_dir:
            self.bump(language, verdict.cls, "same-dir")

    def lines(self) -> list[str]:
        langs = sorted({k[0] for k in self.counts})
        out: list[str] = []
        for lang in langs:
            parts = []
            for cls in REF_ROW_CLASSES:
                kept = self.counts.get((lang, cls, "kept"), 0)
                dropped = self.counts.get((lang, cls, "dropped"), 0)
                same_dir = self.counts.get((lang, cls, "same-dir"), 0)
                if kept or dropped:
                    parts.append(f"{cls} {kept}/{dropped}" + (f" (same-dir {same_dir})" if same_dir else ""))
            out.append(f"{lang}: " + ", ".join(parts))
        return out


@dataclass(frozen=True)
class SymbolCandidate:
    path: str
    name: str
    query: str
    gold: tuple[str, ...]
    name_case: bool
    # (language, verdict) per corpus-relevant referencing file; counted into
    # the gate only when the candidate is sampled and emits a case.
    gate_rows: tuple[tuple[str, "RefVerdict"], ...] = ()


# ---------------------------------------------------------------------------
# Path filters
# ---------------------------------------------------------------------------

def is_test_path(path: str) -> bool:
    lower = path.lower()
    if lower.startswith("./"):
        lower = lower[2:]
    for seg in _TEST_DIR_SEGMENTS:
        if f"/{seg}/" in lower or lower.startswith(f"{seg}/"):
            return True
    if lower.endswith(_TEST_SUFFIXES_LOWER) or path.endswith(_TEST_SUFFIXES_EXACT):
        return True
    basename = lower.rsplit("/", 1)[-1]
    return basename.startswith("test_")


def glob_to_regex(pattern: str) -> re.Pattern[str]:
    """Translate a globset-style pattern (`**/tests/**`, `**/*Test.php`) to a regex.

    `**` spans directory separators; `*` and `?` do not. `{a,b}` expands to an
    alternation and `[...]` passes through as a character class (a leading
    `!` becomes `^`). A pattern without a leading `**/` is anchored at the
    path start, matching globset semantics.
    """
    return re.compile("^" + _glob_body_to_regex(pattern) + "$")


def _glob_body_to_regex(pattern: str) -> str:
    out: list[str] = []
    i = 0
    n = len(pattern)
    while i < n:
        c = pattern[i]
        if pattern.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif pattern.startswith("/**", i) and i + 3 == n:
            out.append("(?:/.*)?")
            i += 3
        elif pattern.startswith("**", i):
            out.append(".*")
            i += 2
        elif c == "*":
            out.append("[^/]*")
            i += 1
        elif c == "?":
            out.append("[^/]")
            i += 1
        elif c == "{":
            close = _matching_brace(pattern, i)
            if close is None:
                out.append(re.escape(c))
                i += 1
                continue
            branches = _split_alternation(pattern[i + 1:close])
            out.append("(?:" + "|".join(_glob_body_to_regex(b) for b in branches) + ")")
            i = close + 1
        elif c == "[":
            close = pattern.find("]", i + 2)
            if close == -1:
                out.append(re.escape(c))
                i += 1
                continue
            body = pattern[i + 1:close]
            if body.startswith("!"):
                body = "^" + body[1:]
            out.append("[" + body.replace("\\", "\\\\") + "]")
            i = close + 1
        else:
            out.append(re.escape(c))
            i += 1
    return "".join(out)


def _matching_brace(pattern: str, open_idx: int) -> int | None:
    depth = 0
    for j in range(open_idx, len(pattern)):
        if pattern[j] == "{":
            depth += 1
        elif pattern[j] == "}":
            depth -= 1
            if depth == 0:
                return j
    return None


def _split_alternation(body: str) -> list[str]:
    branches: list[str] = []
    depth = 0
    start = 0
    for j, ch in enumerate(body):
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
        elif ch == "," and depth == 0:
            branches.append(body[start:j])
            start = j + 1
    branches.append(body[start:])
    return branches


def _load_config(project: Path) -> dict:
    cfg = project / ".codesage" / "config.toml"
    if not cfg.is_file():
        return {}
    with cfg.open("rb") as fh:
        return tomllib.load(fh)


def load_exclude_patterns(project: Path) -> list[re.Pattern[str]]:
    patterns = _load_config(project).get("index", {}).get("exclude_patterns", [])
    return [glob_to_regex(p) for p in patterns if isinstance(p, str)]


def project_name(project: Path) -> str:
    name = _load_config(project).get("project", {}).get("name")
    if isinstance(name, str) and name:
        return name
    return project.name


def is_source_candidate(
    path: str,
    excludes: list[re.Pattern[str]],
    include_tests: bool,
) -> bool:
    if not include_tests and is_test_path(path):
        return False
    return not any(rx.match(path) for rx in excludes)


def is_markdown_path(path: str) -> bool:
    return path.lower().endswith(MARKDOWN_SUFFIXES)


# ---------------------------------------------------------------------------
# Index access
# ---------------------------------------------------------------------------

def open_index(project: Path) -> sqlite3.Connection:
    db = project / ".codesage" / "index.db"
    if not db.is_file():
        sys.exit(f"no index at {db}; onboard the project first")
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn


def symbol_counts_by_path(conn: sqlite3.Connection) -> dict[str, int]:
    rows = conn.execute(
        "SELECT files.path AS path, COUNT(symbols.id) AS n "
        "FROM files LEFT JOIN symbols ON symbols.file_id = files.id "
        "GROUP BY files.id"
    )
    return {r["path"]: r["n"] for r in rows}


def definition_file_counts(conn: sqlite3.Connection) -> dict[str, int]:
    rows = conn.execute(
        "SELECT name, COUNT(DISTINCT file_id) AS n FROM symbols GROUP BY name"
    )
    return {r["name"]: r["n"] for r in rows}


def file_symbol_names(conn: sqlite3.Connection, file_id: int) -> set[str]:
    rows = conn.execute("SELECT DISTINCT name FROM symbols WHERE file_id = ?", (file_id,))
    return {r["name"] for r in rows}


def _qualifier_of(to_name: str, tail: str) -> str | None:
    """Segment right before `tail` in a qualified reference, or None."""
    # Relies on refs.to_name_tail being the parser-computed final segment, so
    # slicing it off `to_name` leaves the qualifier path intact.
    if not to_name.endswith(tail) or to_name == tail:
        return None
    prefix = to_name[:-len(tail)]
    segments = [s for s in _QUALIFIER_SEP.split(prefix) if s]
    return segments[-1] if segments else None


def qualifier_accepted(
    qualifier: str | None,
    defining_path: str,
    file_symbols: set[str],
    language: str,
) -> bool:
    """Whether a tail-matched reference's qualifier plausibly names the defining file.

    Accepted: relative-path keywords (`super`, `crate`, `Self`; `self` only
    when the defining language is Rust, since Python's `self.foo()` is
    instance-relative), the defining file's stem, any directory segment of
    its path, a symbol defined in it, or a crate-style qualifier whose last
    `_`-separated component is a directory segment (`codesage_graph` for
    `crates/graph/src/...`). Foreign type qualifiers (`UnixStream`, `File`,
    `fs`) fail all of these.
    """
    if not qualifier:
        return False
    if qualifier == "self":
        return language == "rust"
    if qualifier in RELATIVE_PATH_QUALIFIERS or qualifier in file_symbols:
        return True
    p = Path(defining_path)
    dir_segments = {seg for seg in p.parent.parts if seg not in ("", ".")}
    if qualifier == p.stem or qualifier in dir_segments:
        return True
    return qualifier.rsplit("_", 1)[-1] in dir_segments


def binds_name_locally(name: str, text: str) -> bool:
    """True when `text` binds `name` to a callable of its own.

    Best effort over the enumerated forms below (closure, nested fn, def,
    lambda, arrow, const/static callable). A miss keeps a real gold file; a
    comment or string mentioning `fn name(` can drop one.
    """
    n = re.escape(name)
    alternatives = (
        rf"\blet\s+(?:mut\s+)?{n}\s*(?::[^=;]+)?=\s*(?:move\s+)?\|",   # Rust closure
        rf"\bfn\s+{n}\b",                                             # nested fn
        rf"\bdef\s+{n}\b",                                            # Python def
        rf"\bfunction\s+{n}\b",                                       # JS/PHP function
        rf"\b{n}\s*=\s*(?:function|lambda)\b",                        # assigned callable
        rf"\b{n}\s*:=\s*func\b",                                      # Go func literal
        rf"\b(?:const|static)\s+{n}\s*:",                             # Rust const/static
        rf"\bconst\s+{n}\s*=\s*(?:async\s+)?\(?[^;\n]*=>",            # JS arrow
        rf"\b{n}\s*:\s*Callable\b[^=\n]*=",                           # Python typed callable
    )
    return re.search("|".join(alternatives), text) is not None


@functools.lru_cache(maxsize=512)
def _file_text(project: Path, path: str) -> str | None:
    try:
        return (project / path).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None


@functools.lru_cache(maxsize=512)
def _file_lines(project: Path, path: str) -> tuple[str, ...] | None:
    text = _file_text(project, path)
    return None if text is None else tuple(split_source_lines(text))


@functools.lru_cache(maxsize=4096)
def _file_imports(conn: sqlite3.Connection, file_id: int) -> tuple[frozenset[str], tuple[str, ...]]:
    """(import tails, import to_names) recorded for one file."""
    rows = conn.execute(
        "SELECT to_name, to_name_tail FROM refs WHERE from_file_id = ? AND kind IN ('import', 'include')",
        (file_id,),
    ).fetchall()
    return frozenset(r["to_name_tail"] for r in rows), tuple(r["to_name"] for r in rows)


@functools.lru_cache(maxsize=4096)
def _file_symbols_cached(conn: sqlite3.Connection, file_id: int) -> frozenset[str]:
    return frozenset(file_symbol_names(conn, file_id))


@functools.lru_cache(maxsize=4096)
def _file_type_mentions(conn: sqlite3.Connection, file_id: int) -> frozenset[str]:
    """Type names a file imports, extends, uses as a trait, type-hints, or instantiates."""
    placeholders = ",".join("?" for _ in OWNER_EVIDENCE_KINDS)
    rows = conn.execute(
        f"SELECT to_name_tail FROM refs WHERE from_file_id = ? AND kind IN ({placeholders})",
        (file_id, *OWNER_EVIDENCE_KINDS),
    ).fetchall()
    return frozenset(r["to_name_tail"] for r in rows)


def owner_type(qualified_name: str, name: str) -> str | None:
    """Segment before `name` in the definition's qualified name, generics stripped.

    `Database::name` -> `Database`; `MapperContext<'a>::name` -> `MapperContext`;
    `Foo<Bar<Baz>>::name` -> `Foo`; `Vec<crate::Foo>::name` -> `Vec`;
    `Ns\\Base\\name` -> `Base`; a free function has no owner.
    """
    segments = [s for s in _QUALIFIER_SEP.split(_strip_angle_spans(qualified_name)) if s]
    if len(segments) < 2 or segments[-1] != name:
        return None
    return segments[-2].strip() or None


def _strip_angle_spans(text: str) -> str:
    """Remove balanced `<...>` spans (nested) so generic arguments never split a path."""
    out: list[str] = []
    depth = 0
    for ch in text:
        if ch == "<":
            depth += 1
        elif ch == ">" and depth > 0:
            depth -= 1
        elif depth == 0:
            out.append(ch)
    return "".join(out)


def call_shape(lines: tuple[str, ...] | None, line: int, col: int, name: str) -> tuple[str, str | None]:
    """Classify a bare call row from the source text at (1-based line, 0-based col).

    Returns ("receiver", None) when `.` or `->` precedes the token,
    ("source-qualified", qualifier) when `::` or `\\` precedes it, otherwise
    ("bare", None). A stale position that does not land on `name` is "bare".
    """
    if lines is None or not 1 <= line <= len(lines):
        return "bare", None
    text = lines[line - 1]
    end = col + len(name)
    if not text.startswith(name, col) or (end < len(text) and (text[end].isalnum() or text[end] == "_")):
        return "bare", None
    prefix = text[:col].rstrip()
    if prefix.endswith(RECEIVER_SUFFIXES):
        return "receiver", None
    m = _SOURCE_QUALIFIER.search(prefix)
    if m:
        return "source-qualified", m.group(1)
    return "bare", None


def _include_segments(to_name: str) -> list[str]:
    return [s for s in _INCLUDE_SEGMENT_SEP.split(to_name) if s and s not in (".", "..")]


def bare_call_accepted(
    language: str,
    candidate_path: str,
    imports: tuple[frozenset[str], tuple[str, ...]],
    name: str,
    defining_path: str,
    file_symbols: set[str],
) -> bool:
    """Rule (c): a truly bare call needs an import row naming the symbol.

    C/C++ and Go have no name-level imports, so there the candidate is also
    accepted when it lives in the defining file's directory (same package /
    translation-unit neighbourhood) or holds an include/import row with a
    path segment that passes `qualifier_accepted` for the defining file.
    """
    tails, to_names = imports
    if name in tails:
        return True
    if language not in NEIGHBOURHOOD_LANGUAGES:
        return False
    if Path(candidate_path).parent == Path(defining_path).parent:
        return True
    return any(
        qualifier_accepted(seg, defining_path, file_symbols, language)
        for to_name in to_names
        for seg in _include_segments(to_name)
    )


def import_names_type(
    imports: tuple[frozenset[str], tuple[str, ...]],
    name: str,
    defining_path: str,
    file_symbols: set[str],
    language: str,
) -> bool:
    """Whether an import row resolves `name` to the defining file.

    A qualified import (`use Ns\\Message`, `use crate::a_mod::A`) counts only
    when the segment before the name passes `qualifier_accepted`; a tail-only
    match would let `use Other\\Message` in a far directory stand in for
    `Ns\\Message`. Languages whose import rows carry no module path (Python
    `from x import Y`, JS/TS named imports) can only be tail-checked; there a
    bare import row (`to_name == name`) counts. A longer identifier ending in
    the name (`LogMessage`) never matches.
    """
    for to_name in imports[1]:
        if to_name == name:
            return True
        if (
            to_name.endswith(name)
            and _TRAILING_QUALIFIER_SEP.search(to_name[:-len(name)])
            and qualifier_accepted(_qualifier_of(to_name, name), defining_path, file_symbols, language)
        ):
            return True
    return False


def type_ref_accepted(
    to_name: str,
    tail: str,
    name: str,
    candidate_path: str,
    imports: tuple[frozenset[str], tuple[str, ...]],
    defining_path: str,
    file_symbols: set[str],
    language: str,
) -> str | None:
    """How a type row (`extends`, trait use, type hint, `new`) names the definition.

    Returns "qualified", "import", or "same-dir" for an accepted row, None for
    a dropped one. A qualified row (`\\Gelf\\Message`, `std::runtime_error`)
    counts only when its qualifier passes `qualifier_accepted`; otherwise a
    same-tail type from another package would become gold. A bare row counts
    when the candidate holds an import row resolving the type to the defining
    file (`import_names_type`), or, for SAME_DIR_TYPE_LANGUAGES only, when it
    shares the defining file's directory (namespace / package by convention).
    The directory arm is skipped when an import row's tail equals the name
    without resolving to the defining file: `use Gelf\\Message;` in the
    defining directory shadows the local class.
    """
    if to_name != name:
        ok = qualifier_accepted(_qualifier_of(to_name, tail), defining_path, file_symbols, language)
        return "qualified" if ok else None
    if import_names_type(imports, name, defining_path, file_symbols, language):
        return "import"
    if name in imports[0]:
        return None
    if language in SAME_DIR_TYPE_LANGUAGES and Path(candidate_path).parent == Path(defining_path).parent:
        return "same-dir"
    return None


@dataclass(frozen=True)
class RefVerdict:
    path: str
    cls: str
    accepted: bool
    # True when the file is kept by a type-ref row whose only evidence is the
    # shared directory: no accepted type-ref row via qualifier or import, and
    # no other accepted row class.
    same_dir: bool = False


def reference_verdicts(
    conn: sqlite3.Connection,
    project: Path,
    name: str,
    defining_file_id: int,
    defining_path: str,
    defining_qualified: str,
    language: str,
) -> list[RefVerdict]:
    """Per-file verdicts for same-language files that reference `name`.

    Type rows (`extends`, trait use, type hint, instantiation) naming the
    symbol count when qualified and the qualifier passes `qualifier_accepted`,
    or bare and the file imports the type from the defining file, or (PHP,
    Go, C/C++ only) shares the defining directory (`type_ref_accepted`; a
    kept file resting on the directory alone is flagged `same_dir` for the
    gate sub-count). Call rows recorded with a qualifier count
    when the qualifier passes `qualifier_accepted`. Call rows recorded bare
    are classified from the source text at their line/col: (a) receiver
    calls (`x.name()`, `x->name()`) count only when the definition's owner
    type is named by the candidate file (imported, extended, trait-used,
    type-hinted, instantiated, or defined there; see OWNER_EVIDENCE_KINDS)
    and the method name is not a builtin-protocol name for the language
    (BUILTIN_METHOD_NAMES), so a parent reached only through a grandparent
    stays dropped; (b) calls qualified in source but recorded bare
    (`A::name()`, `A\\name()`) recover the qualifier and run
    `qualifier_accepted`; (c) truly bare calls need an import row naming the
    symbol (C/C++/Go: or the same directory / an include row naming the
    defining file, see `bare_call_accepted`), and are still dropped when the
    file binds the name locally (`binds_name_locally`). Files whose only rows
    are `import` / `include` are dropped. Callers guarantee the name has
    exactly one defining file; otherwise exact-name rows would collect
    homonym callers. Glob imports are recorded as the module path
    (`use a::b::*` -> `a::b`) or not at all (`use super::*`), so a caller
    that reaches the symbol only through one is conservatively dropped
    (cs-zz6). The caller filters test/excluded paths and counts the gate.
    """
    rows = conn.execute(
        "SELECT files.id AS fid, files.path AS path, refs.to_name AS to_name, "
        "refs.to_name_tail AS tail, refs.kind AS kind, refs.line AS line, refs.col AS col "
        "FROM refs JOIN files ON files.id = refs.from_file_id "
        "WHERE (refs.to_name = ? OR refs.to_name_tail = ?) "
        "AND refs.from_file_id != ? AND files.language = ? "
        "ORDER BY files.path",
        (name, name, defining_file_id, language),
    ).fetchall()
    if not rows:
        return []
    file_symbols = file_symbol_names(conn, defining_file_id)
    owner = owner_type(defining_qualified, name)
    builtin = name in BUILTIN_METHOD_NAMES.get(language, frozenset())
    # path -> {class: accepted?}; the best class decides the outcome.
    verdicts: dict[str, dict[str, bool]] = {}
    # path -> type-ref acceptance reasons seen ("qualified" / "import" / "same-dir").
    type_reasons: dict[str, set[str]] = {}
    for r in rows:
        path = r["path"]
        if r["kind"] in REEXPORT_REF_KINDS:
            verdicts.setdefault(path, {}).setdefault("import-only", False)
            continue
        if r["kind"] in TYPE_REF_KINDS:
            cls = "type-ref"
            reason = type_ref_accepted(
                r["to_name"], r["tail"], name, path, _file_imports(conn, r["fid"]),
                defining_path, file_symbols, language,
            )
            ok = reason is not None
            if reason:
                type_reasons.setdefault(path, set()).add(reason)
        elif r["to_name"] != name:
            ok = qualifier_accepted(_qualifier_of(r["to_name"], r["tail"]), defining_path, file_symbols, language)
            cls = "qualified"
        else:
            cls, qualifier = call_shape(_file_lines(project, path), r["line"], r["col"], name)
            imports = _file_imports(conn, r["fid"])
            if cls == "receiver":
                ok = owner is not None and not builtin and (
                    owner in _file_type_mentions(conn, r["fid"]) or owner in _file_symbols_cached(conn, r["fid"])
                )
            elif cls == "source-qualified":
                ok = qualifier_accepted(qualifier, defining_path, file_symbols, language)
            else:
                ok = bare_call_accepted(language, path, imports, name, defining_path, file_symbols)
                if ok:
                    text = _file_text(project, path)
                    ok = text is None or not binds_name_locally(name, text)
        cell = verdicts.setdefault(path, {})
        cell[cls] = cell.get(cls, False) or ok
    out: list[RefVerdict] = []
    for path, cell in sorted(verdicts.items()):
        accepted = [cls for cls in REF_ROW_CLASSES if cell.get(cls)]
        deciding = accepted[0] if accepted else next(cls for cls in REF_ROW_CLASSES if cls in cell)
        same_dir = accepted == ["type-ref"] and type_reasons.get(path) == {"same-dir"}
        out.append(RefVerdict(path, deciding, bool(accepted), same_dir))
    return out


def referencing_paths(
    conn: sqlite3.Connection,
    project: Path,
    name: str,
    defining_file_id: int,
    defining_path: str,
    defining_qualified: str,
    language: str,
) -> list[str]:
    """Accepted referencing files (see `reference_verdicts`)."""
    return [v.path for v in reference_verdicts(
        conn, project, name, defining_file_id, defining_path, defining_qualified, language,
    ) if v.accepted]


# ---------------------------------------------------------------------------
# Mode: cochange
# ---------------------------------------------------------------------------

def _git(project: Path, args: list[str]) -> subprocess.CompletedProcess | None:
    try:
        return subprocess.run(
            ["git", *args], cwd=project, capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=GIT_TIMEOUT_SECS,
        )
    except subprocess.TimeoutExpired:
        print(f"[self-eval] warn: git timed out after {GIT_TIMEOUT_SECS}s in {project}", file=sys.stderr)
        return None
    except OSError as e:
        print(f"[self-eval] warn: git unavailable ({e}); git-derived fields degrade", file=sys.stderr)
        return None


def git_log_commits(
    project: Path, commits: int,
) -> tuple[list[tuple[str, str, list[str]]], int] | None:
    """Return ([(sha, subject, changed_paths)], window_size) or None when git fails.

    The window is the last N non-merge commits of the repository; the walk
    is then scoped with the pathspec `-- .` so only commits touching the
    project directory are returned, with `--relative` paths that match the
    index when the project root is a git subdirectory.
    """
    rev = _git(project, ["rev-list", "--no-merges", f"-n{commits}", "HEAD"])
    if rev is None or rev.returncode != 0:
        detail = rev.stderr.strip() if rev is not None else "timeout"
        print(f"[cochange] warn: git rev-list failed in {project}: {detail}", file=sys.stderr)
        return None
    window = set(rev.stdout.split())
    log = _git(project, [
        "log", "--name-only", "--no-merges", "--relative", f"-n{commits}",
        "--format=%x1e%H%x1f%s", "--", ".",
    ])
    if log is None or log.returncode != 0:
        detail = log.stderr.strip() if log is not None else "timeout"
        print(f"[cochange] warn: git log failed in {project}: {detail}", file=sys.stderr)
        return None
    out: list[tuple[str, str, list[str]]] = []
    for record in log.stdout.split("\x1e"):
        if not record.strip():
            continue
        head, _, body = record.partition("\n")
        sha, _, subject = head.partition("\x1f")
        sha = sha.strip()
        if sha not in window:
            continue
        paths = [line.strip() for line in body.splitlines() if line.strip()]
        out.append((sha, subject.strip(), paths))
    return out, len(window)


def conventional_type(subject: str) -> str | None:
    m = _CONVENTIONAL_PREFIX.match(subject)
    return m.group(1).lower() if m else None


def _subject_match_text(subject: str) -> str:
    m = _CONVENTIONAL_PREFIX.match(subject)
    if not m:
        return subject
    scope = m.group(2) or ""
    return f"{scope} {subject[m.end():]}"


def subject_content_words(subject: str) -> list[str]:
    body = _CONVENTIONAL_PREFIX.sub("", subject, count=1)
    return [w for w in _WORD.findall(body) if w.lower() not in _STOPWORDS]


def _subject_match_len(path: str, subject_lower: str) -> int:
    """Longest of {path stem, parent dir name} that occurs word-bounded in the subject."""
    p = Path(path)
    best = 0
    for candidate in (p.stem, p.parent.name):
        cand = candidate.lower()
        if not cand or len(cand) <= best:
            continue
        if re.search(rf"(?<![a-z0-9_]){re.escape(cand)}(?![a-z0-9_])", subject_lower):
            best = len(cand)
    return best


def choose_seed(
    candidates: list[str],
    subject: str,
    symbol_counts: dict[str, int],
) -> tuple[str, str]:
    """Return (seed_path, rule) with rule in {"subject", "symbols"}.

    Matching runs on the conventional scope plus the subject body: the type
    token is dropped so `docs:` / `build:` / `ci:` never match a directory of
    the same name, while `fix(mcp):` still names the `mcp/` directory. Equal
    match lengths resolve to the lexicographically smallest path; the
    symbol-count fallback resolves ties to the lexicographically largest.
    """
    subject_lower = _subject_match_text(subject).lower()
    scored = [(_subject_match_len(p, subject_lower), p) for p in candidates]
    best_len = max(n for n, _ in scored)
    if best_len > 0:
        seed = min(p for n, p in scored if n == best_len)
        return seed, "subject"
    seed = max(candidates, key=lambda p: (symbol_counts[p], p))
    return seed, "symbols"


def build_cochange_cases(
    commits: list[tuple[str, str, list[str]]],
    symbol_counts: dict[str, int],
    excludes: list[re.Pattern[str]],
    *,
    min_files: int,
    max_files: int,
    include_tests: bool,
    include_all_types: bool,
    stats: CochangeStats,
) -> list[Case]:
    cases: list[Case] = []
    for sha, subject, paths in commits:
        stats.in_scope += 1
        if not subject:
            continue
        candidates = sorted({
            p for p in paths
            if p in symbol_counts and is_source_candidate(p, excludes, include_tests)
        })
        if not (min_files <= len(candidates) <= max_files):
            continue
        if len(subject_content_words(subject)) < MIN_SUBJECT_CONTENT_WORDS:
            stats.skipped_short_subject += 1
            continue
        seed, rule = choose_seed(candidates, subject, symbol_counts)
        expected = [p for p in candidates if p != seed]
        if not include_all_types:
            ctype = conventional_type(subject)
            if ctype in SKIPPED_COMMIT_TYPES or (
                ctype == DOCS_COMMIT_TYPE and all(is_markdown_path(p) for p in expected)
            ):
                stats.skipped_type += 1
                continue
        stats.kept += 1
        if rule == "subject":
            stats.seed_by_subject += 1
        else:
            stats.seed_by_symbols += 1
        cases.append(Case(
            id=f"cc-{sha[:12]}",
            query=subject,
            expected_files=expected,
            source=f"cochange:{sha}",
        ))
    return cases


# ---------------------------------------------------------------------------
# Mode: known-item — comment extraction
# ---------------------------------------------------------------------------

_ATTRIBUTE_PREFIXES = ("#[", "#![", "@")
_SLASH_COMMENT_PREFIXES = ("///", "//!", "//")
_DASH_COMMENT_PREFIX = "--"
_TRIPLE_QUOTES = ('"""', "'''")
_TAG_LINE = re.compile(r"^(?:@\w+|\\\w+|:(?:param|type|return|returns|rtype|raises|yields)\b)")
_BLOCK_LINE = re.compile(r"^\s*(/\*|\*|\*/)")
_PY_DEF = re.compile(r"^\s*(?:async\s+)?(?:def|class)\b")
_RUST_MOD_OPEN = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")
# `test` as a bare cfg predicate token: not inside `not(test)`, not part of a
# string such as feature = "test-utils".
_CFG_TEST_ATTR = re.compile(
    r"""^#\[cfg\((?!.*\bnot\s*\(\s*test\b).*(?<![\w"'\-])test(?![\w"'\-]).*\)\]"""
)
_DOCSTRING_SCAN_LINES = 8


def split_source_lines(text: str) -> list[str]:
    """Split on `\\n` only, matching tree-sitter's line numbering."""
    return text.split("\n")


def _is_attribute_line(stripped: str) -> bool:
    return stripped.startswith(_ATTRIBUTE_PREFIXES)


def _is_line_comment(stripped: str, language: str) -> bool:
    if _is_attribute_line(stripped):
        return False
    if stripped.startswith(_SLASH_COMMENT_PREFIXES):
        return True
    if stripped.startswith(_DASH_COMMENT_PREFIX):
        return language in DASH_COMMENT_LANGUAGES
    return stripped.startswith("#") and language in HASH_COMMENT_LANGUAGES


def _strip_comment_markers(raw: str, language: str) -> str:
    s = raw.strip()
    for marker in ("/**", "/*!", "/*"):
        if s.startswith(marker):
            s = s[len(marker):]
            break
    if s.endswith("*/"):
        s = s[:-2]
    s = s.strip()
    markers = ["///", "//!", "//", "#", "*"]
    if language in DASH_COMMENT_LANGUAGES:
        markers.append(_DASH_COMMENT_PREFIX)
    for marker in markers:
        if s.startswith(marker):
            s = s[len(marker):]
            break
    return s.strip()


def _clean_comment_lines(raw_lines: list[str], language: str) -> str:
    """Strip markers, drop `@tag` / `\\tag` / Sphinx field lines, join to one text."""
    kept: list[str] = []
    for raw in raw_lines:
        text = _strip_comment_markers(raw, language)
        if not text:
            continue
        if _TAG_LINE.match(text):
            continue
        kept.append(text)
    return " ".join(kept).strip()


def _comment_block_above(lines: list[str], def_idx: int, language: str) -> list[str]:
    """Return the raw comment lines directly above `def_idx`, top to bottom.

    Blank lines and attribute / decorator lines between the comment and the
    definition are skipped; the block itself must be contiguous. A `/* */`
    block qualifies only when every one of its lines starts with `/*`, `*`,
    or `*/`, which rejects trailing comments on code lines and commented-out
    code.
    """
    i = def_idx - 1
    while i >= 0 and (not lines[i].strip() or _is_attribute_line(lines[i].strip())):
        i -= 1
    if i < 0:
        return []
    stripped = lines[i].strip()
    if stripped.endswith("*/"):
        end = i
        while i >= 0 and "/*" not in lines[i]:
            i -= 1
        if i < 0:
            return []
        block = lines[i:end + 1]
        return block if all(_BLOCK_LINE.match(line) for line in block) else []
    if not _is_line_comment(stripped, language):
        return []
    end = i
    while i >= 0 and _is_line_comment(lines[i].strip(), language):
        i -= 1
    return lines[i + 1:end + 1]


def _indent(line: str) -> int:
    return len(line) - len(line.lstrip())


def _python_docstring(lines: list[str], def_idx: int) -> list[str]:
    """Return the raw lines of a docstring that opens right after the signature.

    The signature scan stops at a second `def` / `class` or at a dedent back
    to the definition's indentation (a closing `)` line is still signature).
    """
    def_indent = _indent(lines[def_idx])
    limit = min(len(lines), def_idx + 1 + _DOCSTRING_SCAN_LINES)
    sig_end: int | None = None
    for i in range(def_idx, limit):
        line = lines[i]
        if i > def_idx and line.strip():
            if _PY_DEF.match(line):
                return []
            if _indent(line) <= def_indent and not line.lstrip().startswith(")"):
                return []
        if line.rstrip().endswith(":"):
            sig_end = i
            break
    if sig_end is None:
        return []
    i = sig_end + 1
    while i < len(lines) and not lines[i].strip():
        i += 1
    if i >= len(lines):
        return []
    body = lines[i].strip().lstrip("rRbBuU")
    quote = next((q for q in _TRIPLE_QUOTES if body.startswith(q)), None)
    if quote is None:
        return []
    if body.count(quote) >= 2:
        return [body[len(quote):].split(quote, 1)[0]]
    out = [body[len(quote):]]
    for j in range(i + 1, len(lines)):
        if quote in lines[j]:
            out.append(lines[j].split(quote, 1)[0])
            return out
        out.append(lines[j])
    return []


def source_comment(lines: list[str], line_start: int, language: str) -> str | None:
    """Doc text for the symbol defined at 1-based `line_start`, or None."""
    def_idx = line_start - 1
    if not 0 <= def_idx < len(lines):
        return None
    raw: list[str] = []
    if language == "python":
        raw = _python_docstring(lines, def_idx)
    if not raw:
        raw = _comment_block_above(lines, def_idx, language)
    if not raw:
        return None
    text = _clean_comment_lines(raw, language)
    if text.lower().startswith(LICENSE_PREFIXES):
        return None
    if len(text.split()) < MIN_COMMENT_WORDS:
        return None
    return text


def rust_test_module_ranges(lines: list[str]) -> list[tuple[int, int]]:
    """1-based inclusive line ranges of `#[cfg(test)] mod x { ... }` blocks.

    The attribute may be any `#[cfg(...)]` whose predicate mentions `test`
    (`#[cfg(all(test, unix))]`), may share its line with `mod x {`, and
    blank, attribute, and `//` comment lines may sit between the attribute
    and the `mod` line. Best effort: braces are counted textually, so a
    brace inside a string or comment can shift the range end.
    """
    ranges: list[tuple[int, int]] = []
    for i, line in enumerate(lines):
        m = _CFG_TEST_ATTR.match(line.strip())
        if not m:
            continue
        rest = line.strip()[m.end():].strip()
        if rest:
            if not _RUST_MOD_OPEN.match(rest):
                continue
            j = i
        else:
            j = i + 1
            while j < len(lines):
                s = lines[j].strip()
                if s and not _is_attribute_line(s) and not s.startswith(_SLASH_COMMENT_PREFIXES):
                    break
                j += 1
            if j >= len(lines) or not _RUST_MOD_OPEN.match(lines[j]):
                continue
        depth = 0
        end = None
        for k in range(j, len(lines)):
            depth += lines[k].count("{") - lines[k].count("}")
            if depth <= 0:
                end = k
                break
        ranges.append((i + 1, (end if end is not None else len(lines) - 1) + 1))
    return ranges


def doc_query(comment: str) -> str:
    first_sentence = re.split(r"(?<=[.!?])\s+", comment.strip(), maxsplit=1)[0]
    tokens = _WORD.findall(first_sentence)
    kept = [t for t in tokens if t.lower() not in _STOPWORDS]
    return " ".join(kept[:MAX_DOC_QUERY_WORDS])


def name_tokens(name: str) -> list[str]:
    parts: list[str] = []
    for run in _ALNUM_RUN.findall(name):
        parts.extend(p.lower() for p in _CAMEL_SPLIT.split(run) if p)
    return parts


def is_leaky_query(name: str, query: str) -> bool:
    """True when every alphanumeric token of the symbol name appears as a whole word."""
    tokens = name_tokens(name)
    haystack = query.lower()
    return bool(tokens) and all(
        re.search(rf"(?<![a-z0-9]){re.escape(t)}(?![a-z0-9])", haystack) for t in tokens
    )


def stable_case_id(path: str, name: str) -> str:
    digest = hashlib.sha1(f"{path}\0{name}".encode("utf-8")).hexdigest()
    return f"ki-{digest[:8]}"


# ---------------------------------------------------------------------------
# Mode: known-item — candidate selection
# ---------------------------------------------------------------------------

class SourceCache:
    """Holds the lines of one path at a time; rows arrive ordered by path."""

    def __init__(self, project: Path) -> None:
        self.project = project
        self._path: str | None = None
        self._lines: list[str] | None = None
        self._test_ranges: list[tuple[int, int]] = []

    def load(self, path: str, language: str) -> list[str] | None:
        if path != self._path:
            self._path = path
            try:
                text = (self.project / path).read_text(encoding="utf-8", errors="replace")
                self._lines = split_source_lines(text)
            except OSError:
                self._lines = None
            self._test_ranges = (
                rust_test_module_ranges(self._lines)
                if self._lines is not None and language == "rust" else []
            )
        return self._lines

    def in_test_module(self, line_start: int) -> bool:
        return any(lo <= line_start <= hi for lo, hi in self._test_ranges)


def eligible_symbols(
    conn: sqlite3.Connection,
    project: Path,
    excludes: list[re.Pattern[str]],
    *,
    gold: str,
    include_tests: bool,
    stats: KnownItemStats,
) -> list[SymbolCandidate]:
    """Return candidates ordered by (path, line_start, name).

    Symbols are deduplicated to MIN(line_start) per (file_id, name) so usage
    rows recorded as symbols never stand in for the definition.
    """
    def_counts = definition_file_counts(conn)
    placeholders = ",".join("?" for _ in KNOWN_ITEM_KINDS)
    rows = conn.execute(
        "SELECT files.id AS file_id, files.path AS path, files.language AS language, "
        "symbols.name AS name, MIN(symbols.line_start) AS line_start, "
        "symbols.qualified_name AS qualified_name "
        "FROM symbols JOIN files ON symbols.file_id = files.id "
        f"WHERE symbols.kind IN ({placeholders}) "
        "GROUP BY files.id, symbols.name "
        "ORDER BY files.path, line_start, symbols.name",
        KNOWN_ITEM_KINDS,
    )
    cache = SourceCache(project)
    out: list[SymbolCandidate] = []
    for r in rows:
        if not is_source_candidate(r["path"], excludes, include_tests):
            continue
        defining_files = def_counts.get(r["name"], 0)
        if defining_files > MAX_DEFINITION_FILES:
            stats.ambiguous += 1
            continue
        lines = cache.load(r["path"], r["language"])
        if lines is None or cache.in_test_module(r["line_start"]):
            continue
        comment = source_comment(lines, r["line_start"], r["language"])
        if comment is None:
            continue
        query = doc_query(comment)
        if len(query.split()) < MIN_DOC_QUERY_TOKENS:
            continue
        if gold == "references":
            if defining_files != 1:
                stats.multi_definition += 1
                continue
            verdicts = [
                v for v in reference_verdicts(
                    conn, project, r["name"], r["file_id"], r["path"], r["qualified_name"], r["language"],
                )
                if is_source_candidate(v.path, excludes, include_tests)
            ]
            refs = [v.path for v in verdicts if v.accepted]
            if not refs or len(refs) > MAX_REFERENCE_FILES:
                stats.no_references += 1
                continue
            gate_rows = tuple((r["language"], v) for v in verdicts)
            expected = tuple(refs)
        else:
            gate_rows = ()
            expected = (r["path"],)
        name_case = defining_files == 1
        if not name_case:
            stats.doc_only += 1
        out.append(SymbolCandidate(r["path"], r["name"], query, expected, name_case, gate_rows))
    stats.eligible = len(out)
    return out


def build_known_item_cases(
    symbols: list[SymbolCandidate],
    *,
    sample: int,
    seed: int,
    stats: KnownItemStats,
    carry_defining_file: bool = False,
) -> list[Case]:
    rng = random.Random(seed)
    chosen = rng.sample(symbols, min(sample, len(symbols)))
    stats.sampled = len(chosen)
    cases: list[Case] = []
    for sym in chosen:
        for language, verdict in sym.gate_rows:
            stats.gate.bump_verdict(language, verdict)
        base = stable_case_id(sym.path, sym.name)
        leaky = is_leaky_query(sym.name, sym.query)
        if leaky:
            stats.leaky += 1
        defining = sym.path if carry_defining_file else None
        if sym.name_case:
            cases.append(Case(
                id=f"{base}-name",
                query=sym.name,
                expected_files=list(sym.gold),
                source="known-item:name",
                defining_file=defining,
            ))
        cases.append(Case(
            id=f"{base}-doc",
            query=sym.query,
            expected_files=list(sym.gold),
            source="known-item:doc-leaky" if leaky else "known-item:doc",
            defining_file=defining,
        ))
    return cases


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------

def _first_line(text: str) -> str:
    return text.strip().splitlines()[0] if text.strip() else ""


def index_digest(conn: sqlite3.Connection) -> str:
    """sha256 over sorted (path, content_hash) plus files/symbols/refs row counts."""
    h = hashlib.sha256()
    for r in conn.execute("SELECT path, content_hash FROM files ORDER BY path"):
        h.update(f"{r['path']}\0{r['content_hash']}\n".encode("utf-8"))
    counts = [conn.execute(f"SELECT COUNT(*) FROM {t}").fetchone()[0] for t in ("files", "symbols", "refs")]
    h.update(("|".join(str(c) for c in counts)).encode("utf-8"))
    return f"{h.hexdigest()[:16]} (files={counts[0]}, symbols={counts[1]}, refs={counts[2]})"


def script_revision(script: Path) -> str:
    """HEAD of the script's repo; `-dirty+sha256:<8>` appended when the script has local edits."""
    script = script.resolve()
    rev = _git(script.parent, ["rev-parse", "HEAD"])
    digest = hashlib.sha256(script.read_bytes()).hexdigest()
    if rev is None or rev.returncode != 0:
        return "sha256:" + digest[:16]
    status = _git(script.parent, ["status", "--porcelain", "--", str(script)])
    dirty = status is None or status.returncode != 0 or bool(status.stdout.strip())
    return rev.stdout.strip() + (f"-dirty+sha256:{digest[:8]}" if dirty else "")


def provenance_lines(project: Path, conn: sqlite3.Connection) -> list[str]:
    head = _git(project, ["rev-parse", "HEAD"])
    project_head = head.stdout.strip() if head is not None and head.returncode == 0 else "not-a-git-repo"
    script_rev = script_revision(Path(__file__))
    try:
        ver = subprocess.run(["codesage", "--version"], capture_output=True, text=True, timeout=15)
        codesage_version = _first_line(ver.stdout) if ver.returncode == 0 else "unknown"
    except (OSError, subprocess.SubprocessError):
        codesage_version = "unknown"
    return [
        f"generated: {_dt.datetime.now(_dt.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')}",
        f"project_head: {project_head}",
        f"codesage: {codesage_version}",
        f"script_revision: {script_rev}",
        f"index_digest: {index_digest(conn)}",
    ]


def render_corpus(
    project: Path,
    mode: str,
    cases: list[Case],
    summary_lines: list[str],
    *,
    gold: str = "defining",
    provenance: list[str] | None = None,
    gate_lines: list[str] | None = None,
) -> str:
    header = [
        f"# Self-hosting eval corpus ({mode}) generated by bench/self-eval.py",
        f"# project: {project}",
        *[f"# {line}" for line in (provenance or [])],
        *[f"# {line}" for line in summary_lines],
        "#",
        "# Scoring: run with bench/codesage-bench-runner. `codesage search --json`",
        "# returns chunk rows in engine order; the runner's parse_results dedupes",
        "# them by file_path, so first_hit_rank is the 1-based rank of the first",
        "# expected file among DISTINCT files, not among emitted rows. It never",
        "# reads the row `score`, so equal-score rows keep emission order (no",
        "# midrank tie rule). Compare runs at the same limit only.",
        f"# Gold cap: {GOLD_CAP_NOTE}.",
    ]
    if mode == "cochange":
        header += [
            "# Seed rule: the changed file whose path stem or parent-dir name occurs",
            "# in the subject scope or body (word-bounded, longest match, ties to the",
            "# lexicographically smallest path) is dropped from gold; with no match,",
            "# the file with the most symbols (ties to the largest path) is dropped.",
            "# Subject filter: conventional types style/chore/ci/build/deps/release",
            "# skipped, docs skipped unless a non-markdown gold file remains, and",
            "# subjects with < 3 content words after stopword strip skipped.",
        ]
    if mode == "known-item":
        header += [
            f"# gold: {gold}",
            f"# Contamination: {CONTAMINATION_NOTE}.",
            "# Doc cases whose query contains every token of the symbol name carry",
            "# source `known-item:doc-leaky`; filter them out for the non-leaky arm.",
        ]
        if gold == "defining":
            header += [
                "# Names defined in 2..3 files emit only the doc case (the bare name",
                "# would carry the same query with different gold).",
                f"# Saturation: {SATURATION_NOTE}.",
            ]
        else:
            header += [
                "# References gold: same-language files with a refs row naming the",
                "# symbol. Type rows (extends, trait use, type hint, new) count when",
                "# qualified and the qualifier passes the rule below, or bare and the",
                "# file imports the type with a qualifier passing that rule (languages",
                "# whose import rows carry no module path, Python `from x import Y` and",
                "# JS/TS named imports, can only be tail-checked; a bare import row",
                "# counts), or (PHP, Go, C/C++ only) shares the defining directory with",
                "# no same-tail import that failed to resolve; `(same-dir N)` below",
                "# counts kept files whose only evidence is the shared directory. Call rows",
                "# recorded with a qualifier count when it is super/crate/",
                "# Self (`self` for Rust only), the defining file's stem, a directory",
                "# segment of its path or crate-path component, or a symbol defined",
                "# there. Rows recorded bare are classified from the source at line/col:",
                "# (a) receiver calls (x.foo(), x->foo()) count only when the definition's",
                "# owner type is named by the candidate file (imported, extended, trait-",
                "# used, type-hinted, instantiated, or defined there); (b) calls",
                "# qualified in source (A::foo(), A\\foo()) recover the qualifier and use",
                "# the rule above; (c) truly bare calls need an import row naming the",
                "# symbol, or for C/C++/Go the same directory as the defining file or an",
                "# include/import row naming it, and are dropped when the file binds the",
                "# name to a local callable. Files whose only rows are import/include are",
                "# dropped. Glob imports are recorded as the module path (`use a::b::*` ->",
                "# `a::b`) or not at all (`use super::*`), so callers reached only through",
                "# one are conservatively dropped (cs-zz6). Only names with exactly one",
                "# defining file are used. Each case carries `defining_file`; the runner",
                "# drops it from the returned list before ranking, otherwise name queries",
                "# carry a structural first-hit floor of 2 (the defining file ranks first",
                "# and is excluded from gold). Gate counts below are corpus-relevant:",
                "# test-path and excluded candidates are filtered before counting, and",
                "# only symbols that emit a case (1..MAX_REFERENCE_FILES kept files and",
                "# drawn by --sample) are counted.",
                "# Gate counts per language, class kept/dropped files:",
                *[f"#   {line}" for line in (gate_lines or ["(none)"])],
            ]
    body = yaml.safe_dump(
        {
            "project_root": str(project),
            "scoring": {"k_values": [5, 10]},
            "cases": [c.as_dict() for c in cases],
        },
        sort_keys=False,
        allow_unicode=True,
        width=1000,
    )
    return "\n".join(header) + "\n" + body


def write_corpus(out_dir: Path, filename: str, text: str) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / filename
    path.write_text(text, encoding="utf-8")
    return path


# ---------------------------------------------------------------------------
# Entry
# ---------------------------------------------------------------------------

def run_cochange(project: Path, args: argparse.Namespace, conn: sqlite3.Connection) -> Path | None:
    stats = CochangeStats()
    walked = git_log_commits(project, args.commits)
    if walked is None:
        print("[cochange] warn: skipping cochange (project is not a usable git checkout)", file=sys.stderr)
        return None
    commits, stats.scanned = walked
    cases = build_cochange_cases(
        commits,
        symbol_counts_by_path(conn),
        load_exclude_patterns(project),
        min_files=args.min_files,
        max_files=args.max_files,
        include_tests=args.include_tests,
        include_all_types=args.include_all_types,
        stats=stats,
    )
    summary = [
        f"commits in scope: {stats.in_scope} of {stats.scanned} scanned, kept: {stats.kept} "
        f"(changed indexed source files in [{args.min_files}, {args.max_files}]), "
        f"skipped by type: {stats.skipped_type}, skipped short subject: {stats.skipped_short_subject}",
        f"seed by subject: {stats.seed_by_subject}, seed by symbol count: {stats.seed_by_symbols}",
        f"cases written: {len(cases)}",
    ]
    path = write_corpus(
        args.out,
        f"{project_name(project)}-cochange.yaml",
        render_corpus(project, "cochange", cases, summary, provenance=provenance_lines(project, conn)),
    )
    for line in summary[:-1]:
        print(f"[cochange] {line}")
    print(f"[cochange] {summary[-1]} -> {path}")
    return path


def run_known_item(project: Path, args: argparse.Namespace, conn: sqlite3.Connection) -> Path:
    stats = KnownItemStats()
    symbols = eligible_symbols(
        conn,
        project,
        load_exclude_patterns(project),
        gold=args.gold,
        include_tests=args.include_tests,
        stats=stats,
    )
    cases = build_known_item_cases(
        symbols, sample=args.sample, seed=args.seed, stats=stats,
        carry_defining_file=args.gold == "references",
    )
    first = (
        f"symbols eligible: {stats.eligible} (>= {MIN_COMMENT_WORDS}-word doc comment, "
        f">= {MIN_DOC_QUERY_TOKENS}-token query), "
    )
    if args.gold == "references":
        first += (f"multi-definition skipped: {stats.ambiguous + stats.multi_definition}"
                  f", no/too many references skipped: {stats.no_references}")
    else:
        first += (f"ambiguous skipped: {stats.ambiguous} (> {MAX_DEFINITION_FILES} defining files), "
                  f"doc-only (2..{MAX_DEFINITION_FILES} defining files): {stats.doc_only}")
    first += f", sampled: {stats.sampled} (seed {args.seed})"
    summary = [first, f"cases written: {len(cases)}, doc-leaky: {stats.leaky}"]
    suffix = "known-item" if args.gold == "defining" else "known-item-refs"
    path = write_corpus(
        args.out,
        f"{project_name(project)}-{suffix}.yaml",
        render_corpus(
            project, "known-item", cases, summary, gold=args.gold,
            provenance=provenance_lines(project, conn),
            gate_lines=stats.gate.lines() if args.gold == "references" else None,
        ),
    )
    print(f"[known-item] gold: {args.gold}")
    print(f"[known-item] {summary[0]}")
    print(f"[known-item] {summary[1]} -> {path}")
    if args.gold == "references":
        for line in stats.gate.lines():
            print(f"[known-item] gate {line}")
    print(f"[known-item] {CONTAMINATION_NOTE}")
    if args.gold == "defining":
        print(f"[known-item] {SATURATION_NOTE}")
    return path


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--mode", choices=("cochange", "known-item", "both"), required=True)
    ap.add_argument("--project", type=Path, required=True)
    ap.add_argument("--out", type=Path, default=Path(__file__).resolve().parent / "corpora" / "self")
    ap.add_argument("--commits", type=int, default=80)
    ap.add_argument("--min-files", type=int, default=2)
    ap.add_argument(
        "--max-files", type=int, default=DEFAULT_MAX_FILES,
        help="max changed indexed source files per commit, seed included; gold is one fewer "
             "so recall@10 has no structural ceiling",
    )
    ap.add_argument(
        "--include-all-types", action="store_true",
        help="keep style/chore/ci/build/deps/release/docs commits (the 3-content-word floor still applies)",
    )
    ap.add_argument("--sample", type=int, default=150)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument(
        "--gold", choices=("defining", "references"), default=None,
        help="known-item gold (default defining): the defining file, or the 1..10 same-language "
             "files referencing the symbol (test paths excluded unless --include-tests)",
    )
    ap.add_argument(
        "--include-tests", action="store_true",
        help="keep FileCategory-style test paths as seeds, gold, and known-item sources",
    )
    args = ap.parse_args(argv)

    project = args.project.expanduser().resolve()
    if not project.is_dir():
        ap.error(f"project root does not exist: {project}")
    if args.min_files < 2:
        ap.error("--min-files must be >= 2 (a seed plus at least one gold file)")
    if args.max_files < args.min_files:
        ap.error("--max-files must be >= --min-files")
    if args.sample < 1:
        ap.error("--sample must be >= 1")
    if args.gold is not None and args.mode == "cochange":
        print("[self-eval] warn: --gold only affects known-item; ignored for --mode cochange", file=sys.stderr)
    args.gold = args.gold or "defining"

    conn = open_index(project)
    try:
        cochange_path: Path | None = None
        if args.mode in ("cochange", "both"):
            cochange_path = run_cochange(project, args, conn)
            if cochange_path is None and args.mode == "cochange":
                return 1
        if args.mode in ("known-item", "both"):
            run_known_item(project, args, conn)
    finally:
        conn.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
