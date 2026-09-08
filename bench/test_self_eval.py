#!/usr/bin/env python3
"""Regression tests for bench/self-eval.py.

Usage:
  python3 bench/test_self_eval.py

Bare-assert style — no pytest dependency. Exits 0 on success, 1 on failure.
Builds a throwaway git repo with real source files and a minimal
`.codesage/index.db` using the `files` / `symbols` / `refs` columns from
crates/storage/src/schema.rs.
"""

from __future__ import annotations

import contextlib
import hashlib
import importlib.machinery
import importlib.util
import io
import os
import random
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

try:
    import yaml
except ImportError:
    sys.exit("pyyaml required: pip install pyyaml")

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "self-eval.py"
failures: list[str] = []


def _load(filename: str, modname: str):
    spec = importlib.util.spec_from_file_location(modname, HERE / filename)
    mod = importlib.util.module_from_spec(spec)
    # dataclasses resolve postponed annotations through sys.modules.
    sys.modules[modname] = mod
    spec.loader.exec_module(mod)
    return mod


def check(cond: bool, label: str) -> None:
    if not cond:
        failures.append(f"  {label}")


se = _load("self-eval.py", "self_eval")

SCHEMA = """
CREATE TABLE files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    language TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    indexed_at INTEGER NOT NULL DEFAULT 0,
    boundaries_derived_at INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE symbols (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    col_start INTEGER NOT NULL,
    col_end INTEGER NOT NULL,
    rationale TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE refs (
    id INTEGER PRIMARY KEY,
    from_file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    from_symbol TEXT,
    to_name TEXT NOT NULL,
    to_name_tail TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL
);
"""

A_MOD_RS = """\
/// Connection wrapper struct owning the socket handle for the daemon.
pub struct A;
/// Open the daemon socket and perform the version handshake first.
pub fn connect() {}
/// Dual helper defined in the module file with its own doc text.
pub fn dual() {}
/// Draw the frame buffer to the terminal without flicker artifacts.
pub fn render() {}
/// Look up one row by its primary key in the table.
pub fn lookup(&self) {}
/// Helper exercised by exactly ten callers to pin the cap.
pub fn ten_caller() {}
"""

RENDER_THREE_RS = """\
/// Paint the summary table for the third caller in plain text.
pub fn render() {}
"""

ALPHA_RS = """\
/// Cached for the lifetime of the instance so lookups stay cheap.
/// Second sentence is ignored.
#[inline]
pub fn alpha_one() {}

pub fn alpha_two() {}
pub fn alpha_three() {}
pub fn shared_name() {}

/// Reset the cache counter when the cache counter overflows its budget.
pub fn cache_counter() {}

/// Dual helper defined in alpha with a different doc text entirely.
pub fn dual() {}

/// Emit the alpha report as markdown rows for the reviewer to read.
pub fn render() {}

#[cfg(test)]
mod tests {
    /// Documented helper that lives inside the test module only.
    fn alpha_test_helper() {}
}

#[cfg(test)]
// Second test module guarded by a comment line between attribute and mod.
mod more_tests {
    /// Another documented helper that lives inside a test module.
    fn alpha_test_helper_two() {}
}

#[cfg(all(test, unix))] mod unix_tests {
    /// Documented helper inside a one-line attributed test module here.
    fn alpha_test_helper_three() {}
}
"""

BETA_C = """\
/**
 * First sentence has enough words in it. Second one is ignored.
 * @param x unused
 */
int beta_one(int x) { return x; }
/** Shared helper defined in many files so it is ambiguous gold. */
static int shared_name(void) { return 0; }
#define BETA_MAX 10
#define BETA_MIN 1
int beta_two(void) { return BETA_MAX; }
int beta_three(void) { return 0; } /* trailing comment that is easily long enough to count */
int beta_four(void) { return 1; }
/* old:
int x = compute(a, b);
*/
int beta_five(void) { return 2; }
int beta_one_use = 0;
"""

EPSILON_C = """\
/*
 * Copyright 2024 Example Corp. Licensed under the MIT license terms.
 */
int epsilon_one(void) { return 1; }
int use_beta(void) { return beta_one(1); }
"""

ZETA_C = "int z(void) { return beta_one(2); }\n"
ETA_C = '#include "../beta.h"\nint e(void) { return beta_one(3); }\n'

GAMMA_PY = """\
def gamma_one():
    \"\"\"Short doc.\"\"\"
    return 1


@staticmethod
def gamma_two(
    a,
):
    \"\"\"Docstring spanning the multi-line signature for coverage.

    More detail.
    \"\"\"
    return a


# Helper used by the registry when a plugin registers late.
@classmethod
def gamma_three(cls):
    return cls


def shared_name():
    return 0


# Comment above that should lose to the docstring below it.
def gamma_four():
    \"\"\"Docstring that wins over the comment block above the def.\"\"\"
    return 4


def gamma_five(a, b)
def gamma_six():
    \"\"\"Docstring owned by the sixth helper and never by the fifth one above.\"\"\"
    return 6


def gamma_seven(
    a,
    b
)
value = 3
\"\"\"Stray string that must not be claimed by gamma_seven at all.\"\"\"
"""

DELTA_PHP = """\
<?php
/**
 * Normalises the incoming payload before validation runs.
 * @param array $payload raw request body
 * @return array
 */
function delta_one(array $payload): array { return $payload; }
function shared_name() {}
function delta_only() {}
/** This is the one for it and the other. */
function delta_two() {}
# Hash comments are legal PHP and this one is long enough.
function delta_three() {}
"""

DOCUMENTED_RS = """\
/// Documented but excluded because of where this file lives.
pub fn {name}() {{}}
"""

BASE_PHP = """\
<?php
namespace Ns;
/** Shared parent for formatters that need the default wire helpers. */
class Base {
    /** Format one record into the wire representation for output. */
    public function fmt() {}
}
"""

JAR_PY = """\
class Jar:
    def values(self):
        \"\"\"Return the cookie values in insertion order for iteration.\"\"\"
        return []

    def clear_expired(self, now):
        \"\"\"Drop cookies whose expiry passed before the given timestamp.\"\"\"
        return now
"""
SESS_PY = """\
from jar import Jar


class Session:
    def close(self):
        for v in self.adapters.values():
            v.close()

    def tidy(self, jar: Jar):
        return jar.clear_expired(0)
"""
FORMATTER_PHP = """\
<?php
namespace Ns;
class Formatter extends Base {
    public function run() { return $this->fmt(); }
}
"""
OTHER_PHP = """\
<?php
namespace Ns;
class Other {
    public function run($x) { return $x->fmt(); }
}
"""
MESSAGE_PHP = """\
<?php
namespace Ns;
/** Log record envelope carried between handlers and formatters. */
class Message {}
"""
GELF_HANDLER_PHP = """\
<?php
namespace Ns;
class GelfHandler {
    public function send(\\Gelf\\Message $m) {}
}
"""
WIRE_PHP = """\
<?php
namespace Ns;
class Wire extends Message {}
"""
NOTE_PHP = """\
<?php
namespace Far;
class Note extends Message {}
"""
IMPORTED_PHP = """\
<?php
namespace Far;
use Ns\\Message;
class Imported extends Message {}
"""
FOREIGN_PHP = """\
<?php
namespace Far;
use Other\\Message;
class Foreign extends Message {}
"""
USES_A_RS = "fn f(a: A) {}\n"
SHADOW_PHP = """\
<?php
namespace Ns;
use Gelf\\Message;
class Shadow extends Message {}
"""
LOOKUP_TEST_RS = "use crate::a_mod::A;\nfn t(a: A) { a.lookup(); }\n"

FILE_24_RS = "pub fn bulk_24() {\n    let alpha_one = |x| x;\n    alpha_one(1);\n}\n"
FILE_35_RS = "use crate::a_mod::A;\nfn f(a: A) { a.lookup(); }\n"
FILE_36_RS = "use other::B;\nfn g(b: B) { b.lookup(); }\n"
FILE_37_RS = "fn h() { a_mod::connect(); }\n"


def _line(src: str, needle: str) -> int:
    for i, line in enumerate(src.split("\n"), start=1):
        if needle in line:
            return i
    raise AssertionError(f"fixture needle not found: {needle!r}")


def _pos(src: str, needle: str, token: str) -> tuple[int, int]:
    """(1-based line, 0-based col) of `token` on the first line containing `needle`."""
    line = _line(src, needle)
    return line, src.split("\n")[line - 1].index(token)


# path -> (language, source, [(name, kind, needle) | (name, kind, needle, qualified_name)])
INDEX: dict[str, tuple[str, str, list[tuple]]] = {
    "src/a_mod.rs": ("rust", A_MOD_RS, [
        ("A", "struct", "pub struct A"),
        ("connect", "function", "fn connect"),
        ("dual", "function", "fn dual"),
        ("render", "function", "fn render"),
        ("lookup", "method", "fn lookup", "A<'a>::lookup"),
        ("ten_caller", "function", "fn ten_caller"),
    ]),
    "src/render_three.rs": ("rust", RENDER_THREE_RS, [
        ("render", "function", "fn render"),
    ]),
    "src/alpha.rs": ("rust", ALPHA_RS, [
        ("alpha_one", "function", "fn alpha_one"),
        ("alpha_two", "function", "fn alpha_two"),
        ("alpha_three", "function", "fn alpha_three"),
        ("shared_name", "function", "fn shared_name"),
        ("cache_counter", "function", "fn cache_counter"),
        ("dual", "function", "fn dual"),
        ("render", "function", "fn render"),
        ("alpha_test_helper", "function", "fn alpha_test_helper()"),
        ("alpha_test_helper_two", "function", "fn alpha_test_helper_two"),
        ("alpha_test_helper_three", "function", "fn alpha_test_helper_three"),
    ]),
    "src/beta.c": ("c", BETA_C, [
        ("beta_one", "function", "int beta_one(int"),
        ("shared_name", "function", "static int shared_name"),
        ("beta_two", "function", "int beta_two"),
        ("beta_three", "function", "int beta_three"),
        ("beta_four", "function", "int beta_four"),
        ("beta_five", "function", "int beta_five"),
        # Usage row recorded as a symbol: must never replace the definition.
        ("beta_one", "function", "int beta_one_use"),
    ]),
    "src/epsilon.c": ("c", EPSILON_C, [
        ("epsilon_one", "function", "int epsilon_one"),
        ("use_beta", "function", "int use_beta"),
    ]),
    "src/sub/zeta_c.c": ("c", ZETA_C, [("z", "function", "int z(")]),
    "src/sub/eta.c": ("c", ETA_C, [("e", "function", "int e(")]),
    "src/gamma.py": ("python", GAMMA_PY, [
        ("gamma_one", "function", "def gamma_one"),
        ("gamma_two", "function", "def gamma_two"),
        ("gamma_three", "function", "def gamma_three"),
        ("shared_name", "function", "def shared_name"),
        ("gamma_four", "function", "def gamma_four"),
        ("gamma_five", "function", "def gamma_five"),
        ("gamma_six", "function", "def gamma_six"),
        ("gamma_seven", "function", "def gamma_seven"),
    ]),
    "src/zeta.py": ("python", "import gamma\n", []),
    "src/jar.py": ("python", JAR_PY, [
        ("Jar", "class", "class Jar"),
        ("values", "method", "def values", "Jar.values"),
        ("clear_expired", "method", "def clear_expired", "Jar.clear_expired"),
    ]),
    "src/sess.py": ("python", SESS_PY, [
        ("Session", "class", "class Session"),
        ("close", "method", "def close", "Session.close"),
        ("tidy", "method", "def tidy", "Session.tidy"),
    ]),
    "src/Ns/Base.php": ("php", BASE_PHP, [
        ("Base", "class", "class Base"),
        ("fmt", "method", "function fmt", "Ns\\Base\\fmt"),
    ]),
    "src/Ns/Formatter.php": ("php", FORMATTER_PHP, [
        ("Formatter", "class", "class Formatter"),
        ("run", "method", "function run", "Ns\\Formatter\\run"),
    ]),
    "src/Ns/Other.php": ("php", OTHER_PHP, [
        ("Other", "class", "class Other"),
        ("run", "method", "function run", "Ns\\Other\\run"),
    ]),
    # Message: type rows only. Qualified `\Gelf\Message` (foreign package, dropped);
    # bare `extends Message` in the defining directory (kept), in another directory
    # without an import (dropped), with a `use Ns\Message` import (kept), and with
    # a same-tail `use Other\Message` import from a foreign namespace (dropped).
    "src/Ns/Message.php": ("php", MESSAGE_PHP, [("Message", "class", "class Message")]),
    "src/Ns/GelfHandler.php": ("php", GELF_HANDLER_PHP, []),
    "src/Ns/Wire.php": ("php", WIRE_PHP, []),
    "src/far/Note.php": ("php", NOTE_PHP, []),
    "src/far/Imported.php": ("php", IMPORTED_PHP, []),
    "src/far/Foreign.php": ("php", FOREIGN_PHP, []),
    # Same directory as Message.php, but `use Gelf\Message` shadows the local class.
    "src/Ns/Shadow.php": ("php", SHADOW_PHP, []),
    # A: bare Rust type hint in the defining directory without an import. Rust
    # resolves types by `use`, not by directory, so the row is dropped.
    "src/uses_a.rs": ("rust", USES_A_RS, [("f", "function", "fn f(")]),
    "src/delta.php": ("php", DELTA_PHP, [
        ("delta_one", "function", "function delta_one"),
        ("shared_name", "function", "function shared_name"),
        ("delta_only", "function", "function delta_only"),
        ("delta_two", "function", "function delta_two"),
        ("delta_three", "function", "function delta_three"),
    ]),
    "src/vendored/keep_out.rs": ("rust", DOCUMENTED_RS.format(name="vendored_fn"), [
        ("vendored_fn", "function", "fn vendored_fn"),
    ]),
    "tests/alpha_test.rs": ("rust", DOCUMENTED_RS.format(name="test_alpha"), [
        ("test_alpha", "function", "fn test_alpha"),
    ]),
    # Same-language test path with an accepted receiver call: must never reach the gate counts.
    "tests/lookup_test.rs": ("rust", LOOKUP_TEST_RS, []),
    "docs/guide.md": ("markdown", "# Guide\n", []),
    "docs/other.md": ("markdown", "# Other\n", []),
}
BULK_COUNT = 40
for i in range(BULK_COUNT):
    INDEX[f"src/bulk/file_{i:02d}.rs"] = ("rust", f"pub fn bulk_{i}() {{}}\n", [(f"bulk_{i}", "function", "pub fn")])
# Closure shadow: a local `alpha_one` binding the extractor never indexes.
INDEX["src/bulk/file_24.rs"] = ("rust", FILE_24_RS, [("bulk_24", "function", "pub fn")])
# Receiver call on an imported owner type (rule a, accepted).
INDEX["src/bulk/file_35.rs"] = ("rust", FILE_35_RS, [("f", "function", "fn f(")])
# Receiver call on a foreign owner type (rule a, dropped).
INDEX["src/bulk/file_36.rs"] = ("rust", FILE_36_RS, [("g", "function", "fn g(")])
# `::`-qualified in source but recorded bare (rule b).
INDEX["src/bulk/file_37.rs"] = ("rust", FILE_37_RS, [("h", "function", "fn h(")])

# (from_path, to_name, to_name_tail, kind[, line, col])
REFS: list[tuple] = [
    # alpha_one: import + bare call (gold); bare call without import (dropped);
    # import only (dropped); import + bare call but a local closure shadow (dropped);
    # bare call without any import (dropped).
    ("src/bulk/file_03.rs", "alpha_one", "alpha_one", "import"),
    ("src/bulk/file_03.rs", "alpha_one", "alpha_one", "call"),
    ("src/bulk/file_04.rs", "alpha_one", "alpha_one", "call"),
    ("src/bulk/file_05.rs", "alpha_one", "alpha_one", "import"),
    ("src/bulk/file_24.rs", "alpha_one", "alpha_one", "import"),
    ("src/bulk/file_24.rs", "alpha_one", "alpha_one", "call", *_pos(FILE_24_RS, "alpha_one(1)", "alpha_one")),
    ("src/bulk/file_19.rs", "alpha_one", "alpha_one", "call"),
    # beta_one (C): same-dir bare call accepted; other-dir without include dropped;
    # other-dir with an include naming the defining file accepted; test file excluded.
    ("src/epsilon.c", "beta_one", "beta_one", "call", *_pos(EPSILON_C, "use_beta", "beta_one")),
    ("src/sub/zeta_c.c", "beta_one", "beta_one", "call", *_pos(ZETA_C, "int z", "beta_one")),
    ("src/sub/eta.c", "../beta.h", "beta.h", "include", 1, 0),
    ("src/sub/eta.c", "beta_one", "beta_one", "call", *_pos(ETA_C, "int e", "beta_one")),
    ("tests/alpha_test.rs", "beta_one", "beta_one", "call"),
    # gamma_two: tail match qualified by the defining stem, same language.
    ("src/zeta.py", "gamma.gamma_two", "gamma_two", "call"),
    # connect: A:: qualifier is a symbol in the defining file; B:: is a homonym;
    # super:: accepted; UnixStream:: rejected; `a_mod::connect()` recorded bare (rule b).
    ("src/alpha.rs", "A::connect", "connect", "call"),
    ("src/bulk/file_02.rs", "B::connect", "connect", "call"),
    ("src/bulk/file_21.rs", "super::connect", "connect", "call"),
    ("src/bulk/file_22.rs", "UnixStream::connect", "connect", "call"),
    ("src/bulk/file_37.rs", "connect", "connect", "call", *_pos(FILE_37_RS, "a_mod::connect", "connect")),
    # lookup (method A::lookup): receiver call with owner imported (kept) / foreign owner (dropped).
    ("src/bulk/file_35.rs", "crate::a_mod::A", "A", "import"),
    ("src/bulk/file_35.rs", "lookup", "lookup", "call", *_pos(FILE_35_RS, "a.lookup", "lookup")),
    ("src/bulk/file_36.rs", "other::B", "B", "import"),
    ("src/bulk/file_36.rs", "lookup", "lookup", "call", *_pos(FILE_36_RS, "b.lookup", "lookup")),
    ("tests/lookup_test.rs", "crate::a_mod::A", "A", "import"),
    ("tests/lookup_test.rs", "lookup", "lookup", "call", *_pos(LOOKUP_TEST_RS, "a.lookup", "lookup")),
    # fmt (method Ns\Base\fmt): `$this->fmt()` in a subclass that `extends Base` (kept);
    # `$x->fmt()` in a file that never names Base (dropped).
    ("src/Ns/Formatter.php", "Base", "Base", "inheritance"),
    ("src/Ns/Formatter.php", "fmt", "fmt", "call", *_pos(FORMATTER_PHP, "$this->fmt", "fmt")),
    ("src/Ns/Other.php", "fmt", "fmt", "call", *_pos(OTHER_PHP, "$x->fmt", "fmt")),
    # Message (class Ns\Message): type rows, see the INDEX comment.
    ("src/Ns/GelfHandler.php", "\\Gelf\\Message", "Message", "type_hint"),
    ("src/Ns/Wire.php", "Message", "Message", "inheritance"),
    ("src/far/Note.php", "Message", "Message", "inheritance"),
    ("src/far/Imported.php", "Ns\\Message", "Message", "import"),
    ("src/far/Imported.php", "Message", "Message", "inheritance"),
    ("src/far/Foreign.php", "Other\\Message", "Message", "import"),
    ("src/far/Foreign.php", "Message", "Message", "inheritance"),
    ("src/uses_a.rs", "A", "A", "type_hint"),
    ("src/Ns/Shadow.php", "Gelf\\Message", "Message", "import"),
    ("src/Ns/Shadow.php", "Message", "Message", "inheritance"),
    # Jar.values: `self.adapters.values()` is a dict call; the caller imports Jar for
    # unrelated reasons, so the builtin-name stoplist must drop it. Jar.clear_expired
    # on the same imported owner is kept.
    ("src/sess.py", "jar.Jar", "Jar", "import"),
    ("src/sess.py", "values", "values", "call", *_pos(SESS_PY, "self.adapters.values", "values")),
    ("src/sess.py", "clear_expired", "clear_expired", "call", *_pos(SESS_PY, "jar.clear_expired", "clear_expired")),
    # render: 3 defining files, exact-name caller -> no refs-mode case at all.
    ("src/bulk/file_20.rs", "render", "render", "call"),
    # delta_one: referenced only from rust files (language mismatch).
    *[(f"src/bulk/file_{i:02d}.rs", "delta_one", "delta_one", "call") for i in range(3)],
    # cache_counter: 11 referencing files (import + call each), one over the cap.
    *[(f"src/bulk/file_{i:02d}.rs", "cache_counter", "cache_counter", k)
      for i in range(6, 17) for k in ("import", "call")],
    # ten_caller: exactly 10 referencing files, at the cap.
    *[(f"src/bulk/file_{i:02d}.rs", "ten_caller", "ten_caller", k)
      for i in range(25, 35) for k in ("import", "call")],
]

# Eligible known-item symbols in (path, line_start) order with --gold defining.
ELIGIBLE_ORDER: list[tuple[str, str]] = [
    ("src/Ns/Base.php", "Base"), ("src/Ns/Base.php", "fmt"),
    ("src/Ns/Message.php", "Message"),
    ("src/a_mod.rs", "A"), ("src/a_mod.rs", "connect"), ("src/a_mod.rs", "dual"), ("src/a_mod.rs", "render"),
    ("src/a_mod.rs", "lookup"), ("src/a_mod.rs", "ten_caller"),
    ("src/alpha.rs", "alpha_one"), ("src/alpha.rs", "cache_counter"), ("src/alpha.rs", "dual"),
    ("src/alpha.rs", "render"),
    ("src/beta.c", "beta_one"),
    ("src/delta.php", "delta_one"), ("src/delta.php", "delta_three"),
    ("src/gamma.py", "gamma_two"), ("src/gamma.py", "gamma_three"),
    ("src/gamma.py", "gamma_four"), ("src/gamma.py", "gamma_six"),
    ("src/jar.py", "values"), ("src/jar.py", "clear_expired"),
    ("src/render_three.rs", "render"),
]
MULTI_DEFINITION = {"dual", "render"}
NAME_CASE_SYMBOLS = {name for _p, name in ELIGIBLE_ORDER if name not in MULTI_DEFINITION}
EXPECTED_REF_GOLD = {
    "alpha_one": ["src/bulk/file_03.rs"],
    "beta_one": ["src/epsilon.c", "src/sub/eta.c"],
    "gamma_two": ["src/zeta.py"],
    "connect": ["src/alpha.rs", "src/bulk/file_21.rs", "src/bulk/file_37.rs"],
    "lookup": ["src/bulk/file_35.rs"],
    "ten_caller": [f"src/bulk/file_{i:02d}.rs" for i in range(25, 35)],
    "fmt": ["src/Ns/Formatter.php"],
    "Base": ["src/Ns/Formatter.php"],
    "Message": ["src/Ns/Wire.php", "src/far/Imported.php"],
    "clear_expired": ["src/sess.py"],
}


def kid(path: str, name: str) -> str:
    return "ki-" + hashlib.sha1(f"{path}\0{name}".encode()).hexdigest()[:8]


def build_index(db: Path) -> None:
    conn = sqlite3.connect(db)
    conn.executescript(SCHEMA)
    ids: dict[str, int] = {}
    for fid, (path, (language, src, syms)) in enumerate(INDEX.items(), start=1):
        ids[path] = fid
        conn.execute(
            "INSERT INTO files (id, path, language, content_hash) VALUES (?, ?, ?, 'h')",
            (fid, path, language),
        )
        for entry in syms:
            name, kind, needle = entry[:3]
            qualified = entry[3] if len(entry) > 3 else name
            line = _line(src, needle)
            conn.execute(
                "INSERT INTO symbols (file_id, name, qualified_name, kind, line_start, line_end, "
                "col_start, col_end) VALUES (?, ?, ?, ?, ?, ?, 0, 0)",
                (fid, name, qualified, kind, line, line),
            )
    for entry in REFS:
        from_path, to_name, tail, kind = entry[:4]
        line, col = entry[4:6] if len(entry) > 4 else (1, 0)
        conn.execute(
            "INSERT INTO refs (from_file_id, from_symbol, to_name, to_name_tail, kind, line, col) "
            "VALUES (?, NULL, ?, ?, ?, ?, ?)",
            (ids[from_path], to_name, tail, kind, line, col),
        )
    conn.commit()
    conn.close()


def git(repo: Path, *args: str) -> None:
    env = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
    subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", *args],
        cwd=repo, check=True, capture_output=True, env=env,
    )


def commit_files(git_root: Path, base: Path, subject: str, paths: list[str]) -> None:
    # Append-only edits keep every definition's line_start stable.
    for p in paths:
        f = base / p
        f.parent.mkdir(parents=True, exist_ok=True)
        with f.open("a", encoding="utf-8") as fh:
            fh.write(f"// {subject}\n")
    git(git_root, "add", "-A")
    git(git_root, "commit", "-q", "-m", subject)


COMMITS: list[tuple[str, list[str]]] = [
    # Scaffold: 57 indexed source candidates, dropped at the default --max-files 10.
    ("touch everything in the bulk tree at once", [f"src/bulk/file_{i:02d}.rs" for i in range(BULK_COUNT)]),
    ("solo: touch gamma", ["src/gamma.py"]),
    ("pair: alpha and beta together",
     ["src/alpha.rs", "src/beta.c", "tests/alpha_test.rs", "src/vendored/keep_out.rs", "README.md"]),
    ("wire two modules end to end", ["src/alpha.rs", "src/beta.c"]),
    ("chore: bump alpha and beta versions", ["src/alpha.rs", "src/beta.c"]),
    ("fix: it", ["src/alpha.rs", "src/beta.c"]),
    ("refactor: tidy the bulk loader path", ["src/bulk/file_00.rs", "src/delta.php"]),
    ("docs: rewrite the beta notes", ["docs/guide.md", "docs/other.md", "src/beta.c"]),
    ("docs: explain delta and beta interplay", ["src/delta.php", "src/beta.c"]),
    ("fix(bulk): make the loader robust again", ["src/bulk/file_01.rs", "src/delta.php"]),
]
SCAFFOLD_CANDIDATES = 65

EXPECTED_SUBJECTS = {
    "pair: alpha and beta together": ["src/beta.c"],
    # No stem in the subject: alpha.rs has 10 symbol rows, beta.c 7.
    "wire two modules end to end": ["src/beta.c"],
    "refactor: tidy the bulk loader path": ["src/delta.php"],
    "docs: explain delta and beta interplay": ["src/beta.c"],
    "fix(bulk): make the loader robust again": ["src/delta.php"],
}


def build_repo(git_root: Path, project: Path, *, outside_commit: bool = False) -> None:
    git_root.mkdir(parents=True, exist_ok=True)
    project.mkdir(parents=True, exist_ok=True)
    git(git_root, "init", "-q")
    (project / ".codesage").mkdir()
    (project / ".codesage" / "config.toml").write_text(
        '[project]\nname = "fixture"\n\n[index]\nexclude_patterns = ["**/vendored/**"]\n',
        encoding="utf-8",
    )
    for path, (_language, src, _syms) in INDEX.items():
        f = project / path
        f.parent.mkdir(parents=True, exist_ok=True)
        f.write_text(src, encoding="utf-8")
    build_index(project / ".codesage" / "index.db")
    for subject, paths in COMMITS:
        commit_files(git_root, project, subject, paths)
    if outside_commit:
        commit_files(git_root, git_root, "outside: change the top-level readme only", ["top.rs"])


def cases_by_query(path: Path) -> dict[str, dict]:
    doc = yaml.safe_load(path.read_text(encoding="utf-8"))
    return {c["query"]: c for c in doc["cases"]}


def run_main(argv: list[str]) -> tuple[int, str]:
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        rc = se.main(argv)
    return rc, buf.getvalue()


def header_of(path: Path) -> str:
    return path.read_text(encoding="utf-8").split("project_root:")[0]


def check_provenance(header: str, label: str) -> None:
    for key in ("generated: ", "project_head: ", "codesage: ", "script_revision: ", "index_digest: "):
        check(f"# {key}" in header, f"{label}: header carries {key.strip()}")
    check("(files=" in header and "symbols=" in header and "refs=" in header, f"{label}: index digest lists row counts")


def unit_checks() -> None:
    for p in ("tests/foo.rs", "src/__tests__/x.ts", "app/FooTest.php", "web/a.test.ts",
              "pkg/test_util.py", "pkg/x_test.go", "ext/a.phpt", "./test/x.c"):
        check(se.is_test_path(p), f"is_test_path accepts {p}")
    for p in ("src/latest.php", "src/Manifests.java", "src/contest/x.rs", "src/testing.rs"):
        check(not se.is_test_path(p), f"is_test_path rejects {p}")
    rx = se.glob_to_regex("**/vendored/**")
    check(bool(rx.match("src/vendored/keep_out.rs")), "glob **/vendored/** matches nested")
    check(not rx.match("src/vendor/x.rs"), "glob **/vendored/** rejects sibling dir")
    check(bool(se.glob_to_regex("**/*Test.php").match("app/A/BTest.php")), "glob *Test.php")
    check(not se.glob_to_regex("*.rs").match("src/a.rs"), "single * does not cross /")
    alt = se.glob_to_regex("**/*.{js,ts}")
    check(bool(alt.match("a/b.ts")) and bool(alt.match("b.js")) and not alt.match("a/b.rs"), "glob {js,ts} alternation")
    alt2 = se.glob_to_regex("**/{vendor,node_modules}/**")
    check(bool(alt2.match("x/node_modules/y/z.js")) and bool(alt2.match("vendor/a.php"))
          and not alt2.match("x/vendors/a.php"), "glob {vendor,node_modules} alternation")
    nested = se.glob_to_regex("src/{a,b/{c,d}}.rs")
    check(bool(nested.match("src/b/d.rs")) and bool(nested.match("src/a.rs")) and not nested.match("src/b.rs"),
          "glob nested alternation")
    cls = se.glob_to_regex("**/[Tt]est*/**")
    check(bool(cls.match("x/Tests/a.rs")) and bool(cls.match("test1/b.rs")) and not cls.match("x/rest/a.rs"),
          "glob [Tt] character class")
    neg = se.glob_to_regex("[!a]*.rs")
    check(bool(neg.match("b.rs")) and not neg.match("a.rs"), "glob [!a] negated class")

    alpha = se.split_source_lines(ALPHA_RS)
    check(se.source_comment(alpha, _line(ALPHA_RS, "fn alpha_one"), "rust")
          == "Cached for the lifetime of the instance so lookups stay cheap. Second sentence is ignored.",
          "source_comment: rust /// through #[inline]")
    check(se.source_comment(alpha, _line(ALPHA_RS, "fn alpha_two"), "rust") is None, "source_comment: no comment -> None")
    want_ranges = [
        (_line(ALPHA_RS, "#[cfg(test)]"), _line(ALPHA_RS, "fn alpha_test_helper()") + 1),
        (_line(ALPHA_RS, "// Second test module") - 1, _line(ALPHA_RS, "fn alpha_test_helper_two") + 1),
        (_line(ALPHA_RS, "#[cfg(all(test, unix))]"), _line(ALPHA_RS, "fn alpha_test_helper_three") + 1),
    ]
    check(se.rust_test_module_ranges(alpha) == want_ranges,
          f"rust_test_module_ranges: three modules incl. one-line cfg(all(test, unix)) (got {se.rust_test_module_ranges(alpha)})")
    check(se.rust_test_module_ranges(["#[cfg(feature = \"x\")]", "mod tests {", "}"]) == [],
          "rust_test_module_ranges: cfg without test predicate ignored")
    check(se.rust_test_module_ranges(["#[cfg(not(test))]", "mod tests {", "}"]) == [],
          "rust_test_module_ranges: cfg(not(test)) is not a test module")
    check(se.rust_test_module_ranges(["#[cfg(feature = \"test-utils\")]", "mod tests {", "}"]) == [],
          "rust_test_module_ranges: feature = \"test-utils\" is not a test predicate")
    check(se.rust_test_module_ranges(["#[cfg(any(test, feature = \"x\"))]", "mod tests {", "}"]) == [(1, 3)],
          "rust_test_module_ranges: any(test, ...) still detected")
    beta = se.split_source_lines(BETA_C)
    check(se.source_comment(beta, _line(BETA_C, "int beta_one(int"), "c")
          == "First sentence has enough words in it. Second one is ignored.", "source_comment: C /** */ block with @param dropped")
    check(se.source_comment(beta, _line(BETA_C, "int beta_two"), "c") is None, "source_comment: #define run is not a C comment")
    check(se.source_comment(beta, _line(BETA_C, "int beta_four"), "c") is None, "source_comment: trailing */ on a code line rejected")
    check(se.source_comment(beta, _line(BETA_C, "int beta_five"), "c") is None, "source_comment: commented-out code rejected")
    check(se.source_comment(se.split_source_lines(EPSILON_C), _line(EPSILON_C, "int epsilon_one"), "c") is None,
          "source_comment: license header rejected")
    check(se.source_comment(["--i; // decrement the counter before the call below", "int f(void)"], 2, "c") is None,
          "source_comment: C `--i;` is not a comment")
    check(se.source_comment(["-- Returns the widget count for the given owner id.", "function count(owner)"], 2, "lua") is None,
          "source_comment: -- comments off unless the language is in DASH_COMMENT_LANGUAGES")
    gamma = se.split_source_lines(GAMMA_PY)
    check(se.source_comment(gamma, _line(GAMMA_PY, "def gamma_one"), "python") is None, "source_comment: short docstring rejected")
    check(se.source_comment(gamma, _line(GAMMA_PY, "def gamma_two"), "python")
          == "Docstring spanning the multi-line signature for coverage. More detail.",
          "source_comment: docstring after multi-line signature")
    check(se.source_comment(gamma, _line(GAMMA_PY, "def gamma_three"), "python")
          == "Helper used by the registry when a plugin registers late.", "source_comment: # comment through @classmethod")
    check(se.source_comment(gamma, _line(GAMMA_PY, "def gamma_four"), "python")
          == "Docstring that wins over the comment block above the def.",
          "source_comment: python docstring preferred over comment block")
    check(se.source_comment(gamma, _line(GAMMA_PY, "def gamma_five"), "python") is None,
          "source_comment: signature scan aborts at the next def")
    check(se.source_comment(gamma, _line(GAMMA_PY, "def gamma_seven"), "python") is None,
          "source_comment: signature scan aborts at a dedent")
    delta = se.split_source_lines(DELTA_PHP)
    check(se.source_comment(delta, _line(DELTA_PHP, "function delta_one"), "php")
          == "Normalises the incoming payload before validation runs.", "source_comment: PHP docblock with @param/@return stripped")
    check(se.source_comment(delta, _line(DELTA_PHP, "function delta_only"), "php") is None,
          "source_comment: PHP function without docblock")
    check(se.source_comment(delta, _line(DELTA_PHP, "function delta_three"), "php")
          == "Hash comments are legal PHP and this one is long enough.", "source_comment: # comment accepted for PHP")
    check(se.split_source_lines("a\r\nb\n") == ["a\r", "b", ""], "split_source_lines splits on \\n only")
    check(se.is_leaky_query("cache_counter", "Reset cache counter cache counter overflows budget"),
          "is_leaky_query: every name token present as a word")
    check(not se.is_leaky_query("FakeDaemon", "stand-in daemon answers embed_texts"), "is_leaky_query: missing token")
    check(not se.is_leaky_query("dim", "Dimension of the vectors"), "is_leaky_query: dim vs Dimension not leaky")
    check(se._qualifier_of("A::connect", "connect") == "A" and se._qualifier_of("gamma.gamma_two", "gamma_two") == "gamma"
          and se._qualifier_of("connect", "connect") is None, "_qualifier_of: last segment before the tail")
    for q, path, want in (
        ("super", "src/a_mod.rs", True), ("self", "src/a_mod.rs", True), ("crate", "src/a_mod.rs", True),
        ("Self", "src/a_mod.rs", True),
        ("codesage_graph", "crates/graph/src/trace.rs", True), ("graph", "crates/graph/src/trace.rs", True),
        ("trace", "crates/graph/src/trace.rs", True),
        ("UnixStream", "crates/cli/src/daemon.rs", False), ("File", "crates/cli/src/daemon.rs", False),
        ("fs", "crates/cli/src/daemon.rs", False), ("process", "crates/cli/src/daemon.rs", False),
        ("Connection", "crates/storage/src/db/mod.rs", False), (None, "src/a_mod.rs", False),
    ):
        check(se.qualifier_accepted(q, path, set(), "rust") is want, f"qualifier_accepted({q!r}, {path}) is {want}")
    check(se.qualifier_accepted("A", "src/a_mod.rs", {"A"}, "rust"), "qualifier_accepted: symbol defined in the file")
    check(not se.qualifier_accepted("self", "src/x.py", set(), "python") and se.qualifier_accepted("self", "src/x.rs", set(), "rust"),
          "qualifier_accepted: self only for Rust")
    for text, want in (
        ("let line_at = |i| i;", True), ("let mut line_at = move |i| i;", True),
        ("let line_at: Box<dyn Fn(usize) -> usize> = Box::new(|i| i);", False),
        ("let line_at: fn(usize) -> usize = |i| i;", True),
        ("let mut line_at = 0;", False), ("let line_at = root.join(\"x\");", False), ("let line_at = tools;", False),
        ("fn line_at(x: u32) {}", True), ("def line_at(x):", True), ("function line_at() {}", True),
        ("line_at = lambda x: x", True), ("line_at = function() {}", True), ("line_at = (a, b)", False),
        ("line_at := func(i int) int { return i }", True), ("line_at := 3", False),
        ("const line_at: fn(u32) -> u32 = helper;", True), ("static line_at: Lazy<Foo> = Lazy::new(f);", True),
        ("const line_at = (i) => i;", True), ("const line_at = async (i) => i;", True),
        ("const line_at = i => i;", True), ("const line_at = 3;", False),
        ("line_at: Callable[[int], int] = helper", True), ("line_at: int = 3", False),
        ("use foo::line_at;\nline_at(3);", False), ("let line_at_2 = |x| x; other_line_at(1);", False),
    ):
        check(se.binds_name_locally("line_at", text) is want, f"binds_name_locally({text!r}) is {want}")
    check(se._strip_comment_markers("-- foo bar", "c") == "-- foo bar" and se._strip_comment_markers("-- foo bar", "lua") == "-- foo bar",
          "_strip_comment_markers: -- kept while DASH_COMMENT_LANGUAGES is empty")
    saved_dash = se.DASH_COMMENT_LANGUAGES
    se.DASH_COMMENT_LANGUAGES = frozenset({"lua"})
    try:
        check(se._strip_comment_markers("-- foo bar", "lua") == "foo bar" and se._strip_comment_markers("-- foo bar", "c") == "-- foo bar",
              "_strip_comment_markers: -- stripped only for DASH_COMMENT_LANGUAGES members")
    finally:
        se.DASH_COMMENT_LANGUAGES = saved_dash

    # References classification helpers.
    check(se.owner_type("Database::file_id_for_path", "file_id_for_path") == "Database"
          and se.owner_type("Monolog\\Handler\\AbstractHandler\\setLevel", "setLevel") == "AbstractHandler"
          and se.owner_type("MapperContext<'a>::allowed", "allowed") == "MapperContext"
          and se.owner_type("Foo<Bar<Baz>>::name", "name") == "Foo"
          and se.owner_type("Vec<crate::Foo>::name", "name") == "Vec"
          and se.owner_type("line_at", "line_at") is None, "owner_type: segment before the name, generics stripped")
    check(se.call_shape(("    x.connect_all();",), 1, "    x.connect_all();".index("connect"), "connect") == ("bare", None),
          "call_shape: token must end at an identifier boundary")
    lines = ("    if let Some(id) = db.file_id_for_path(path)? {", "    obj->setLevel(x);", "    A::connect();",
             "    Ns\\connect();", "    connect();", "    let r = connect(1);")
    check(se.call_shape(lines, 1, lines[0].index("file_id_for_path"), "file_id_for_path") == ("receiver", None),
          "call_shape: `.` receiver")
    check(se.call_shape(lines, 2, lines[1].index("setLevel"), "setLevel") == ("receiver", None), "call_shape: `->` receiver")
    check(se.call_shape(lines, 3, lines[2].index("connect"), "connect") == ("source-qualified", "A"), "call_shape: :: qualifier")
    check(se.call_shape(lines, 4, lines[3].index("connect"), "connect") == ("source-qualified", "Ns"), "call_shape: \\ qualifier")
    check(se.call_shape(lines, 5, lines[4].index("connect"), "connect") == ("bare", None), "call_shape: bare")
    check(se.call_shape(lines, 6, lines[5].index("connect"), "connect") == ("bare", None), "call_shape: bare after `=`")
    check(se.call_shape(lines, 1, 0, "connect") == ("bare", None) and se.call_shape(None, 1, 0, "x") == ("bare", None)
          and se.call_shape(lines, 99, 0, "x") == ("bare", None), "call_shape: stale position / missing text -> bare")
    no_imports = (frozenset(), ())
    for language, importer, module, target, unrelated in [
        ("rust", "src/api/client.rs", "super::*", "src/api.rs", "src/other/api.rs"),
        ("rust", "crates/app/src/client.rs", "crate::api::*", "crates/app/src/api.rs", "src/api.rs"),
        ("rust", "src/client.rs", "crate::*", "src/lib.rs", "other/src/lib.rs"),
        ("rust", "src/api/mod.rs", "self::*", "src/api/mod.rs", "src/lib.rs"),
        ("python", "pkg/client.py", ".api.*", "pkg/api.py", "api.py"),
        ("python", "pkg/nested/client.py", "..*", "pkg/__init__.py", "pkg/nested/__init__.py"),
        ("python", "client.py", "pkg.api.*", "pkg/api.py", "other/pkg/api.py"),
    ]:
        imported = (frozenset({"*"}), (module,))
        check(se.glob_import_targets(module, language, importer, target),
              f"glob accepts the named module: {module} from {importer}")
        check(not se.bare_call_accepted(language, importer, imported, "run", unrelated, set()),
              f"glob rejects an unrelated same-name definition: {module} from {importer}")
    check(not se.glob_import_targets("super::super::*", "rust", "src/client.rs", "src/lib.rs"),
          "glob cannot escape the crate root")
    check(not se.glob_import_targets("...*", "python", "pkg/client.py", "__init__.py"),
          "glob cannot escape the Python project root")
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        for index, (source, symbol, qualified, runtime_exported, accepted) in enumerate([
            ("def run():\n    return 1\n", "run", "run", True, True),
            ("def _hidden():\n    return 1\n", "_hidden", "_hidden", False, False),
            ("__all__ = []\ndef run():\n    return 1\n", "run", "run", False, False),
            ("__all__ = ['_hidden']\ndef _hidden():\n    return 1\n", "_hidden", "_hidden", True, True),
            ("__all__ = [x for x in ['run']]\ndef run():\n    return 1\n", "run", "run", True, False),
            ("class Owner:\n    def run(self):\n        return 1\n", "run", "Owner.run", False, False),
            ("def outer():\n    def run():\n        return 1\n    return run\n", "run", "outer.run", False, False),
            ("__all__ = []\n__all__.append('run')\ndef run():\n    return 1\n", "run", "run", True, False),
            ("def __getattr__(name):\n    return []\ndef run():\n    return 1\n", "run", "run", False, False),
            ("def run():\n    return 1\nrun = 3\n", "run", "run", True, False),
        ]):
            module = f"exports_{index}"
            target = module + ".py"
            (root / target).write_text(source)
            runtime = subprocess.run(
                [sys.executable, "-c", f"from {module} import *; print({symbol!r} in globals())"],
                cwd=root, capture_output=True, text=True, check=True,
            )
            check((runtime.stdout.strip() == "True") == runtime_exported,
                  f"Python runtime export control: {module}")
            evidence = (frozenset({"*"}), (module + ".*",))
            check(se.bare_call_accepted("python", "client.py", evidence, symbol, target, set(), root, qualified) == accepted,
                  f"Python glob gold requires static top-level function export evidence: {module}")
        check(not se.bare_call_accepted("python", "client.py", (frozenset({"*"}), ("missing.*",)),
                                       "run", "missing.py", set(), root, "run"),
              "Python glob gold rejects missing source")
        invalid = "__all__ = ['missing', 'run']\ndef run():\n    return 1\n"
        (root / "invalid_exports.py").write_text(invalid)
        failed_import = subprocess.run([sys.executable, "-c", "from invalid_exports import *"],
                                       cwd=root, capture_output=True, text=True)
        check(failed_import.returncode != 0 and "AttributeError" in failed_import.stderr,
              "Python runtime rejects __all__ naming a missing binding")
        check(not se.python_glob_exports(invalid, "run"), "invalid __all__ cannot supply glob gold")
    check(se.bare_call_accepted("rust", "src/x.rs", (frozenset({"beta_one"}), ("crate::beta_one",)), "beta_one", "src/beta.c", set()),
          "bare_call_accepted: import row naming the symbol")
    check(not se.bare_call_accepted("rust", "src/x.rs", no_imports, "beta_one", "src/beta.c", set()),
          "bare_call_accepted: rust bare call without import dropped")
    check(se.bare_call_accepted("c", "src/epsilon.c", no_imports, "beta_one", "src/beta.c", set()),
          "bare_call_accepted: C same directory accepted")
    check(not se.bare_call_accepted("c", "src/sub/z.c", no_imports, "beta_one", "src/beta.c", set()),
          "bare_call_accepted: C other directory without include dropped")
    check(se.bare_call_accepted("c", "src/sub/e.c", (frozenset({"beta.h"}), ("../beta.h",)), "beta_one", "src/beta.c", set()),
          "bare_call_accepted: C include naming the defining file accepted")
    check(se.bare_call_accepted("go", "pkg/other/x.go", (frozenset({"store"}), ("github.com/x/y/pkg/store",)), "Open",
                                "pkg/store/db.go", set()), "bare_call_accepted: Go import path segment matches defining dir")


def main() -> int:
    with tempfile.TemporaryDirectory() as td:
        repo = Path(td) / "fixture"
        build_repo(repo, repo)
        out = Path(td) / "out"
        unit_checks()

        # ---------------- cochange ----------------
        rc, stdout = run_main(["--mode", "cochange", "--project", str(repo), "--out", str(out), "--commits", "20"])
        check(rc == 0, "cochange: exit 0")
        check("commits in scope: 10 of 10 scanned" in stdout, f"cochange: in-scope count (stdout: {stdout.splitlines()[:1]})")
        cochange_path = out / "fixture-cochange.yaml"
        check(cochange_path.is_file(), "cochange: writes <name>-cochange.yaml")
        doc = yaml.safe_load(cochange_path.read_text(encoding="utf-8"))
        check(doc["project_root"] == str(repo.resolve()), "cochange: project_root")
        cc = {c["query"]: c for c in doc["cases"]}
        check(set(cc) == set(EXPECTED_SUBJECTS), f"cochange: kept subjects (got {sorted(cc)})")
        for subject, gold in EXPECTED_SUBJECTS.items():
            check(cc.get(subject, {}).get("expected_files") == gold,
                  f"cochange: gold for {subject!r} (got {cc.get(subject, {}).get('expected_files')})")
        pair = cc.get("pair: alpha and beta together", {})
        check(set(pair) == {"id", "query", "expected_files", "source"}, "cochange: runner keys, no defining_file")
        check(pair.get("source", "").startswith("cochange:") and len(pair.get("source", "")) == len("cochange:") + 40,
              "cochange: source carries full sha")
        check(pair.get("id") == "cc-" + pair.get("source", "")[len("cochange:"):][:12], "cochange: id is cc-<sha12>")
        header = header_of(cochange_path)
        check("DISTINCT files" in header and "never" in header and "`score`" in header,
              "cochange: header documents distinct-file rank and no tie rule")
        check("Seed rule" in header and "Subject filter" in header and "recall@k <= k/|gold|" in header,
              "cochange: header documents seed, subject, and gold-cap rules")
        check_provenance(header, "cochange")
        head_sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True).stdout.strip()
        check(f"# project_head: {head_sha}" in header, "cochange: project_head is the fixture repo HEAD")

        rc, _ = run_main(["--mode", "cochange", "--project", str(repo), "--out", str(out / "all"),
                          "--commits", "20", "--include-all-types"])
        cc_all = cases_by_query(out / "all" / "fixture-cochange.yaml")
        check(rc == 0 and set(cc_all) == set(EXPECTED_SUBJECTS) | {
            "chore: bump alpha and beta versions", "docs: rewrite the beta notes"},
            f"cochange: --include-all-types admits chore + markdown-only docs (got {sorted(cc_all)})")
        check(cc_all.get("chore: bump alpha and beta versions", {}).get("expected_files") == ["src/beta.c"],
              "cochange: chore commit seeded by longest stem match")
        check(cc_all.get("docs: rewrite the beta notes", {}).get("expected_files") == ["docs/guide.md", "docs/other.md"],
              "cochange: docs commit seeded by stem, markdown gold remains")
        check("fix: it" not in cc_all, "cochange: < 3 content words skipped even with --include-all-types")

        rc, _ = run_main(["--mode", "cochange", "--project", str(repo), "--out", str(out / "wide"),
                          "--commits", "20", "--max-files", str(SCAFFOLD_CANDIDATES + 3)])
        wide = cases_by_query(out / "wide" / "fixture-cochange.yaml")
        bulk = wide.get("touch everything in the bulk tree at once", {})
        check(rc == 0 and len(bulk.get("expected_files", [])) == SCAFFOLD_CANDIDATES - 1
              and "src/bulk/file_00.rs" not in bulk.get("expected_files", []),
              "cochange: wide --max-files admits scaffold; parent-dir match seeds first bulk file")
        rc, _ = run_main(["--mode", "cochange", "--project", str(repo), "--out", str(out / "cap"),
                          "--commits", "20", "--max-files", str(SCAFFOLD_CANDIDATES - 1)])
        check(rc == 0 and "touch everything in the bulk tree at once" not in cases_by_query(out / "cap" / "fixture-cochange.yaml"),
              "cochange: --max-files counts the seed")

        # Project root as a git subdirectory: `-- .` scopes the walk, --relative keeps index paths.
        sub_root = Path(td) / "monorepo"
        build_repo(sub_root, sub_root / "sub", outside_commit=True)
        rc, stdout = run_main(["--mode", "cochange", "--project", str(sub_root / "sub"), "--out", str(out / "sub"),
                               "--commits", "20"])
        cc_sub = cases_by_query(out / "sub" / "fixture-cochange.yaml")
        check(rc == 0 and {k: v["expected_files"] for k, v in cc_sub.items()} == EXPECTED_SUBJECTS,
              f"cochange: subdirectory project matches index paths (got {sorted(cc_sub)})")
        check("commits in scope: 10 of 11 scanned" in stdout, "cochange: outside commit counted as out of scope")

        # ---------------- known-item (defining) ----------------
        rc, stdout = run_main(["--mode", "known-item", "--project", str(repo), "--out", str(out), "--seed", "7"])
        check(rc == 0, "known-item: exit 0")
        check("no/too many references" not in stdout and "doc-only (2..3 defining files): 5" in stdout,
              "known-item: refs counter absent and doc-only counter present in defining mode")
        check(se.SATURATION_NOTE in stdout, "known-item: saturation note printed in defining mode")
        ki_path = out / "fixture-known-item.yaml"
        check(ki_path.is_file(), "known-item: writes <name>-known-item.yaml")
        ki_cases = yaml.safe_load(ki_path.read_text(encoding="utf-8"))["cases"]
        by_id = {c["id"]: c for c in ki_cases}
        want_ids = {kid(p, n) + "-doc" for p, n in ELIGIBLE_ORDER} | {
            kid(p, n) + "-name" for p, n in ELIGIBLE_ORDER if n in NAME_CASE_SYMBOLS}
        check(set(by_id) == want_ids, f"known-item: exact case-id set (missing {sorted(want_ids - set(by_id))}, "
                                      f"extra {sorted(set(by_id) - want_ids)})")
        check(all("defining_file" not in c for c in ki_cases), "known-item: defining mode carries no defining_file")
        names = {c["query"] for c in ki_cases if c["source"] == "known-item:name"}
        check(names == NAME_CASE_SYMBOLS, f"known-item: name cases (got {sorted(names)})")
        dual_docs = [c for c in ki_cases if c["id"] in (kid("src/a_mod.rs", "dual") + "-doc", kid("src/alpha.rs", "dual") + "-doc")]
        check(len(dual_docs) == 2 and "dual" not in names and len({c["query"] for c in dual_docs}) == 2,
              "known-item: name defined in 2 files -> zero name cases, two distinct doc cases")
        render_ids = {kid(p, "render") + "-doc" for p in ("src/a_mod.rs", "src/alpha.rs", "src/render_three.rs")}
        check(render_ids <= set(by_id) and "render" not in names,
              "known-item: name defined in 3 files -> three doc cases, no name case")
        for absent, why in (
            ("alpha_two", "uncommented"), ("gamma_one", "short docstring"), ("delta_only", "no docblock"),
            ("shared_name", ">3 defining files"), ("test_alpha", "test path"), ("vendored_fn", "excluded"),
            ("beta_two", "#define run"), ("beta_four", "trailing */"), ("beta_five", "commented-out code"),
            ("epsilon_one", "license header"), ("delta_two", "< 4-token query"),
            ("alpha_test_helper", "#[cfg(test)] module"), ("alpha_test_helper_two", "second #[cfg(test)] module"),
            ("alpha_test_helper_three", "one-line #[cfg(all(test, unix))] module"),
            ("gamma_five", "next-def abort"), ("gamma_seven", "dedent abort"),
        ):
            check(absent not in names, f"known-item: {absent} skipped ({why})")
        alpha_id = kid("src/alpha.rs", "alpha_one")
        check(by_id.get(alpha_id + "-name", {}).get("expected_files") == ["src/alpha.rs"],
              "known-item: id is ki-<sha1(path\\0name)[:8]>-name and gold is the file")
        check(by_id.get(alpha_id + "-doc", {}).get("query") == "Cached lifetime instance lookups stay cheap",
              f"known-item: rust doc query (got {by_id.get(alpha_id + '-doc', {}).get('query')!r})")
        check(by_id.get(kid("src/beta.c", "beta_one") + "-doc", {}).get("query") == "First sentence enough words",
              "known-item: usage row deduped to MIN(line_start), first sentence only")
        cc_case = by_id.get(kid("src/alpha.rs", "cache_counter") + "-doc", {})
        check(cc_case.get("source") == "known-item:doc-leaky"
              and cc_case.get("query") == "Reset cache counter cache counter overflows budget",
              "known-item: leaky doc case tagged known-item:doc-leaky")
        leaky_ids = {c["id"] for c in ki_cases if c["source"] == "known-item:doc-leaky"}
        check(leaky_ids == {kid("src/alpha.rs", "cache_counter") + "-doc", kid("src/a_mod.rs", "dual") + "-doc",
                            kid("src/alpha.rs", "dual") + "-doc", kid("src/jar.py", "values") + "-doc"},
              f"known-item: leaky set is cache_counter, both `Dual ...` docs, and Jar.values (got {sorted(leaky_ids)})")
        check(by_id.get(kid("src/delta.php", "delta_one") + "-doc", {}).get("query")
              == "Normalises incoming payload before validation runs", "known-item: PHP doc query with tags stripped")
        gamma_docs = {c["query"] for c in ki_cases if c["source"] == "known-item:doc" and c["expected_files"] == ["src/gamma.py"]}
        check(gamma_docs == {"Docstring spanning multi-line signature coverage", "Helper used registry plugin registers late",
                             "Docstring wins comment block above def", "Docstring owned sixth helper never fifth one above"},
              f"known-item: python doc queries (got {sorted(gamma_docs)})")
        check(all(4 <= len(c["query"].split()) <= 12 for c in ki_cases if c["source"] != "known-item:name"),
              "known-item: doc query within [4, 12] tokens")
        ki_text = ki_path.read_text(encoding="utf-8")
        check(se.CONTAMINATION_NOTE in ki_text and se.SATURATION_NOTE in ki_text and "gold: defining" in ki_text
              and "recall@k <= k/|gold|" in ki_text, "known-item: contamination, saturation, gold, cap notes in header")
        check_provenance(header_of(ki_path), "known-item")

        # Deterministic sampling and stable ids (provenance line differs by timestamp only).
        def body(path: Path) -> str:
            return path.read_text(encoding="utf-8").split("project_root:", 1)[1]

        run_main(["--mode", "known-item", "--project", str(repo), "--out", str(out / "again"), "--seed", "7"])
        check(body(out / "again" / "fixture-known-item.yaml") == body(ki_path), "known-item: same seed reproduces the corpus body")
        run_main(["--mode", "known-item", "--project", str(repo), "--out", str(out / "seed8"), "--seed", "8"])
        ids8 = {c["id"] for c in yaml.safe_load((out / "seed8" / "fixture-known-item.yaml").read_text(encoding="utf-8"))["cases"]}
        check(ids8 == set(by_id), "known-item: ids stable across seeds")
        for seed in (1, 2, 3, 4, 5, 6):
            run_main(["--mode", "known-item", "--project", str(repo), "--out", str(out / f"s{seed}"),
                      "--sample", "2", "--seed", str(seed)])
            d = yaml.safe_load((out / f"s{seed}" / "fixture-known-item.yaml").read_text(encoding="utf-8"))
            got = {c["id"] for c in d["cases"] if c["id"].endswith("-doc")}
            picks = random.Random(seed).sample(ELIGIBLE_ORDER, 2)
            want = {kid(p, n) + "-doc" for p, n in picks}
            want_total = len(picks) + sum(1 for _p, n in picks if n in NAME_CASE_SYMBOLS)
            check(got == want and len(d["cases"]) == want_total,
                  f"known-item: --sample 2 seed {seed} draws {picks} in (path, line) order (got {sorted(got)})")

        run_main(["--mode", "known-item", "--project", str(repo), "--out", str(out / "tests"), "--include-tests"])
        inc = yaml.safe_load((out / "tests" / "fixture-known-item.yaml").read_text(encoding="utf-8"))["cases"]
        inc_names = {c["query"] for c in inc if c["source"] == "known-item:name"}
        check("test_alpha" in inc_names and "alpha_test_helper" not in inc_names,
              "known-item: --include-tests admits tests/ symbols but not #[cfg(test)] modules")

        # ---------------- known-item (references) ----------------
        rc, stdout = run_main(["--mode", "known-item", "--project", str(repo), "--out", str(out), "--gold", "references"])
        refs_path = out / "fixture-known-item-refs.yaml"
        check(rc == 0 and refs_path.is_file(), "known-item refs: writes <name>-known-item-refs.yaml")
        check("no/too many references skipped:" in stdout and se.SATURATION_NOTE not in stdout,
              "known-item refs: refs counter printed, saturation note not")
        check("multi-definition skipped: 9" in stdout and "doc-only" not in stdout,
              "known-item refs: one combined multi-definition counter (shared_name x4 + dual x2 + render x3), no doc-only")
        check("gate rust:" in stdout and "gate c:" in stdout and "gate php:" in stdout,
              "known-item refs: gate counts printed per language")
        refs_cases = yaml.safe_load(refs_path.read_text(encoding="utf-8"))["cases"]
        ref_names = {c["query"]: c["expected_files"] for c in refs_cases if c["source"] == "known-item:name"}
        check(ref_names == EXPECTED_REF_GOLD, f"known-item refs: gold per symbol (got {ref_names})")
        for name, absent, why in (
            ("alpha_one", "src/bulk/file_04.rs", "bare call without import row"),
            ("alpha_one", "src/bulk/file_05.rs", "import-only file"),
            ("alpha_one", "src/bulk/file_19.rs", "bare call without any import"),
            ("alpha_one", "src/bulk/file_24.rs", "closure shadow"),
            ("connect", "src/bulk/file_02.rs", "B::connect homonym"),
            ("connect", "src/bulk/file_22.rs", "UnixStream::connect"),
            ("lookup", "src/bulk/file_36.rs", "receiver call on a foreign owner type"),
            ("fmt", "src/Ns/Other.php", "receiver call in a file that never names the owner"),
            ("values", "src/sess.py", "builtin-protocol method name on an imported owner"),
            ("beta_one", "src/sub/zeta_c.c", "C other-dir bare call without include"),
            ("beta_one", "tests/alpha_test.rs", "test path"),
            ("lookup", "tests/lookup_test.rs", "same-language test path"),
            ("Message", "src/Ns/GelfHandler.php", "foreign-qualified type hint `\\Gelf\\Message`"),
            ("Message", "src/far/Note.php", "bare `extends Message` in another directory without an import"),
            ("Message", "src/far/Foreign.php", "bare `extends Message` under a same-tail foreign `use Other\\Message`"),
            ("A", "src/uses_a.rs", "Rust same-directory bare type hint without a `use`"),
            ("Message", "src/Ns/Shadow.php", "same-directory `extends Message` shadowed by `use Gelf\\Message`"),
        ):
            check(absent not in ref_names.get(name, []), f"known-item refs: {name} drops {absent} ({why})")
        check("src/bulk/file_35.rs" in ref_names.get("lookup", []),
              "known-item refs: receiver call with imported (generic) owner kept")
        check(ref_names.get("fmt") == ["src/Ns/Formatter.php"], "known-item refs: `$this->fmt()` kept via `extends Base`")
        check(ref_names.get("Base") == ["src/Ns/Formatter.php"] and "src/Ns/Other.php" not in ref_names.get("Base", []),
              "known-item refs: same-directory `extends Base` row is gold")
        check(ref_names.get("Message") == ["src/Ns/Wire.php", "src/far/Imported.php"],
              f"known-item refs: bare type rows kept via same directory or `use` import (got {ref_names.get('Message')})")
        check(ref_names.get("clear_expired") == ["src/sess.py"] and "values" not in ref_names,
              "known-item refs: Jar.clear_expired kept, Jar.values (builtin name) has no gold")
        check("gate python:" in stdout, "known-item refs: python gate line printed")
        check("src/bulk/file_37.rs" in ref_names.get("connect", []), "known-item refs: `a_mod::connect()` recovered from source")
        check("src/epsilon.c" in ref_names.get("beta_one", []), "known-item refs: C same-dir bare call kept")
        check("src/sub/eta.c" in ref_names.get("beta_one", []), "known-item refs: C include naming the defining file kept")
        check(len(ref_names.get("ten_caller", [])) == 10 and "cache_counter" not in ref_names,
              "known-item refs: MAX_REFERENCE_FILES pinned at the boundary (10 kept, 11 skipped)")
        check("delta_one" not in ref_names, "known-item refs: cross-language references ignored")
        refs_ids = {c["id"] for c in refs_cases}
        check(not any(kid(p, "render") + "-doc" in refs_ids for p in ("src/a_mod.rs", "src/alpha.rs", "src/render_three.rs")),
              "known-item refs: 3-definition render yields no refs-mode case")
        check(all(c.get("defining_file") for c in refs_cases)
              and next(c for c in refs_cases if c["query"] == "alpha_one")["defining_file"] == "src/alpha.rs",
              "known-item refs: every case carries defining_file")
        refs_text = refs_path.read_text(encoding="utf-8")
        check("gold: references" in refs_text and "import/include are" in refs_text and se.SATURATION_NOTE not in refs_text,
              "known-item refs: header names gold mode, no saturation note")
        check("Names defined in 2..3 files" not in refs_text, "known-item refs: no doc-only header line")
        check("explicit wildcard" in refs_text and "full reindex" in refs_text and "explicit wildcard" in se.__doc__,
              "known-item refs: glob-import rule requires a marker and names the legacy reindex remedy")
        check("structural first-hit floor" in refs_text and "receiver calls" in refs_text and "Gate counts" in refs_text,
              "known-item refs: header documents first-hit floor, receiver rule, and gate counts")
        check("259" not in se.__doc__ and "152" not in se.__doc__, "docstring states rules, not snapshot counts")
        check_provenance(header_of(refs_path), "known-item refs")

        # Gate counts are corpus-relevant: test-path candidates never reach the header,
        # even when the raw verdicts accept them (tests/lookup_test.rs is a kept receiver row).
        rconn = se.open_index(repo)
        a_mod_id = rconn.execute("SELECT id FROM files WHERE path = 'src/a_mod.rs'").fetchone()["id"]
        raw_lookup = se.reference_verdicts(rconn, repo, "lookup", a_mod_id, "src/a_mod.rs", "A<'a>::lookup", "rust")
        check(any(v.path == "tests/lookup_test.rs" and v.accepted and v.cls == "receiver" for v in raw_lookup),
              f"known-item refs: raw verdicts accept the test-path receiver call (got {raw_lookup})")
        gate_rust = next((l for l in stdout.splitlines() if "gate rust:" in l), "")
        check("import-only" in gate_rust and "receiver 1/1" in gate_rust,
              f"known-item refs: rust gate counts corpus-relevant files (got {gate_rust!r})")
        check("bare 11/0" not in gate_rust and "cache_counter" not in ref_names,
              f"known-item refs: rust gate excludes the over-cap cache_counter files (got {gate_rust!r})")
        gate_c = next((l for l in stdout.splitlines() if "gate c:" in l), "")
        check("bare 2/1" in gate_c, f"known-item refs: c gate counts 2 kept / 1 dropped (got {gate_c!r})")
        gate_php = next((l for l in stdout.splitlines() if "gate php:" in l), "")
        check("type-ref 3/4 (same-dir 2)" in gate_php and "receiver 1/1" in gate_php,
              f"known-item refs: php gate counts type-ref 3 kept / 4 dropped, 2 resting on same-dir (got {gate_php!r})")
        check("(same-dir N)" in refs_text and "PHP, Go," in refs_text,
              "known-item refs: header documents the same-dir sub-count and the languages it applies to")
        gate_python = next((l for l in stdout.splitlines() if "gate python:" in l), "")
        check("receiver 1/0" in gate_python,
              f"known-item refs: python gate omits the case-less Jar.values receiver drop (got {gate_python!r})")
        check("same-dir" not in gate_rust and "same-dir" not in gate_python,
              "known-item refs: same-dir sub-count only where a kept type-ref rests on the directory")
        check("only symbols that emit a case" in refs_text and "drawn by --sample" in refs_text,
              "known-item refs: header states the case-emitting, post-sample gate scope")

        # Gate counts cover the sampled symbols only: with --sample 1 exactly one
        # language line appears, and its kept files equal the emitted gold.
        for seed in (1, 2, 3, 4, 5):
            rc, s_out = run_main(["--mode", "known-item", "--project", str(repo), "--out", str(out / f"rs{seed}"),
                                  "--gold", "references", "--sample", "1", "--seed", str(seed)])
            s_cases = yaml.safe_load((out / f"rs{seed}" / "fixture-known-item-refs.yaml").read_text(encoding="utf-8"))["cases"]
            s_gate = [l.split("gate ", 1)[1] for l in s_out.splitlines() if "] gate " in l]
            s_lang = INDEX[s_cases[0]["defining_file"]][0]
            kept = sum(int(m.group(1)) for m in re.finditer(r" (\d+)/\d+", s_gate[0])) if s_gate else -1
            check(rc == 0 and len(s_gate) == 1 and s_gate[0].startswith(f"{s_lang}:")
                  and kept == len(s_cases[0]["expected_files"]),
                  f"known-item refs: --sample 1 seed {seed} gate counts the sampled symbol only (got {s_gate})")

        # type_ref_accepted arms: qualified / import / same-dir (PHP, Go, C/C++ only) / dropped.
        no_imports = (frozenset(), ())
        tra = se.type_ref_accepted
        check(tra("Ns\\Message", "Message", "Message", "src/far/X.php", no_imports, "src/Ns/Message.php", {"Message"}, "php")
              == "qualified", "type_ref_accepted: qualifier naming a defining-path segment / symbol -> qualified")
        check(tra("Message", "Message", "Message", "src/Ns/Wire.php", no_imports, "src/Ns/Message.php", set(), "php") == "same-dir"
              and tra("A", "A", "A", "src/uses_a.rs", no_imports, "src/a_mod.rs", set(), "rust") is None
              and tra("Jar", "Jar", "Jar", "src/sess.py", no_imports, "src/jar.py", set(), "python") is None,
              "type_ref_accepted: same-dir arm applies to PHP, not Rust / Python")
        check(tra("Message", "Message", "Message", "src/Ns/Shadow.php", (frozenset({"Message"}), ("Gelf\\Message",)),
                  "src/Ns/Message.php", set(), "php") is None,
              "type_ref_accepted: an unresolved same-tail import shadows the same-dir arm")
        check(tra("A", "A", "A", "src/bulk/x.rs", (frozenset({"A"}), ("crate::a_mod::A",)), "src/a_mod.rs", set(), "rust")
              == "import"
              and tra("Jar", "Jar", "Jar", "src/other/s.py", (frozenset({"Jar"}), ("jar.Jar",)), "src/jar.py", set(), "python")
              == "import"
              and tra("Message", "Message", "Message", "src/far/I.php", (frozenset({"Message"}), ("Message",)),
                      "src/Ns/Message.php", set(), "php") == "import",
              "type_ref_accepted: import arm accepts a bare import or a qualifier passing qualifier_accepted")
        check(tra("A", "A", "A", "src/bulk/x.rs", (frozenset({"A"}), ("other::A",)), "src/a_mod.rs", set(), "rust") is None
              and tra("Message", "Message", "Message", "src/far/F.php", (frozenset({"Message"}), ("Other\\Message",)),
                      "src/Ns/Message.php", set(), "php") is None
              and tra("Message", "Message", "Message", "src/far/F.php", (frozenset({"LogMessage"}), ("Ns\\LogMessage",)),
                      "src/Ns/Message.php", set(), "php") is None,
              "type_ref_accepted: import arm rejects foreign qualifiers and longer identifiers sharing the suffix")

        # ---------------- provenance helpers ----------------
        digest_a = se.index_digest(se.open_index(repo))
        digest_b = se.index_digest(se.open_index(repo))
        check(digest_a == digest_b, "index_digest: identical across two runs")
        mutated = Path(td) / "mutated"
        shutil.copytree(repo / ".codesage", mutated / ".codesage")
        mconn = sqlite3.connect(mutated / ".codesage" / "index.db")
        mconn.execute("UPDATE files SET content_hash = 'changed' WHERE path = 'src/alpha.rs'")
        mconn.commit()
        mconn.close()
        check(se.index_digest(se.open_index(mutated)) != digest_a, "index_digest: changes when a content_hash changes")

        rev_repo = Path(td) / "revrepo"
        rev_repo.mkdir()
        git(rev_repo, "init", "-q")
        shutil.copy(SCRIPT, rev_repo / "self-eval.py")
        git(rev_repo, "add", "self-eval.py")
        git(rev_repo, "commit", "-q", "-m", "seed")
        clean = se.script_revision(rev_repo / "self-eval.py")
        check(len(clean) == 40 and "-dirty" not in clean, f"script_revision: clean tree is the bare sha (got {clean!r})")
        with (rev_repo / "self-eval.py").open("a", encoding="utf-8") as fh:
            fh.write("# local edit\n")
        dirty = se.script_revision(rev_repo / "self-eval.py")
        check(dirty.startswith(clean + "-dirty+sha256:") and len(dirty) == 40 + len("-dirty+sha256:") + 8,
              f"script_revision: dirty tree appends -dirty+sha256:<8> (got {dirty!r})")
        real_run = se.subprocess.run

        def _no_git(*_a, **_k):
            raise FileNotFoundError("git")

        se.subprocess.run = _no_git
        try:
            check(se._git(repo, ["rev-parse", "HEAD"]) is None, "_git: missing binary returns None")
            check(se.script_revision(SCRIPT).startswith("sha256:"), "script_revision: falls back to sha256 without git")
        finally:
            se.subprocess.run = real_run

        # ---------------- runner: defining_file exclusion ----------------
        loader = importlib.machinery.SourceFileLoader("bench_runner", str(HERE / "codesage-bench-runner"))
        runner_spec = importlib.util.spec_from_loader("bench_runner", loader)
        runner = importlib.util.module_from_spec(runner_spec)
        loader.exec_module(runner)
        rows = [("src/alpha.rs", "def"), ("src/beta.c", "caller")]
        check(runner.returned_for_case(rows, {"defining_file": "src/alpha.rs"}) == [("src/beta.c", "caller")],
              "runner: defining_file dropped before ranking")
        check(runner.returned_for_case(rows, {}) == rows and runner.returned_for_case(rows, {"defining_file": None}) == rows,
              "runner: cases without defining_file are untouched")
        check(runner.score_case(["src/beta.c"], runner.returned_for_case(rows, {"defining_file": "src/alpha.rs"}), [5, 10])
              ["first_hit_rank"] == 1, "runner: refs-mode first-hit floor of 2 disappears")
        runner_src = (HERE / "codesage-bench-runner").read_text(encoding="utf-8")
        check(not hasattr(runner, "search_limit_for_case") and "limit + 1" not in runner_src
              and "run_codesage_search(args.codesage_bin, project_root, query, args.limit)" in runner_src,
              "runner: every case searches at --limit (no refs-mode over-fetch)")
        three = [("src/alpha.rs", "def"), ("src/beta.c", "caller"), ("src/gamma.py", "other")]
        check(runner.returned_for_case(three, {"defining_file": "src/alpha.rs"}) == three[1:]
              and runner.returned_for_case(three, {}) == three,
              "runner: the excluded defining_file costs a slot; the returned length is the list after the drop")
        hdr = runner.format_header(
            project_root=Path("/p"), head_sha="abc", size_str="s", corpus_name="c.yaml", case_count=4, limit=10,
            k_values=[5, 10], cs_version="v", embedding_model="m", embedding_device="", embedding_dim="",
            reranker="none", baseline="b", run_ts="t", refs_mode_cases=3,
        )
        check(any("Refs-mode adjustment" in l and "(3/4 cases affected" in l and "costs one slot" in l
                  and "comparable only with other refs-mode runs at the same --limit (10)" in l and "top-11" not in l
                  for l in hdr),
              "runner: header discloses the slot cost, the affected-case count, and the comparability limit")
        hdr_plain = runner.format_header(
            project_root=Path("/p"), head_sha="abc", size_str="s", corpus_name="c.yaml", case_count=4, limit=10,
            k_values=[5, 10], cs_version="v", embedding_model="m", embedding_device="", embedding_dim="",
            reranker="none", baseline="b", run_ts="t",
        )
        check(not any("Refs-mode adjustment" in l for l in hdr_plain), "runner: no refs-mode line without defining_file cases")

        # ---------------- both ----------------
        rc, _ = run_main(["--mode", "both", "--project", str(repo), "--out", str(out / "both")])
        check(rc == 0 and (out / "both" / "fixture-cochange.yaml").is_file()
              and (out / "both" / "fixture-known-item.yaml").is_file(), "both: writes two YAMLs")

        # ---------------- CLI: validation exits, warnings, non-git project ----------------
        def run_cli(*argv: str) -> subprocess.CompletedProcess:
            return subprocess.run([sys.executable, str(SCRIPT), *argv], capture_output=True, text=True, timeout=120)

        base = ["--mode", "cochange", "--project", str(repo), "--out", str(out / "cli")]
        r = run_cli(*base, "--min-files", "1")
        check(r.returncode == 2 and "--min-files must be >= 2" in r.stderr, f"cli: --min-files 1 exits 2 (rc={r.returncode})")
        r = run_cli(*base, "--max-files", "1")
        check(r.returncode == 2 and "--max-files must be >= --min-files" in r.stderr, "cli: --max-files < --min-files exits 2")
        r = run_cli(*base, "--sample", "0")
        check(r.returncode == 2 and "--sample must be >= 1" in r.stderr, f"cli: --sample 0 exits 2 (rc={r.returncode})")
        r = run_cli("--mode", "cochange", "--project", str(Path(td) / "missing"))
        check(r.returncode == 2 and "project root does not exist" in r.stderr, "cli: missing project exits 2")
        r = run_cli(*base, "--gold", "references")
        check(r.returncode == 0 and "warn" in r.stderr and "--gold" in r.stderr, "cli: --gold with --mode cochange warns")
        r = run_cli(*base)
        check(r.returncode == 0 and "--gold" not in r.stderr, "cli: no --gold warning when the flag is absent")

        nogit = Path(td) / "nogit"
        shutil.copytree(repo, nogit, ignore=shutil.ignore_patterns(".git"))
        r = run_cli("--mode", "both", "--project", str(nogit), "--out", str(out / "nogit"))
        check(r.returncode == 0 and "warn" in r.stderr and (out / "nogit" / "fixture-known-item.yaml").is_file()
              and not (out / "nogit" / "fixture-cochange.yaml").exists(),
              f"cli: --mode both on a non-git project warns and still writes known-item (rc={r.returncode})")
        check("project_head: not-a-git-repo" in header_of(out / "nogit" / "fixture-known-item.yaml"),
              "cli: non-git project records project_head: not-a-git-repo")
        r = run_cli("--mode", "cochange", "--project", str(nogit), "--out", str(out / "nogit2"))
        check(r.returncode == 1 and "warn" in r.stderr, f"cli: --mode cochange on a non-git project exits 1 (rc={r.returncode})")

    if failures:
        print(f"FAILED ({len(failures)}):")
        for f in failures:
            print(f)
        return 1
    print("all self-eval tests passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
