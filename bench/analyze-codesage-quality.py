#!/usr/bin/env python3
"""
Retrospective *quality + utility* analysis of CodeSage MCP usage from Claude
Code session logs.

Sibling to `bench/analyze-codesage-usage.py`. That script measures volume
(how many bytes each codesage tool returned); this script measures whether
those calls actually helped the agent. Questions answered:

- How often did codesage return an empty or error payload?
- When the agent reached for Grep, how often was the pattern something
  codesage would have answered in one call (single identifier or
  pipe-joined identifier list)?
- After a codesage tool_result, how often did the agent immediately reach
  for Grep on a symbol it had just asked codesage about? That is a
  "codesage didn't satisfy me" signal — distinct from a genuine follow-up
  on a different token.
- Per tool, how often was a codesage call *terminal*: no native retrieval
  (Grep / Glob / unexplained Read / grep-shaped Bash) within the next N
  tool calls. Definition adapted from ripwire `docs/METHODOLOGY.md` §9.

No forward instrumentation; all signal is extracted from transcripts at
`~/.claude/projects/*/*.jsonl` (main sessions) and
`~/.claude/projects/*/*/subagents/*.jsonl` (subagent transcripts).

Usage:
  bench/analyze-codesage-quality.py [--window-days 7] [--terminality-window 3] [--projects-root PATH] [--output PATH]

Stdlib only.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import re
import shlex
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any

TOOL_PREFIX = "mcp__codesage__"

# Risk and coupling calls are outside the retrieval-selection denominator.
RETRIEVAL_CODESAGE_TOOLS = {
    "search",
    "find_symbol",
    "find_references",
    "impact_analysis",
    "export_context",
    "list_dependencies",
}

# Short ASCII identifiers such as `fd` and `pt` count.
IDENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")

# Allow `|` and whitespace for identifier alternatives and keywords (`fn foo`).
REGEX_META_RE = re.compile(r"[.\\\[\](){}^$*+?]")




def is_subagent_transcript(path: Path) -> bool:
    return path.parent.name == "subagents"


def iter_transcripts(root: Path, min_mtime: float) -> list[Path]:
    """Top-level session transcripts plus subagent transcripts under
    `<project>/<session>/subagents/*.jsonl`, both filtered by mtime.
    """
    out: list[Path] = []
    if not root.is_dir():
        return out
    for project in root.iterdir():
        if not project.is_dir():
            continue
        candidates = list(project.glob("*.jsonl")) + list(project.glob("*/subagents/*.jsonl"))
        for f in candidates:
            try:
                if f.stat().st_mtime >= min_mtime:
                    out.append(f)
            except OSError:
                continue
    return out


def flatten_result(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts: list[str] = []
        for item in content:
            if isinstance(item, dict):
                t = item.get("text")
                if isinstance(t, str):
                    parts.append(t)
        return "\n".join(parts)
    return ""


def extract_events(transcript: Path) -> list[dict[str, Any]]:
    """Return an ordered list of user / tool_use / tool_result events for one
    transcript. Each event carries enough context to compute the metrics
    without re-parsing.

    `user` events carry plain text from the user's typed prompt (excluding
    `tool_result` wrapper messages that also carry role=user). They are
    used by the §2.3 question-shape breakdown to attribute each retrieval
    decision back to the user question that prompted it.
    """
    try:
        fp = transcript.open("r", encoding="utf-8", errors="replace")
    except OSError:
        return []
    events: list[dict[str, Any]] = []
    pending_use: dict[str, dict[str, Any]] = {}
    with fp:
        for line in fp:
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            msg = obj.get("message") if isinstance(obj, dict) else None
            if not isinstance(msg, dict):
                continue
            role = msg.get("role")
            content = msg.get("content")
            ts = obj.get("timestamp") if isinstance(obj, dict) else None

            # tool_result blocks also use role=user; they are not user questions.
            if role == "user":
                user_text = ""
                has_tool_result = False
                if isinstance(content, str):
                    user_text = content
                elif isinstance(content, list):
                    parts = []
                    for c in content:
                        if not isinstance(c, dict):
                            continue
                        if c.get("type") == "tool_result":
                            has_tool_result = True
                            break
                        t = c.get("text")
                        if isinstance(t, str):
                            parts.append(t)
                    if not has_tool_result:
                        user_text = "\n".join(parts).strip()
                if user_text:
                    events.append({"kind": "user", "text": user_text, "ts": ts})
                if not has_tool_result:
                    continue

            if not isinstance(content, list):
                continue
            for c in content:
                if not isinstance(c, dict):
                    continue
                ctype = c.get("type")
                if ctype == "tool_use":
                    name = c.get("name") or ""
                    tid = c.get("id")
                    inp = c.get("input") or {}
                    ev = {
                        "kind": "tool_use",
                        "tool": name,
                        "id": tid,
                        "input": inp,
                        "ts": ts,
                    }
                    events.append(ev)
                    if tid:
                        pending_use[tid] = ev
                elif ctype == "tool_result":
                    tid = c.get("tool_use_id")
                    text = flatten_result(c.get("content"))
                    use = pending_use.pop(tid, None)
                    events.append({
                        "kind": "tool_result",
                        "id": tid,
                        "text": text,
                        "ts": ts,
                        "pair_tool": (use or {}).get("tool"),
                        "pair_input": (use or {}).get("input", {}),
                    })
    return events




def classify_codesage_result(text: str) -> str:
    """Bucket a codesage tool_result payload into quality categories.

    Returns one of: `empty`, `error`, `ok`. `empty` means the agent got a
    structurally empty answer (empty array, empty object); `error` means an
    MCP-level or parameter-parse failure.
    """
    t = text.strip()
    if not t:
        return "empty"
    if t.startswith("MCP error") or t.startswith("Error:") or t.startswith("Exit code "):
        return "error"
    try:
        data = json.loads(t)
    except (json.JSONDecodeError, ValueError):
        return "ok"
    if data is None:
        return "empty"
    if isinstance(data, list) and not data:
        return "empty"
    if isinstance(data, dict):
        has_content = False
        for v in data.values():
            if isinstance(v, list) and v:
                has_content = True
                break
            if isinstance(v, dict) and v:
                has_content = True
                break
            if isinstance(v, (str, int, float, bool)) and v not in ("", 0, False):
                has_content = True
                break
        if not has_content:
            return "empty"
    return "ok"


# Require question or implementation-request phrasing, not just topic keywords.
SEMANTIC_QUESTION_RES = [
    re.compile(r"\bwhere\s+(does|is|are|do\s+we)\b.*\b(handle|happen|live|loaded|defined|managed|stored|implemented|done|fire|trigger)",
               re.IGNORECASE),
    re.compile(r"\bhow\s+(does|do\s+we|is)\b.*\b(work|handle|implement|done|done\b)", re.IGNORECASE),
    re.compile(r"\bfind\s+(the\s+)?(file|place|spot|code|spot)\s+(that|where)\b", re.IGNORECASE),
    re.compile(r"\bwhich\s+file\s+(handles|implements|contains|holds|owns)", re.IGNORECASE),
    re.compile(r"\bwhat\s+(handles|implements|does)\b", re.IGNORECASE),
    re.compile(r"\b(look|take\s+a\s+look)\s+at\s+(the\s+)?\w+\s+(flow|module|code|pipeline|logic|path|handler|system|layer)",
               re.IGNORECASE),
    re.compile(r"\bshow\s+me\s+(where|the|how)\s+\w+", re.IGNORECASE),
    re.compile(r"\bin\s+the\s+\w+\s+(flow|module|pipeline|handler|layer|system)\b", re.IGNORECASE),
    re.compile(r"\bfix\s+(the\s+)?\w+\s+(flow|code|logic|pipeline|handler|bug)", re.IGNORECASE),
    re.compile(r"\bexplain\s+(the\s+|how\s+)?\w+\s+(flow|works|module|code|logic)", re.IGNORECASE),
]
IDENTIFIER_QUESTION_RES = [
    re.compile(r"`[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*`"),
    re.compile(r"\bwhere\s+is\s+[A-Za-z_][A-Za-z0-9_]{3,}\s+(defined|declared|called|used|implemented)",
               re.IGNORECASE),
    re.compile(r"\bfind\s+(all\s+)?(references?|callers?|callees?)\s+(to|of)\s+[A-Za-z_]"),
    re.compile(r"\b(what|who)\s+calls\s+[A-Za-z_]"),
]
LITERAL_QUESTION_RES = [
    re.compile(r"\b(TODO|FIXME|XXX|HACK)\b"),
    re.compile(r"\bgrep(\s+for)?\s+[\"']", re.IGNORECASE),
    re.compile(r"\bsearch\s+for\s+[\"']", re.IGNORECASE),
    re.compile(r"\b(error|warning|log)\s+message\b", re.IGNORECASE),
]
# Pasted commands and harness notifications also arrive as role=user.
NON_QUESTION_RES = [
    re.compile(r"<task-notification>", re.IGNORECASE),
    re.compile(r"<command-(name|args|message)>", re.IGNORECASE),
    re.compile(r"^\s*#\s*\w[\w\s]+\n+##", re.MULTILINE),  # markdown body with multi-heading
    re.compile(r"<wiki-hint>", re.IGNORECASE),
    re.compile(r"<system-reminder>", re.IGNORECASE),
    re.compile(r"^\s*---\s*\nname:", re.MULTILINE),  # frontmatter — pasted skill/command body
    re.compile(r"\[Request\s+interrupted\s+by\s+user\]"),
]


def is_real_user_question(text: str) -> bool:
    """Filter out pasted slash-command bodies, system reminders, hook
    events, and task notifications that arrive on `role=user` but aren't
    actual user-typed questions.
    """
    if not text or not text.strip():
        return False
    if len(text) > 2000:
        # Long messages are likely pasted command or skill bodies.
        return False
    for r in NON_QUESTION_RES:
        if r.search(text):
            return False
    return True


def classify_user_question(text: str) -> str:
    """Bucket a user message into a retrieval question shape.

    Returns one of: `semantic`, `identifier`, `literal`, `other`. The
    classifier is conservative — it errs toward `other` rather than
    mis-categorize, because the §2.3 question is "what fraction of
    *clearly* semantic-shaped questions go to CodeSage's `search`",
    not "what fraction of every user message".

    Precedence: identifier > literal > semantic > other. Identifier wins
    when both fire because a backticked symbol is a stronger signal
    than concept words appearing in the surrounding sentence.
    """
    if not text:
        return "other"
    snippet = text.strip()[:500]
    for r in IDENTIFIER_QUESTION_RES:
        if r.search(snippet):
            return "identifier"
    for r in LITERAL_QUESTION_RES:
        if r.search(snippet):
            return "literal"
    for r in SEMANTIC_QUESTION_RES:
        if r.search(snippet):
            return "semantic"
    return "other"


def identifier_shaped_grep(pattern: str) -> bool:
    """True when the Grep pattern is a pure identifier or pipe-joined
    identifier list — exactly the shape CodeSage's `find_symbol` /
    `find_references` would answer in one call.

    Conservative: any regex wildcard (`.`, `*`, `[`, `^`, `$`, etc.) outside
    pipe / whitespace / underscore disqualifies.
    """
    if not pattern or len(pattern) < 2:
        return False
    p = pattern.strip()
    if REGEX_META_RE.search(p):
        return False
    for alt in p.split("|"):
        tokens = alt.strip().split()
        if not tokens:
            return False
        if not all(IDENT_RE.match(tok) for tok in tokens):
            return False
    return True


def extract_grep_identifiers(pattern: str) -> set[str]:
    """Return the set of identifier tokens inside a Grep pattern. Used to
    detect "codesage answered then agent re-grepped same token" follow-ups.
    """
    if not pattern:
        return set()
    out: set[str] = set()
    # Mixed regexes can still identify subjects for follow-up matching.
    for token in re.findall(r"[A-Za-z_][A-Za-z0-9_]*", pattern):
        if len(token) >= 3:  # skip noise like 'a', 'if', 'fn'
            out.add(token)
    return out


def extract_codesage_subject(event: dict[str, Any]) -> set[str]:
    """Identifiers mentioned in a codesage call. Drawn from the input
    params (symbol name, file path base, query tokens) and, for a
    tool_result, the response payload when it's small enough to scan.
    Used to detect "agent re-grepped the same symbol after codesage".
    """
    subjects: set[str] = set()
    inp = event.get("input") or event.get("pair_input") or {}
    for key in ("name", "file_path", "target", "query"):
        val = inp.get(key)
        if isinstance(val, str):
            for token in re.findall(r"[A-Za-z_][A-Za-z0-9_]*", val):
                if len(token) >= 3:
                    subjects.add(token)
    text = event.get("text") or ""
    if text and len(text) < 8192:
        for token in re.findall(r"[A-Za-z_][A-Za-z0-9_]*", text):
            if len(token) >= 4:
                subjects.add(token)
    return subjects



DEFAULT_TERMINALITY_WINDOW = 3

# Require a slash and a letter-led extension to exclude basenames and decimals.
RESULT_PATH_RE = re.compile(r"[\w./-]+/[\w.-]+\.[A-Za-z]\w*")
LEADING_DOT_SEGMENTS_RE = re.compile(r"^(\.\.?/)+")
# Only explicit path fields supply follow-through evidence in structured results.
RESULT_PATH_KEYS = {"file_path", "file", "path"}
RESULT_PATH_KEY_SUFFIXES = ("_file", "_path", "files", "paths")

NATIVE_GREP_WORDS = {"grep", "egrep", "fgrep", "rg"}
# Untracked directory changes make relative paths ineligible for retrieval.
CWD_UNKNOWN = "<unknown-cwd>"
GIT_REF_RE = re.compile(r"^(origin|upstream|refs)/|\.\.|[\^~]|@\{")
# Include RTK wrappers when locating the underlying command.
BASH_WRAPPER_WORDS = {
    "rtk", "proxy", "sudo", "command", "nice", "time", "timeout", "nohup", "env", "stdbuf", "xvfb-run",
}
WRAPPER_FLAG_ARGS: dict[str, set[str]] = {
    "sudo": {"-u", "--user", "-g", "--group", "-C", "-D", "-h", "-p", "-r", "-t", "-T", "-U"},
    "timeout": {"-s", "-k", "--signal", "--kill-after"},
    "env": {"-u", "--unset", "-C", "--chdir", "-S", "--split-string"},
    "stdbuf": {"-i", "-o", "-e"},
    "nice": {"-n"},
    "xvfb-run": {"-s", "-n", "-f", "-p", "-w", "-l", "-e"},
}
TIMEOUT_DURATION_RE = re.compile(r"^\d+(\.\d+)?[smhd]?$")
ENV_ASSIGNMENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
# Flag arguments are not search paths; combined flags can consume them too.
GREP_ARG_FLAG_RES = {
    "grep": re.compile(r"^-[A-Za-z]*[ABCDdefm]$"),
    "rg": re.compile(r"^-[A-Za-z]*[ABCEefgjmMtT]$"),
}
GREP_PATTERN_FLAG_RES = {
    "grep": re.compile(r"^-[A-Za-z]*[ef]$"),
    "rg": re.compile(r"^-[A-Za-z]*[ef]$"),
}
GREP_PATTERN_ATTACHED_RE = re.compile(r"^-[ef](\S+)$")
GREP_PATTERN_LONG_PREFIXES = ("--regexp=", "--file=")
GREP_PATTERN_LONG_FLAGS = {"--regexp", "--file"}
# Directory components or source extensions distinguish pathspecs from refs.
SOURCE_EXT_RE = re.compile(
    r"\.(rs|py|php|phpt|c|h|cc|cpp|cxx|hpp|hh|java|js|jsx|mjs|ts|tsx|go|toml|yaml|yml|json|md|txt|sh|"
    r"scm|sql|m4|w32|lock|cfg|ini|xml|html|css|scss|proto|rb|pl|swift|kt|zig)$",
    re.IGNORECASE,
)
# Orchestration and bookkeeping must not consume retrieval-window slots.
NON_SLOT_TOOLS = {
    "AskUserQuestion", "Agent", "Task", "Skill", "TodoWrite", "Monitor", "ListAgents", "SendMessage",
    "ToolSearch", "TaskOutput", "TaskStop", "TaskUpdate",
}
# Keep pipelines intact: a downstream grep only filters output.
# Preserve find's escaped `\;` terminator. A preceding escaped backslash
# also swallows the boundary because shell quoting is not fully modeled.
BASH_STATEMENT_SPLIT_RE = re.compile(r"\|\||&&|(?<!\\);|\n")
HEREDOC_START_RE = re.compile(r"<<-?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1")
GREP_RECURSIVE_FLAG_RE = re.compile(r"^-[A-Za-z]*[rR][A-Za-z]*$")
# Redirection targets are not retrieval paths.
REDIRECT_BARE_RE = re.compile(r"^(\d*>{1,2}|\d*<{1,3}|&>{1,2})$")
REDIRECT_ATTACHED_RE = re.compile(r"^(\d*>{1,2}|\d*<{1,3}|&>{1,2})\S")
FIND_NAME_PREDICATES = {"-name", "-iname", "-path", "-ipath", "-wholename", "-regex"}
# Only allowlisted read-only utilities keep find -exec eligible for retrieval.
FIND_MUTATING_ACTIONS = {"-delete", "-fls", "-fprint", "-fprint0", "-fprintf"}
FIND_EXEC_ACTIONS = {"-exec", "-execdir", "-ok", "-okdir"}
FIND_READONLY_EXEC_UTILS = {
    "cat", "grep", "rg", "egrep", "fgrep", "head", "tail", "wc", "ls", "stat", "file", "echo", "nl", "od", "jq",
}
FIND_EXEC_TERMINATORS = {";", "+", "{}"}
NON_CODE_PATH_PREFIXES = ("/tmp/", "/proc/", "/dev/", "/var/log/", "/var/tmp/", "/run/")
NON_CODE_PATH_SUFFIXES = (".log", ".output", ".out", ".err")

EDIT_TOOLS = {"Edit", "Write", "MultiEdit"}


def _norm_path(p: str) -> str:
    p = p.strip().strip("'\"`")
    p = LEADING_DOT_SEGMENTS_RE.sub("", p)
    return p.rstrip("/")


def _abs_norm(p: str, project_root: str | None) -> str:
    """Absolute, normalized form of a path; relative paths resolve against
    the codesage call's `project` root.
    """
    p = os.path.expanduser(p.strip().strip("'\"`"))
    if not os.path.isabs(p) and project_root:
        p = os.path.join(project_root, p)
    return os.path.normpath(p)


def _walk_json_paths(node: Any, key: str | None, out: set[str]) -> None:
    if isinstance(node, dict):
        for k, v in node.items():
            _walk_json_paths(v, k, out)
        return
    if isinstance(node, list):
        for item in node:
            _walk_json_paths(item, key, out)
        return
    if not isinstance(node, str) or key is None:
        return
    if key in RESULT_PATH_KEYS or key.endswith(RESULT_PATH_KEY_SUFFIXES):
        p = _norm_path(node)
        if p:
            out.add(p)


FENCE_RE = re.compile(r"^```[A-Za-z0-9_-]*[ \t]*\n|\n```[ \t]*$")


def _parse_result_json(t: str) -> Any:
    """json.loads with recovery for fenced or prose-prefixed payloads: strip
    a leading/trailing ``` fence, then retry from the first `{`/`[` to the
    last `}`/`]`.
    """
    try:
        return json.loads(t)
    except (json.JSONDecodeError, ValueError):
        pass
    stripped = FENCE_RE.sub("", t).strip()
    starts = [i for i in (stripped.find("{"), stripped.find("[")) if i >= 0]
    if not starts:
        return None
    start = min(starts)
    end = max(stripped.rfind("}"), stripped.rfind("]"))
    if end < start:
        return None
    try:
        return json.loads(stripped[start:end + 1])
    except (json.JSONDecodeError, ValueError):
        return None


def extract_result_paths(text: str) -> set[str]:
    """File paths a codesage result named. JSON results yield only the values
    of structured path fields; the regex fallback runs on non-JSON text and
    requires a `/` in the candidate so prose and code snippets that mention
    a bare basename (`Cargo.toml`) don't excuse an unrelated Read.
    """
    t = (text or "").strip()
    if not t:
        return set()
    out: set[str] = set()
    data = _parse_result_json(t)
    if isinstance(data, (dict, list)):
        _walk_json_paths(data, None, out)
        return out
    if "{" in t or "[" in t:
        # Do not extract paths from code inside malformed structured payloads.
        return out
    for m in RESULT_PATH_RE.finditer(t):
        p = _norm_path(m.group(0))
        if p:
            out.add(p)
    return out


def read_hits_result(read_path: str, result_paths: set[str], project_root: str | None = None) -> bool:
    """True when the Read's path is one the result named. With a project
    root, result paths resolve against it and must match exactly; without
    one, a component-boundary suffix match is used and the result path must
    carry a directory component.
    """
    rp = _norm_path(read_path)
    if not rp:
        return False
    read_abs = _abs_norm(rp, project_root)
    for p in result_paths:
        if project_root:
            if _abs_norm(p, project_root) == read_abs:
                return True
            continue
        if "/" in p and (rp == p or rp.endswith("/" + p)):
            return True
    return False


def _strip_heredocs(command: str) -> str:
    out: list[str] = []
    pos = 0
    while True:
        m = HEREDOC_START_RE.search(command, pos)
        if not m:
            out.append(command[pos:])
            break
        line_end = command.find("\n", m.end())
        if line_end == -1:
            out.append(command[pos:m.start()])
            break
        out.append(command[pos:line_end])
        terminator = m.group(2)
        body_pos = line_end + 1
        pos = len(command)
        for line_m in re.finditer(r"^[ \t]*(.*?)[ \t]*$", command[body_pos:], re.MULTILINE):
            if line_m.group(1) == terminator:
                pos = body_pos + line_m.end()
                break
    return "".join(out)


def _tokenize(statement: str) -> list[str]:
    try:
        return shlex.split(statement, posix=True)
    except ValueError:
        return statement.split()


def _is_scratch_abs(norm: str) -> bool:
    return any(norm == p.rstrip("/") or norm.startswith(p) for p in NON_CODE_PATH_PREFIXES)


def _code_path_arg(arg: str, project_root: str | None, cwd: str | None = None) -> bool:
    """Path guard: the argument names something under the project (or, when
    the working directory is unknown, a relative path / `.`) and is not a
    scratch file, log, or task output. Relative arguments resolve against
    `cwd` (tracked across `cd` statements; defaults to the project root).
    Absolute paths outside the declared root, and any absolute path when no
    root is known, fail the guard — terminal-favouring by design.
    """
    a = arg.strip().rstrip(")")
    if not a or a.startswith("-"):
        return False
    if a.lower().endswith(NON_CODE_PATH_SUFFIXES):
        return False
    expanded = os.path.expanduser(a)
    if not os.path.isabs(expanded):
        if cwd == CWD_UNKNOWN:
            return False
        if cwd:
            expanded = os.path.join(cwd, expanded)
    if os.path.isabs(expanded):
        norm = os.path.normpath(expanded)
        if _is_scratch_abs(norm):
            return False
        if not project_root:
            return False
        root = os.path.normpath(project_root)
        return norm == root or norm.startswith(root + os.sep)
    return "/tmp/" not in expanded and not expanded.startswith("/proc")


def _parse_grep(word: str, args: list[str]) -> tuple[str | None, list[str], bool]:
    """(pattern, paths, recursive) for a `grep`/`rg`/`git grep` argument
    list. Flag arguments (`-e PAT`, `-f FILE`, `-A 25`) are consumed and
    never treated as paths. `pattern` is None when no pattern is present
    and "" when patterns come from a file (`-f`, `--file`) and are unknown.
    The recursive flag is also read off combined tokens (`-rne foo`).
    """
    family = "rg" if word == "rg" else "grep"
    arg_flag_re = GREP_ARG_FLAG_RES[family]
    pattern_flag_re = GREP_PATTERN_FLAG_RES[family]
    positional: list[str] = []
    explicit_pattern: str | None = None
    recursive = word in ("rg", "git grep")
    skip = False
    capture_pattern = False

    def note_recursive(tok: str) -> None:
        nonlocal recursive
        if GREP_RECURSIVE_FLAG_RE.match(tok) or tok.startswith(("--recursive", "--dereference-recursive")):
            recursive = True

    for a in args:
        if skip:
            skip = False
            if capture_pattern:
                capture_pattern = False
                explicit_pattern = explicit_pattern or a
            continue
        if a in GREP_PATTERN_LONG_FLAGS:
            # `--file FILE` supplies patterns from a file: pattern unknown.
            explicit_pattern = explicit_pattern or ""
            skip = True
            capture_pattern = a == "--regexp"
            continue
        if a.startswith(GREP_PATTERN_LONG_PREFIXES):
            value = a.split("=", 1)[1]
            explicit_pattern = explicit_pattern or (value if a.startswith("--regexp=") else "")
            continue
        m = GREP_PATTERN_ATTACHED_RE.match(a)
        if m:
            explicit_pattern = explicit_pattern or (m.group(1) if a.startswith("-e") else "")
            continue
        if pattern_flag_re.match(a):
            note_recursive(a)
            explicit_pattern = explicit_pattern or ""
            skip = True
            capture_pattern = a.endswith("e")
            continue
        if arg_flag_re.match(a):
            note_recursive(a)
            skip = True
            continue
        if a.startswith("-") and a != "-":
            note_recursive(a)
            continue
        positional.append(a)
    if explicit_pattern is None:
        if not positional:
            return None, [], recursive
        return positional[0], positional[1:], recursive
    return explicit_pattern, positional, recursive


def _classify_grep(word: str, args: list[str], project_root: str | None, cwd: str | None) -> str | None:
    """The search pattern when a `grep`/`rg` invocation searches code under
    the project, else None ("" when the pattern comes from a file).
    """
    pattern, paths, recursive = _parse_grep(word, args)
    if pattern is None:
        return None
    if paths:
        hit = any(_code_path_arg(p, project_root, cwd) for p in paths)
    else:
        hit = recursive and _code_path_arg(".", project_root, cwd)
    return pattern if hit else None


def _classify_find(args: list[str], project_root: str | None, cwd: str | None) -> tuple[bool, str | None]:
    """(is_retrieval, pattern) for a `find` invocation. Retrieval needs a
    name/path predicate, a root under the project, no mutating action
    (`-delete`, `-fprint`, ...), and any `-exec`/`-execdir`/`-ok`/`-okdir`
    utility drawn from `FIND_READONLY_EXEC_UTILS`; a find that acts on its
    matches is housekeeping, not retrieval. When the exec utility is a
    grep, its pattern is returned so the find is accounted like a grep.
    """
    if not any(a in FIND_NAME_PREDICATES for a in args):
        return False, None
    if any(a in FIND_MUTATING_ACTIONS for a in args):
        return False, None
    pattern: str | None = None
    for i, a in enumerate(args):
        if a not in FIND_EXEC_ACTIONS:
            continue
        util = os.path.basename(args[i + 1]) if i + 1 < len(args) else ""
        if util not in FIND_READONLY_EXEC_UTILS:
            return False, None
        if util in NATIVE_GREP_WORDS and pattern is None:
            exec_args: list[str] = []
            for tok in args[i + 2:]:
                if tok in FIND_EXEC_TERMINATORS:
                    break
                exec_args.append(tok)
            pattern = _parse_grep(util, exec_args)[0]
    roots: list[str] = []
    for a in args:
        if a.startswith("-") or a in ("(", "!"):
            break
        roots.append(a)
    if not roots:
        roots = ["."]
    if not any(_code_path_arg(r, project_root, cwd) for r in roots):
        return False, None
    return True, pattern


def _is_pathspec(arg: str) -> bool:
    """A `git log` positional that names files rather than a revision.
    Refs are rejected first: `origin/master`, `refs/...`, `a..b`, `HEAD~2`,
    `HEAD^`, `@{u}`.
    """
    if GIT_REF_RE.search(arg):
        return False
    return "/" in arg or bool(SOURCE_EXT_RE.search(arg))


def _classify_git(args: list[str], project_root: str | None, cwd: str | None) -> tuple[str | None, str | None]:
    """(label, pattern) for a `git` invocation: `git grep` (always, with its
    pattern parsed like grep), `git blame` (always), `git log` with a
    pathspec / `--` / `-p` / `-S` / `-G`; (None, None) otherwise.
    """
    j = 0
    while j < len(args) and args[j].startswith("-"):
        j += 2 if args[j] in ("-C", "-c") else 1
    if j >= len(args):
        return None, None
    sub = args[j]
    rest = args[j + 1:]
    if sub == "grep":
        pattern, _paths, _recursive = _parse_grep("git grep", rest)
        return "git grep", pattern
    if sub == "blame":
        return "git blame", None
    if sub != "log":
        return None, None
    if "--" in rest:
        return "git log", None
    for a in rest:
        if a == "-p" or a.startswith("-S") or a.startswith("-G") or a in ("--patch", "--pickaxe-regex"):
            return "git log", None
        if not a.startswith("-") and _is_pathspec(a) and _code_path_arg(a, project_root, cwd):
            return "git log", None
    return None, None


def _resolve_cd(target: str, cwd: str | None) -> str | None:
    expanded = os.path.expanduser(target)
    if os.path.isabs(expanded):
        return os.path.normpath(expanded)
    if cwd and cwd != CWD_UNKNOWN:
        return os.path.normpath(os.path.join(cwd, expanded))
    return cwd


def _env_chdir(prefix: list[str], cwd: str | None) -> tuple[str | None, bool]:
    """(working directory, target_was_absolute) for one statement after an
    `env -C DIR` / `--chdir DIR` / `--chdir=DIR` in its wrapper prefix. Only
    an absolute target may re-root the statement, the same rule as `cd`.
    """
    for idx, tok in enumerate(prefix):
        target: str | None = None
        if tok in ("-C", "--chdir") and idx + 1 < len(prefix) and idx > 0 and "env" in prefix[:idx]:
            target = prefix[idx + 1]
        elif tok.startswith("--chdir=") and "env" in prefix[:idx]:
            target = tok.split("=", 1)[1]
        if target is not None:
            return _resolve_cd(target, cwd), os.path.isabs(os.path.expanduser(target))
    return cwd, False


def _skip_wrappers(tokens: list[str], j: int) -> int:
    """Index of the real command word after launcher / wrapper words, their
    flags and flag arguments, and leading env assignments.
    """
    while j < len(tokens):
        tok = tokens[j]
        if ENV_ASSIGNMENT_RE.match(tok):
            j += 1
            continue
        if tok not in BASH_WRAPPER_WORDS:
            return j
        wrapper = tok
        j += 1
        flag_args = WRAPPER_FLAG_ARGS.get(wrapper, set())
        while j < len(tokens) and tokens[j].startswith("-") and tokens[j] != "-":
            j += 2 if tokens[j] in flag_args else 1
        if wrapper == "timeout" and j < len(tokens) and TIMEOUT_DURATION_RE.match(tokens[j]):
            j += 1
    return j


def _command_args(tokens: list[str], start: int) -> list[str]:
    """Arguments of the command at `start`, up to the next pipe or the
    closing `)` of a `$(...)` substitution (the token carrying `)` is kept
    with the paren stripped).
    """
    out: list[str] = []
    skip_next = False
    for tok in tokens[start:]:
        if skip_next:
            skip_next = False
            continue
        if tok in ("|", "&"):
            break
        if REDIRECT_BARE_RE.match(tok):
            skip_next = True
            continue
        if REDIRECT_ATTACHED_RE.match(tok):
            continue
        if tok.endswith(")") and not tok.startswith("("):
            stripped = tok.rstrip(")")
            if stripped:
                out.append(stripped)
            break
        out.append(tok)
    return out


def shell_retrieval(command: Any, project_root: str | None = None) -> tuple[str | None, str | None]:
    """(label, pattern) when a Bash command searches the code base under
    `project_root`, else (None, None). See `shell_retrieval_detail`.
    """
    label, pattern, _root = shell_retrieval_detail(command, project_root)
    return label, pattern


def shell_retrieval_detail(
    command: Any, project_root: str | None = None, *, root_follows_cd: bool = False
) -> tuple[str | None, str | None, str | None]:
    """(label, pattern, root) when a Bash command searches the code base,
    else (None, None, None). The label is `grep`, `rg`, `find`, `git log`,
    `git blame`, or `git grep`; the pattern is the grep/rg search pattern
    when known; `root` is the root the winning statement was judged
    against. With `root_follows_cd` (the §2.3 session-scoped rule) an
    absolute `cd` inside the command re-roots the statements after it,
    statement by statement; without it the root is fixed for the command.

    Only the first command of each pipeline is a candidate (a `grep` that
    consumes another command's output is a filter, not a search), plus a
    `grep|rg|find` that follows `xargs` or opens a `$(...)` substitution.
    Heredoc bodies are skipped. `cd` is tracked across statements so
    relative arguments resolve against the running working directory
    (initially the project root). `grep`/`rg` count when a path argument
    passes the project-root guard (or, with no path, when they recurse the
    working directory and it is under the project); `find` needs a
    name/path predicate and no mutating action (`-delete`, `-fprint*`,
    `-fls`, or an `-exec`/`-ok` family action whose utility is not one of
    the read-only `FIND_READONLY_EXEC_UTILS`); `git log` needs a pathspec
    or `-p`/`-S`/`-G`;
    `git blame`/`git grep` always count. Calls with no project root and
    absolute paths outside it are scored conservatively (not retrieval).
    """
    if not isinstance(command, str) or not command.strip():
        return None, None, None
    root: str | None = os.path.normpath(os.path.expanduser(project_root)) if project_root else None
    cwd: str | None = root
    cwd_lost = False
    for statement in BASH_STATEMENT_SPLIT_RE.split(_strip_heredocs(command)):
        tokens = _tokenize(statement)
        if not tokens:
            continue
        if tokens[0].startswith("("):
            # Subshell nesting is not modeled, so subsequent relative paths are unknown.
            cwd_lost = True
            tokens[0] = tokens[0][1:]
            if not tokens[0]:
                tokens.pop(0)
                if not tokens:
                    continue
        if cwd_lost:
            cwd = CWD_UNKNOWN
        head = _skip_wrappers(tokens, 0)
        stmt_cwd, env_chdir_absolute = _env_chdir(tokens[:head], cwd)
        if head < len(tokens) and tokens[head] in ("pushd", "popd"):
            cwd_lost = True
            cwd = CWD_UNKNOWN
            continue
        if head < len(tokens) and tokens[head] == "cd":
            if cwd_lost:
                continue
            target = tokens[head + 1] if head + 1 < len(tokens) and tokens[head + 1] != "|" else "~"
            if target == "-":
                continue
            cwd = _resolve_cd(target, cwd)
            if root_follows_cd and cwd and cwd != CWD_UNKNOWN and os.path.isabs(os.path.expanduser(target)):
                root = cwd
            continue
        cwd_for_statement = stmt_cwd
        stmt_root = root
        if root_follows_cd and env_chdir_absolute and stmt_cwd and stmt_cwd != CWD_UNKNOWN:
            stmt_root = stmt_cwd
        for i, raw in enumerate(tokens):
            substitution = raw.startswith("$(")
            tok = raw[2:] if substitution else raw
            if not tok:
                continue
            after_xargs = False
            k = i - 1
            while k >= 0 and tokens[k].startswith("-"):
                k -= 1
            if k >= 0 and os.path.basename(tokens[k]) == "xargs":
                after_xargs = True
            if not (i == 0 or substitution or after_xargs):
                continue
            j = i if substitution else _skip_wrappers(tokens, i)
            if j >= len(tokens):
                break
            if j != i:
                tok = tokens[j]
                if tok == "|":
                    continue
            word = os.path.basename(tok)
            args = _command_args(tokens, j + 1)
            if after_xargs and word in NATIVE_GREP_WORDS | {"find"}:
                # xargs receives paths from stdin; judge the producer's paths.
                pipes = [idx for idx in range(i) if tokens[idx] == "|"]
                if not pipes:
                    continue
                stage = tokens[(pipes[-2] + 1 if len(pipes) > 1 else 0):pipes[-1]]
                stage = stage[_skip_wrappers(stage, 0):]
                producer_args = [a for a in stage[1:] if not a.startswith("-")]
                if any(_code_path_arg(a, stmt_root, cwd_for_statement) for a in producer_args):
                    # The producer already passed the path guard.
                    pattern = _parse_grep(word, args)[0] if word in NATIVE_GREP_WORDS else None
                    return word, pattern, stmt_root
                continue
            if word in NATIVE_GREP_WORDS:
                pattern = _classify_grep(word, args, stmt_root, cwd_for_statement)
                if pattern is not None:
                    return word, pattern, stmt_root
            elif word == "find":
                is_find, find_pattern = _classify_find(args, stmt_root, cwd_for_statement)
                if is_find:
                    return "find", find_pattern, stmt_root
            elif word == "git":
                label, pattern = _classify_git(args, stmt_root, cwd_for_statement)
                if label:
                    return label, pattern, stmt_root
    return None, None, None


def last_absolute_cd(command: Any) -> str | None:
    """The working directory a Bash command leaves behind for the next
    call, when it can be followed: the last absolute `cd`/`pushd` target
    (relative targets and `popd` cannot be followed and yield None).
    Claude Code's Bash tool persists the working directory across calls in
    a main session (not in subagent transcripts, where each call starts
    fresh). A `cd` that failed at runtime (missing directory) is still
    adopted: the transcript does not carry the exit status.
    """
    if not isinstance(command, str) or not command.strip():
        return None
    result: str | None = None
    for statement in BASH_STATEMENT_SPLIT_RE.split(_strip_heredocs(command)):
        tokens = _tokenize(statement)
        if not tokens or tokens[0].startswith("("):
            continue
        head = _skip_wrappers(tokens, 0)
        if head >= len(tokens) or tokens[head] not in ("cd", "pushd", "popd"):
            continue
        if tokens[head] == "popd" or head + 1 >= len(tokens) or tokens[head + 1] in ("|", "-"):
            result = None
            continue
        expanded = os.path.expanduser(tokens[head + 1])
        result = os.path.normpath(expanded) if os.path.isabs(expanded) else None
    return result


def is_onboarded_root(root: str | None, session_roots: set[str], cache: dict[str, bool]) -> bool:
    """True when `root` is inside an onboarded project: one of the
    absolute `session_roots` (the `project` values of the session's
    CodeSage calls) or a directory holding `.codesage/index.db`, checking
    the path and every parent, so a grep from `<proj>/crates/graph` or a
    worktree under `<proj>/.claude/worktrees/*` still counts. `cache` is
    keyed on the queried path and lasts one run.
    """
    if not root or root == CWD_UNKNOWN:
        return False
    # abspath would resolve a relative root against the analyzer's own directory.
    expanded = os.path.expanduser(root)
    if not os.path.isabs(expanded):
        return False
    probe = Path(os.path.normpath(expanded))
    candidates = [probe, *probe.parents]
    if any(str(c) in session_roots for c in candidates):
        return True
    key = str(probe)
    if key not in cache:
        cache[key] = any((c / ".codesage" / "index.db").is_file() for c in candidates)
    return cache[key]


def bash_native_retrieval(command: Any, project_root: str | None = None) -> str | None:
    """Label when a Bash command searches the code base, else None. See
    `shell_retrieval`. Known gap, left as is: a wrapper inside a command
    substitution (`$(rtk proxy grep ...)`) is not recognised; `$(grep ...)` is.
    """
    return shell_retrieval(command, project_root)[0]


TERMINALITY_COUNT_KEYS = (
    "calls",
    "terminal",
    "non_terminal",
    "chained",
    "policy_read",
    "follow_through_read",
    "unscoped",
)


def _new_terminality_bucket() -> dict[str, Any]:
    bucket: dict[str, Any] = {key: 0 for key in TERMINALITY_COUNT_KEYS}
    bucket["first_followups"] = {}
    bucket["top_followups"] = []
    bucket["terminal_rate"] = None
    bucket["terminal_rate_strict"] = None
    return bucket


def _score_window(
    calls: list[dict[str, Any]],
    result_paths: set[str],
    project_root: str | None = None,
) -> tuple[str, str | None, int, int, str | None]:
    """Walk the tool calls after one codesage call. Returns
    (outcome, first_non_terminal_followup, policy_reads, follow_through_reads, evidence)
    where outcome is `terminal`, `non_terminal`, or `chained` (another
    codesage call arrived before any native retrieval) and evidence is the
    Bash command / Read path / Grep pattern that decided a non-terminal call.

    A Read is excused when the result named its path (follow-through) or
    when an Edit/Write of the same path follows inside the window (harness
    read-before-edit policy); either way it consumes a window slot but is
    not native retrieval. The policy-read exclusion is deliberately broader
    than ripwire §9, which excuses only the Read of an EDIT verb's own
    target file: here any Read followed by an Edit/Write of the same path
    is excused, because CodeSage has no edit verbs and every edit the agent
    makes after a query goes through the harness's read-before-edit rule.
    """
    policy = 0
    follow = 0
    for k, call in enumerate(calls):
        name = call.get("tool") or ""
        inp = call.get("input") or {}
        if not isinstance(inp, dict):
            inp = {}
        if name.startswith(TOOL_PREFIX):
            return "chained", None, policy, follow, None
        if name in ("Grep", "Glob"):
            return "non_terminal", name, policy, follow, str(inp.get("pattern") or "")
        if name == "Bash":
            command = inp.get("command")
            label = bash_native_retrieval(command, project_root)
            if label:
                return "non_terminal", f"Bash({label})", policy, follow, str(command)
            continue
        if name == "Read":
            path = inp.get("file_path")
            if not isinstance(path, str):
                continue
            if result_paths and read_hits_result(path, result_paths, project_root):
                follow += 1
                continue
            norm = _norm_path(path)
            edited_later = False
            for later in calls[k + 1:]:
                if (later.get("tool") or "") not in EDIT_TOOLS:
                    continue
                later_inp = later.get("input") or {}
                later_path = later_inp.get("file_path") if isinstance(later_inp, dict) else None
                if isinstance(later_path, str) and _norm_path(later_path) == norm:
                    edited_later = True
                    break
            if edited_later:
                policy += 1
                continue
            return "non_terminal", "Read", policy, follow, path
    return "terminal", None, policy, follow, None


def _finalize_terminality_bucket(bucket: dict[str, Any]) -> None:
    ranked = sorted(bucket["first_followups"].items(), key=lambda kv: (-kv[1], kv[0]))
    bucket["top_followups"] = ranked[:5]
    calls = bucket["calls"]
    strict = bucket["terminal"] + bucket["non_terminal"]
    bucket["terminal_rate"] = (bucket["terminal"] / calls) if calls else None
    bucket["terminal_rate_strict"] = (bucket["terminal"] / strict) if strict else None


def _window_calls(events: list[dict[str, Any]], start: int, window: int) -> list[dict[str, Any]]:
    """Tool calls after `start`, capped at `window`, cut at the next user
    turn (a new question re-baselines the agent) and at the next codesage
    call (which is appended so the scorer can report `chained`).
    """
    calls: list[dict[str, Any]] = []
    for e in events[start + 1:]:
        kind = e.get("kind")
        if kind == "user":
            # Harness injections also use role=user and must not cut the window.
            if is_real_user_question(e.get("text") or ""):
                break
            continue
        if kind != "tool_use":
            continue
        if (e.get("tool") or "") in NON_SLOT_TOOLS:
            continue
        calls.append(e)
        if (e.get("tool") or "").startswith(TOOL_PREFIX) or len(calls) >= window:
            break
    return calls


def terminality(
    events: list[dict[str, Any]], window: int = DEFAULT_TERMINALITY_WINDOW
) -> dict[str, Any]:
    """Per-codesage-tool terminality for one transcript's events.

    A codesage call is terminal when none of the next `window` tool calls
    (before the next user turn) is native retrieval: Grep, Glob, a Read of
    a path the result did not name (and that is not edited within the
    window), or a Bash command that searches the code base (see
    `bash_native_retrieval`). Non-terminal calls are also listed under
    `evidence` with the follow-up that decided them.
    """
    results_by_id: dict[Any, dict[str, Any]] = {}
    for e in events:
        if e.get("kind") == "tool_result" and e.get("id") is not None:
            results_by_id.setdefault(e["id"], e)

    tools: dict[str, dict[str, Any]] = {}
    overall = _new_terminality_bucket()
    evidence: list[dict[str, Any]] = []
    for i, ev in enumerate(events):
        if ev.get("kind") != "tool_use":
            continue
        tool = ev.get("tool") or ""
        if not tool.startswith(TOOL_PREFIX):
            continue
        suffix = tool[len(TOOL_PREFIX):]
        inp = ev.get("input") or {}
        project_root = inp.get("project") if isinstance(inp, dict) else None
        if not isinstance(project_root, str) or not project_root:
            project_root = None
        res = results_by_id.get(ev.get("id")) if ev.get("id") is not None else None
        result_paths = extract_result_paths((res or {}).get("text") or "")
        outcome, first, policy, follow, decided_by = _score_window(
            _window_calls(events, i, window), result_paths, project_root
        )
        for bucket in (tools.setdefault(suffix, _new_terminality_bucket()), overall):
            bucket["calls"] += 1
            bucket["policy_read"] += policy
            bucket["follow_through_read"] += follow
            bucket["unscoped"] += 0 if project_root else 1
            bucket[outcome] += 1
            if outcome == "non_terminal":
                bucket["first_followups"][first] = bucket["first_followups"].get(first, 0) + 1
        if outcome == "non_terminal":
            evidence.append({"tool": suffix, "followup": first, "evidence": decided_by})
    for bucket in tools.values():
        _finalize_terminality_bucket(bucket)
    _finalize_terminality_bucket(overall)
    return {"window": window, "tools": tools, "all": overall, "evidence": evidence}


def merge_terminality(parts: list[dict[str, Any]], window: int) -> dict[str, Any]:
    tools: dict[str, dict[str, Any]] = {}
    overall = _new_terminality_bucket()
    evidence: list[dict[str, Any]] = []

    def fold(dst: dict[str, Any], src: dict[str, Any]) -> None:
        for key in TERMINALITY_COUNT_KEYS:
            dst[key] += src.get(key, 0)
        for name, cnt in (src.get("first_followups") or {}).items():
            dst["first_followups"][name] = dst["first_followups"].get(name, 0) + cnt

    for part in parts:
        for suffix, bucket in (part.get("tools") or {}).items():
            fold(tools.setdefault(suffix, _new_terminality_bucket()), bucket)
        fold(overall, part.get("all") or {})
        evidence.extend(part.get("evidence") or [])
    for bucket in tools.values():
        _finalize_terminality_bucket(bucket)
    _finalize_terminality_bucket(overall)
    return {"window": window, "tools": tools, "all": overall, "evidence": evidence}




def aggregate(
    events_per_transcript: list[list[dict[str, Any]]],
    *,
    terminality_window: int = DEFAULT_TERMINALITY_WINDOW,
    subagent_flags: list[bool] | None = None,
) -> dict[str, Any]:
    """`subagent_flags[i]` marks `events_per_transcript[i]` as a subagent
    transcript; the Bash working directory is carried across calls only in
    main sessions.
    """
    quality: dict[str, dict[str, int]] = defaultdict(lambda: {"ok": 0, "empty": 0, "error": 0})

    grep_tool_calls = 0
    grep_tool_calls_all = 0
    # Only onboarded roots offered a CodeSage alternative; retain all-root totals too.
    shell_grep_calls = 0
    shell_grep_calls_all = 0
    identifier_grep_in_codesage_sessions_all = 0
    # Unknown file-supplied patterns stay outside the identifier denominator.
    shell_grep_unparsed = 0
    followup_grep_events = 0
    grep_identifier_shaped = 0
    grep_multi_ident = 0  # pipe-joined or whitespace-split → multiple codesage calls would be needed
    followup_grep_after_codesage = 0
    codesage_results_total = 0

    codesage_retrieval_calls: dict[str, int] = defaultdict(int)
    sessions_with_codesage_available = 0
    sessions_total = 0
    identifier_grep_in_codesage_sessions = 0
    codesage_retrieval_in_codesage_sessions = 0

    reads_after_codesage: list[int] = []

    shape_counts: dict[str, dict[str, int]] = defaultdict(
        lambda: {
            "questions": 0,
            "first_codesage_retrieval": 0,
            "first_codesage_search": 0,  # subset: specifically `search`
            "first_grep": 0,
            "first_read": 0,
            "first_glob": 0,
            "first_other_or_none": 0,
        }
    )

    onboarded_cache: dict[str, bool] = {}

    # Reading a known path is a retrieval choice, even without a search.
    def first_retrieval_tool(
        events: list[dict[str, Any]], start: int, project_root: str | None
    ) -> str | None:
        """From `start`, scan forward until the first *decision* tool_use
        OR the next user message, whichever comes first. Bookkeeping tools
        (`NON_SLOT_TOOLS`) are skipped; a `Bash` call counts only when it
        is a shell grep (grep family, `git grep` → `Grep`) or `find`
        (→ `Glob`; → `Grep` when it runs `-exec grep` and the pattern was
        recovered), the same label set the §2.3 counters use (the root
        gate and the `-f` exclusion are not applied here); other shell
        retrieval (`git log`, `git blame`) is not a decision and is skipped.
        Returns the tool name or None.
        """
        n = len(events)
        j = start
        while j < n:
            e = events[j]
            if e.get("kind") == "user":
                return None
            if e.get("kind") == "tool_use":
                tool = e.get("tool") or ""
                if tool in NON_SLOT_TOOLS:
                    j += 1
                    continue
                if tool == "Bash":
                    label, pattern = shell_retrieval((e.get("input") or {}).get("command"), project_root)
                    if label in NATIVE_GREP_WORDS or label == "git grep" or (label == "find" and pattern is not None):
                        return "Grep"
                    if label == "find":
                        return "Glob"
                    j += 1
                    continue
                return tool
            j += 1
        return None

    def account_grep_pattern(
        pattern: str, session_had_codesage: bool, recent_subjects: list[dict[str, Any]]
    ) -> None:
        """Identifier-shape and follow-up accounting shared by the Grep tool
        and shell greps. A codesage result is counted as "followed by a
        grep" at most once; every (grep, matched result) pair is counted in
        `followup_grep_events`, so the per-result mean is at least 1.
        """
        nonlocal grep_identifier_shaped, grep_multi_ident
        nonlocal identifier_grep_in_codesage_sessions, followup_grep_after_codesage, followup_grep_events
        if identifier_shaped_grep(pattern):
            grep_identifier_shaped += 1
            if session_had_codesage:
                identifier_grep_in_codesage_sessions += 1
            if "|" in pattern or len(pattern.split()) > 1:
                grep_multi_ident += 1
        grep_idents = extract_grep_identifiers(pattern)
        if not grep_idents:
            return
        for entry in recent_subjects:
            if grep_idents & entry["subjects"]:
                followup_grep_events += 1
                if not entry["hit"]:
                    entry["hit"] = True
                    followup_grep_after_codesage += 1

    def session_project_roots(events: list[dict[str, Any]]) -> list[str]:
        """Absolute `project` arguments of the session's codesage calls, in
        order of first appearance (tilde expanded; a relative value such as
        `codesage` cannot be located and is skipped). The first is the
        default root shell paths are judged against; all of them are
        onboarded by construction.
        """
        roots: list[str] = []
        for ev in events:
            if ev.get("kind") == "tool_use" and (ev.get("tool") or "").startswith(TOOL_PREFIX):
                inp = ev.get("input") or {}
                root = inp.get("project") if isinstance(inp, dict) else None
                if not isinstance(root, str) or not root:
                    continue
                expanded = os.path.normpath(os.path.expanduser(root))
                if os.path.isabs(expanded) and expanded not in roots:
                    roots.append(expanded)
        return roots

    def shape_walk(events: list[dict[str, Any]], session_had_codesage: bool, project_root: str | None) -> None:
        """For each *real* user question (excluding pasted command bodies
        and system notifications), classify by shape and record the
        first tool the agent reached for in response. Only sessions
        where codesage was available are counted.

        The denominator the rendered output cares about is
        `retrieval_decisions` per shape — questions that resulted in a
        retrieval-class first action. That excludes turns where the
        agent immediately wrote code, ran a non-search Bash command, or
        reasoned without a tool — none of which are retrieval choices. A
        Bash command that is a shell grep counts as `first=Grep`.
        """
        if not session_had_codesage:
            return
        n = len(events)
        i = 0
        while i < n:
            ev = events[i]
            if ev.get("kind") != "user":
                i += 1
                continue
            text = ev.get("text") or ""
            if not is_real_user_question(text):
                i += 1
                continue
            shape = classify_user_question(text)
            shape_counts[shape]["questions"] += 1
            first_tool = first_retrieval_tool(events, i + 1, project_root)
            bucket = shape_counts[shape]
            if first_tool is None:
                bucket["first_other_or_none"] += 1
            elif first_tool.startswith(TOOL_PREFIX):
                suffix = first_tool[len(TOOL_PREFIX):]
                if suffix in RETRIEVAL_CODESAGE_TOOLS:
                    bucket["first_codesage_retrieval"] += 1
                    if suffix == "search":
                        bucket["first_codesage_search"] += 1
                else:
                    bucket["first_other_or_none"] += 1
            elif first_tool == "Grep":
                bucket["first_grep"] += 1
            elif first_tool == "Read":
                bucket["first_read"] += 1
            elif first_tool == "Glob":
                bucket["first_glob"] += 1
            else:
                bucket["first_other_or_none"] += 1
            i += 1

    for idx_transcript, events in enumerate(events_per_transcript):
        sessions_total += 1
        is_subagent = bool(subagent_flags[idx_transcript]) if subagent_flags else False
        # A call proves availability; its absence cannot prove CodeSage was unavailable.
        had_codesage = any(
            ev["kind"] == "tool_use" and (ev.get("tool") or "").startswith(TOOL_PREFIX)
            for ev in events
        )
        if had_codesage:
            sessions_with_codesage_available += 1
        session_roots_list = session_project_roots(events)
        session_roots = set(session_roots_list)
        session_project = session_roots_list[0] if session_roots_list else None

        shape_walk(events, had_codesage, session_project)

        recent_codesage_subjects: list[dict[str, Any]] = []
        bash_cwd: str | None = None
        i = 0
        n = len(events)
        while i < n:
            e = events[i]
            if e["kind"] == "tool_use" and (e.get("tool") or "").startswith(TOOL_PREFIX):
                tool_suffix = (e.get("tool") or "")[len(TOOL_PREFIX):]
                if tool_suffix in RETRIEVAL_CODESAGE_TOOLS:
                    codesage_retrieval_calls[tool_suffix] += 1
                    if had_codesage:
                        codesage_retrieval_in_codesage_sessions += 1
                subj = extract_codesage_subject(e)
                if subj:
                    recent_codesage_subjects.append({"subjects": subj, "hit": False})
                    if len(recent_codesage_subjects) > 5:
                        recent_codesage_subjects.pop(0)
            elif e["kind"] == "tool_result" and (e.get("pair_tool") or "").startswith(TOOL_PREFIX):
                tool = e["pair_tool"][len(TOOL_PREFIX):]
                cat = classify_codesage_result(e.get("text") or "")
                quality[tool][cat] += 1
                codesage_results_total += 1
                subj = extract_codesage_subject(e)
                if subj:
                    if recent_codesage_subjects:
                        recent_codesage_subjects[-1]["subjects"] |= subj
                    else:
                        recent_codesage_subjects.append({"subjects": subj, "hit": False})
                reads = 0
                j = i + 1
                while j < n:
                    ev = events[j]
                    if ev["kind"] == "tool_use":
                        tname = ev.get("tool") or ""
                        if tname == "Read":
                            reads += 1
                            j += 1
                            continue
                        if tname == "Grep":
                            break
                        break
                    j += 1
                reads_after_codesage.append(reads)
            elif e["kind"] == "tool_use" and e.get("tool") == "Grep":
                # Relative or absent Grep paths inherit the session working tree.
                grep_input = e.get("input") or {}
                grep_path = grep_input.get("path") if isinstance(grep_input, dict) else None
                if isinstance(grep_path, str) and os.path.isabs(os.path.expanduser(grep_path)):
                    grep_root: str | None = os.path.expanduser(grep_path)
                else:
                    grep_root = bash_cwd or session_project
                grep_tool_calls_all += 1
                pattern = grep_input.get("pattern", "") if isinstance(grep_input, dict) else ""
                if isinstance(pattern, str):
                    if had_codesage and identifier_shaped_grep(pattern):
                        identifier_grep_in_codesage_sessions_all += 1
                    if is_onboarded_root(grep_root, session_roots, onboarded_cache):
                        grep_tool_calls += 1
                        account_grep_pattern(pattern, had_codesage, recent_codesage_subjects)
                elif is_onboarded_root(grep_root, session_roots, onboarded_cache):
                    grep_tool_calls += 1
            elif e["kind"] == "tool_use" and e.get("tool") == "Bash":
                # Shell greps belong in the same denominator as native Grep calls.
                command = (e.get("input") or {}).get("command")
                # An absolute cd re-roots only subsequent statements.
                label, pattern, stmt_root = shell_retrieval_detail(
                    command, bash_cwd or session_project, root_follows_cd=True
                )
                if label in NATIVE_GREP_WORDS or label == "git grep" or (label == "find" and pattern is not None):
                    if not pattern:
                        shell_grep_unparsed += 1
                    else:
                        shell_grep_calls_all += 1
                        identifier_shaped = identifier_shaped_grep(pattern)
                        if had_codesage and identifier_shaped:
                            identifier_grep_in_codesage_sessions_all += 1
                        if is_onboarded_root(stmt_root, session_roots, onboarded_cache):
                            shell_grep_calls += 1
                            account_grep_pattern(pattern, had_codesage, recent_codesage_subjects)
                if not is_subagent:
                    bash_cwd = last_absolute_cd(command) or bash_cwd
            i += 1

    return {
        "quality": quality,
        "grep_calls": grep_tool_calls + shell_grep_calls,
        "grep_tool_calls": grep_tool_calls,
        "grep_tool_calls_all": grep_tool_calls_all,
        "shell_grep_calls": shell_grep_calls,
        "shell_grep_calls_all": shell_grep_calls_all,
        "shell_grep_unparsed": shell_grep_unparsed,
        "identifier_grep_in_codesage_sessions_all": identifier_grep_in_codesage_sessions_all,
        "grep_identifier_shaped": grep_identifier_shaped,
        "grep_multi_ident": grep_multi_ident,
        "followup_grep_after_codesage": followup_grep_after_codesage,
        "followup_grep_events": followup_grep_events,
        "codesage_results_total": codesage_results_total,
        "reads_after_codesage": reads_after_codesage,
        "codesage_retrieval_calls": dict(codesage_retrieval_calls),
        "sessions_total": sessions_total,
        "sessions_with_codesage_available": sessions_with_codesage_available,
        "identifier_grep_in_codesage_sessions": identifier_grep_in_codesage_sessions,
        "codesage_retrieval_in_codesage_sessions": codesage_retrieval_in_codesage_sessions,
        "question_shape_counts": {k: dict(v) for k, v in shape_counts.items()},
        "terminality": merge_terminality(
            [terminality(events, terminality_window) for events in events_per_transcript],
            terminality_window,
        ),
    }




def pct(n: int, d: int) -> str:
    if d == 0:
        return "n/a"
    return f"{100.0 * n / d:.1f}%"


def percentiles(xs: list[int]) -> tuple[int, int, int, int]:
    if not xs:
        return 0, 0, 0, 0
    s = sorted(xs)
    return (
        s[len(s) // 2],
        s[int(len(s) * 0.95)] if len(s) >= 20 else s[-1],
        s[int(len(s) * 0.99)] if len(s) >= 100 else s[-1],
        s[-1],
    )


def render(
    agg: dict[str, Any],
    *,
    window_days: int,
    transcripts: int,
    now: str,
    subagent_transcripts: int = 0,
) -> str:
    out: list[str] = []
    q: dict[str, dict[str, int]] = agg["quality"]

    out.append("# CodeSage MCP quality + utility analysis")
    out.append("")
    out.append(f"**Window**: last {window_days} days  ")
    out.append(
        f"**Transcripts scanned**: {transcripts} "
        f"({transcripts - subagent_transcripts} session, {subagent_transcripts} subagent)  "
    )
    out.append(f"**Run at**: {now}  ")
    out.append(f"**CodeSage tool_results analyzed**: {agg['codesage_results_total']}  ")
    shell_all = agg.get("shell_grep_calls_all", agg.get("shell_grep_calls", 0))
    tool_all = agg.get("grep_tool_calls_all", agg.get("grep_tool_calls", 0))
    out.append(
        f"**Grep tool + shell grep calls observed (onboarded roots)**: {agg['grep_calls']} "
        f"(Grep tool {agg.get('grep_tool_calls', agg['grep_calls'])}, "
        f"shell greps {agg.get('shell_grep_calls', 0)}); "
        f"all roots: {tool_all + shell_all} (Grep tool {tool_all}, shell greps {shell_all})"
    )
    out.append("")

    out.append("## Quality: empty / error / ok by tool")
    out.append("")
    if not q:
        out.append("_No codesage tool_results in the window._")
    else:
        out.append("| tool | calls | ok | empty | error | ok rate |")
        out.append("|---|---:|---:|---:|---:|---:|")
        tools = sorted(q.keys(), key=lambda t: sum(q[t].values()), reverse=True)
        total_ok = total_empty = total_error = total = 0
        for t in tools:
            row = q[t]
            calls = row["ok"] + row["empty"] + row["error"]
            total += calls
            total_ok += row["ok"]
            total_empty += row["empty"]
            total_error += row["error"]
            out.append(
                f"| `{t}` | {calls} | {row['ok']} | {row['empty']} | {row['error']} | "
                f"{pct(row['ok'], calls)} |"
            )
        out.append(
            f"| **all** | {total} | {total_ok} | {total_empty} | {total_error} | "
            f"{pct(total_ok, total)} |"
        )
    out.append("")

    out.append("## Utility: Grep tool + shell greps that CodeSage would have answered")
    out.append("")
    gc = agg["grep_calls"]
    gi = agg["grep_identifier_shaped"]
    gm = agg["grep_multi_ident"]
    shell_gated = agg.get("shell_grep_calls", 0)
    shell_all = agg.get("shell_grep_calls_all", shell_gated)
    tool_gated = agg.get("grep_tool_calls", gc)
    tool_all = agg.get("grep_tool_calls_all", tool_gated)
    out.append(
        f"- **Grep calls in window**: {gc} (Grep tool {tool_gated}, "
        f"shell `grep`/`rg`/`git grep` at a pipeline head or `find -exec grep` {shell_gated}; "
        f"both under an onboarded root). "
        f"Excluded from the denominator below: {tool_all - tool_gated} Grep tool calls and "
        f"{shell_all - shell_gated} shell greps under non-onboarded roots, "
        f"{agg.get('shell_grep_unparsed', 0)} shell greps with no recoverable pattern "
        f"(`-f FILE` / `--file`)"
    )
    out.append(
        f"- **Identifier-shaped** (CodeSage `find_symbol` / `find_references` territory): "
        f"{gi} ({pct(gi, gc)})"
    )
    out.append(
        f"- **Multi-identifier** (pipe-joined or space-separated — would take N CodeSage calls "
        f"but one semantic `search`): {gm} ({pct(gm, gc)})"
    )
    out.append("")
    out.append(
        "Interpretation: the identifier-shaped rate is the closest proxy we have for "
        "\"agent left money on the table.\" A pattern like `'fn foo|bar::new|Baz'` "
        "is a CodeSage-shaped query. A pattern with `.*`, character classes, or anchors "
        "is not."
    )
    out.append("")

    out.append("## Tool-selection rate (recommendations §2.3)")
    out.append("")
    retr = agg["codesage_retrieval_calls"]
    retr_total = sum(retr.values())
    retr_in_avail = agg["codesage_retrieval_in_codesage_sessions"]
    id_grep_in_avail = agg["identifier_grep_in_codesage_sessions"]
    avail_decisions = retr_in_avail + id_grep_in_avail
    out.append(
        f"- **Sessions analyzed**: {agg['sessions_total']} total, "
        f"{agg['sessions_with_codesage_available']} had codesage MCP tools actually called"
    )
    out.append(
        f"- **CodeSage retrieval tool calls** (search, find_symbol, find_references, "
        f"impact_analysis, export_context, list_dependencies): {retr_total}"
    )
    if retr:
        parts = ", ".join(f"`{k}`={v}" for k, v in sorted(retr.items(), key=lambda kv: -kv[1]))
        out.append(f"  - breakdown: {parts}")
    else:
        out.append("  - breakdown: none (zero retrieval-class codesage calls in window)")
    out.append(
        f"- **Identifier-shaped Grep tool + shell grep calls in same sessions**: {id_grep_in_avail}"
    )
    if avail_decisions > 0:
        rate = 100.0 * retr_in_avail / avail_decisions
        out.append(
            f"- **Tool-selection rate** (retrieval-class picks that went to CodeSage "
            f"over Grep, sessions where codesage was available, shell greps under "
            f"onboarded roots only): "
            f"**{rate:.1f}%** ({retr_in_avail} CodeSage / {avail_decisions} total "
            f"retrieval-shape decisions)"
        )
    else:
        out.append("- **Tool-selection rate**: n/a (no retrieval-shape decisions in window)")
    id_grep_all = agg.get("identifier_grep_in_codesage_sessions_all", id_grep_in_avail)
    all_decisions = retr_in_avail + id_grep_all
    out.append(
        f"  - including non-onboarded roots: rate {pct(retr_in_avail, all_decisions)} "
        f"({id_grep_all} identifier-shaped greps in CodeSage sessions)"
    )
    out.append("")
    out.append(
        "Interpretation: in sessions where the agent *could have* used CodeSage, "
        "retrieval-class picks went to either a CodeSage MCP tool or to an "
        "identifier-shaped Grep pattern that CodeSage would have answered. Shell "
        "greps are session-scoped: each is judged against the working directory the "
        "Bash tool carried into it (the last absolute `cd`/`pushd` in the session) or, "
        "failing that, the `project` of the session's first CodeSage call; greps whose "
        "pattern could not be recovered are excluded. A shell grep counts only when "
        "that root is onboarded — a `project` seen in any CodeSage call of the session, "
        "or a directory holding `.codesage/index.db` — because elsewhere CodeSage was "
        "not an option; the secondary line keeps the ungated figure. High "
        "rate (>70%) means the agent reaches for CodeSage on merit. Near-zero "
        "rate means the tool affordances (descriptions, CLAUDE.md directives) "
        "are not winning — escalation path is either stronger prompts or "
        "hook-based steering à la the landscape's LSP-enforcement-kit."
    )
    out.append("")

    out.append("## Utility: follow-up Grep after a CodeSage result")
    out.append("")
    fg = agg["followup_grep_after_codesage"]
    fge = agg.get("followup_grep_events", fg)
    cr = agg["codesage_results_total"]
    out.append(
        f"- **Codesage results followed by at least one Grep on a mentioned identifier "
        f"within 5 tool_uses**: {fg} ({pct(fg, cr)})"
    )
    out.append(
        f"- **Follow-up greps per followed result** (mean): "
        f"{(fge / fg):.2f} ({fge} greps)" if fg else "- **Follow-up greps per followed result**: n/a"
    )
    out.append("")
    out.append(
        "Low rate (under ~5%) means agents trust the result. High rate means "
        "either the result was incomplete, the agent distrusts it, or the schema "
        "is hard to navigate. At baseline (no codesage calls at all) this number "
        "is trivially zero, so read it alongside the overall result count."
    )
    out.append("")

    out.append("## Utility: Reads between a CodeSage result and the next action")
    out.append("")
    reads = agg["reads_after_codesage"]
    if reads:
        p50, p95, p99, mx = percentiles(reads)
        total_reads = sum(reads)
        out.append(
            f"- **Read-events between codesage result and next non-(Read|Grep) tool**: "
            f"p50={p50}, p95={p95}, p99={p99}, max={mx}, total_reads={total_reads}"
        )
        zero_reads = sum(1 for r in reads if r == 0)
        out.append(
            f"- **Codesage results that led the agent straight to a semantic action "
            f"(zero Reads in between)**: {zero_reads} ({pct(zero_reads, len(reads))})"
        )
    else:
        out.append("_No codesage results observed._")
    out.append("")
    out.append(
        "Reads in the window after a codesage response indicate the agent had to "
        "fetch file contents anyway — codesage was a preamble, not a substitute. "
        "Zero-read outcomes mean codesage's structured response was self-contained."
    )
    out.append("")

    term = agg.get("terminality") or {}
    term_window = term.get("window", DEFAULT_TERMINALITY_WINDOW)
    out.append(
        f"## Terminality per CodeSage tool (native retrieval within {term_window} calls)"
    )
    out.append("")
    out.append(
        f"A CodeSage call is **terminal** when none of the next {term_window} tool calls "
        "(same transcript, before the next user turn) is native retrieval: `Grep`, "
        "`Glob`, a `Read` of a file the result did not name, or a `Bash` command that "
        "searches the code base — the first command of a pipeline (or one after "
        "`xargs` / inside `$(...)`) being `grep`/`rg` with a project path, `find` with "
        "a name/path predicate and no mutating action (`-delete`, `-fprint*`, `-fls`, or "
        "`-exec`/`-ok` running anything but a read-only utility such as `cat`, `grep`, "
        "`head`, `wc`, `ls`, `stat`, `jq`), `git log` with a pathspec or `-p`/`-S`/`-G`, or "
        "`git blame`/`git grep`. A `grep` that filters another command's output is not "
        "retrieval. `chained` means another CodeSage call arrived before any native "
        "retrieval; `rate` counts chained calls in the denominator, `strict rate` "
        "excludes them. Exclusions, counted separately: `policy-read` is a `Read` "
        "followed within the window by an `Edit`/`Write` of the same path (harness "
        "read-before-edit rule, not a search); `follow-through` is a `Read` of a path "
        "the result itself named. Shell paths are judged against the call's `project` "
        "argument: absolute paths outside that root, and every absolute path on a call "
        "that carried no `project` (`unscoped`), are scored conservatively — they never "
        "count as retrieval, so those calls lean terminal. Delegation is likewise "
        "one-directional: `Agent`/`Task` and other bookkeeping tools consume no window "
        "slot and whatever a subagent greps is never charged to the parent's call, "
        "another terminal-favouring bias. Definition adapted from "
        "ripwire `docs/METHODOLOGY.md` §9; the policy-read exclusion is broader than "
        "ripwire's (any Read followed by an Edit of the same path, not only an edit "
        "verb's own target)."
    )
    out.append("")
    term_tools: dict[str, dict[str, Any]] = term.get("tools") or {}
    if not term_tools:
        out.append("_No codesage tool calls in the window._")
    else:
        out.append(
            "| tool | calls | terminal | non-terminal | chained | rate | strict rate | "
            "policy-read | follow-through | top first follow-ups |"
        )
        out.append("|---|---:|---:|---:|---:|---:|---:|---:|---:|---|")

        def rate(value: float | None) -> str:
            return "n/a" if value is None else f"{100.0 * value:.1f}%"

        def term_row(label: str, b: dict[str, Any]) -> str:
            tops = ", ".join(f"`{name}`×{cnt}" for name, cnt in b.get("top_followups") or [])
            return (
                f"| {label} | {b['calls']} | {b['terminal']} | {b['non_terminal']} | "
                f"{b['chained']} | {rate(b.get('terminal_rate'))} | "
                f"{rate(b.get('terminal_rate_strict'))} | "
                f"{b['policy_read']} | {b['follow_through_read']} | {tops or '—'} |"
            )

        overall_bucket = term.get("all") or _new_terminality_bucket()
        for t in sorted(term_tools, key=lambda t: (-term_tools[t]["calls"], t)):
            out.append(term_row(f"`{t}`", term_tools[t]))
        out.append(term_row("**all**", overall_bucket))
        out.append("")
        out.append(
            f"Unscoped calls (no `project` argument; shell paths scored conservatively): "
            f"{overall_bucket.get('unscoped', 0)} of {overall_bucket['calls']}."
        )
        evidence = term.get("evidence") or []
        if evidence:
            out.append("")
            out.append(f"Non-terminal calls and the follow-up that decided them (first {min(len(evidence), 20)} of {len(evidence)}):")
            out.append("")
            for row in evidence[:20]:
                snippet = " ".join(str(row.get("evidence") or "").split())
                if len(snippet) > 160:
                    snippet = snippet[:157] + "..."
                out.append(f"- `{row.get('tool')}` → `{row.get('followup')}`: `{snippet}`")
    out.append("")

    out.append("## Verdict")
    out.append("")
    notes: list[str] = []
    total_codesage = cr
    if total_codesage < 20:
        notes.append(
            f"**Inconclusive on quality** — only {total_codesage} codesage result(s) in "
            "the window. Widen `--window-days` or check back after more usage."
        )
    else:
        ok_rate = pct(
            sum(r["ok"] for r in q.values()),
            sum(sum(r.values()) for r in q.values()),
        )
        notes.append(f"Ok-rate across all codesage calls: {ok_rate}. Target: >90%.")
    if gc > 0:
        ident_pct = 100.0 * gi / gc
        if ident_pct >= 30:
            notes.append(
                f"**Grep-vs-codesage gap is large**: {ident_pct:.0f}% of Grep tool + shell grep calls were "
                "identifier-shaped — codesage would have answered them in one call. The "
                "tool-selection affordances (CLAUDE.md directives, MCP tool descriptions) "
                "are not winning yet. Consider the next escalation: stronger directives or "
                "hook-based steering."
            )
        elif ident_pct >= 10:
            notes.append(
                f"**Grep-vs-codesage gap is moderate**: {ident_pct:.0f}% of Grep tool + shell grep calls were "
                "identifier-shaped. Watch the trend; if it doesn't fall in the next sweep, "
                "the current affordances aren't enough."
            )
        else:
            notes.append(
                f"**Grep-vs-codesage gap is small**: {ident_pct:.0f}% of Grep tool + shell grep calls were "
                "identifier-shaped. Current affordances appear to be doing their job."
            )
    for n in notes:
        out.append(f"- {n}")
    out.append("")

    shape_counts: dict[str, dict[str, int]] = agg.get("question_shape_counts") or {}
    if shape_counts:
        out.append("## Tool selection by user-question shape")
        out.append("")
        out.append(
            "Each user message in a session that had codesage available is bucketed by "
            "question shape (`semantic`, `identifier`, `literal`, `other`). "
            "The first tool the agent reached for in response is recorded; a `Bash` "
            "shell grep (`grep`/`rg`/`git grep`, or `find -exec grep` with a recovered "
            "pattern) is filed under `first=Grep` and any other retrieval `find` under "
            "`first=Glob`; other Bash calls and bookkeeping tools are skipped. "
            "Classifier rules in `classify_user_question`."
        )
        out.append("")
        out.append(
            "**Question this answers**: do agents skip CodeSage *on the question shapes "
            "where it would actually win* (semantic / paraphrase / concept queries), or "
            "is the 1.1% retrospective rate dominated by identifier-shaped questions "
            "where Grep is genuinely fine?"
        )
        out.append("")
        out.append(
            "Two denominators per shape: total user questions of that shape, and the "
            "subset that triggered a *retrieval-class* first action (Grep / Read / Glob "
            "/ codesage retrieval). The CS-rate is computed against the retrieval "
            "subset — that's the one that reflects 'when the agent actually had a "
            "retrieval decision, what did it pick?', which is the question §2.3 cares "
            "about. Questions whose first action wasn't retrieval-class (`other/none` "
            "column) usually mean the agent answered from context or wrote code "
            "directly; those aren't tool-selection decisions."
        )
        out.append("")
        out.append("| shape | total | retrieval-decisions | first=codesage | first=`search` | first=Grep | first=Read | first=Glob | other/none | CS-rate |")
        out.append("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
        order = ["semantic", "identifier", "literal", "other"]
        for shape in order:
            row = shape_counts.get(shape) or {}
            questions = row.get("questions", 0)
            cs = row.get("first_codesage_retrieval", 0)
            cs_search = row.get("first_codesage_search", 0)
            gp = row.get("first_grep", 0)
            rd = row.get("first_read", 0)
            gl = row.get("first_glob", 0)
            other = row.get("first_other_or_none", 0)
            decisions = cs + gp + rd + gl  # retrieval-class subset
            rate = pct(cs, decisions)
            out.append(
                f"| {shape} | {questions} | {decisions} | {cs} | {cs_search} | {gp} | {rd} | {gl} | {other} | {rate} |"
            )
        out.append("")

        sem = shape_counts.get("semantic") or {}
        sem_q = sem.get("questions", 0)
        sem_cs = sem.get("first_codesage_retrieval", 0)
        sem_decisions = (
            sem.get("first_codesage_retrieval", 0)
            + sem.get("first_grep", 0)
            + sem.get("first_read", 0)
            + sem.get("first_glob", 0)
        )
        if sem_decisions >= 5:
            sem_rate = 100.0 * sem_cs / sem_decisions
            if sem_rate >= 30:
                verdict = (
                    f"**CodeSage is reaching semantic-shaped questions: {sem_rate:.0f}%.** "
                    "The 1.1% aggregate retrospective rate was dominated by identifier-"
                    "shaped questions where Grep is genuinely fine. The system-prompt "
                    "override mechanism is working on the cases that matter; do **not** "
                    "escalate to enforcement hooks (path c)."
                )
            elif sem_rate >= 10:
                verdict = (
                    f"**CodeSage rate on semantic questions is partial: {sem_rate:.0f}%.** "
                    "The override is helping but not winning. Watch the trend; if it "
                    "doesn't climb past 30% in the next sweep, consider hardening the "
                    "override text before escalating to hooks."
                )
            else:
                verdict = (
                    f"**CodeSage is *also* skipped on semantic questions: {sem_rate:.0f}%.** "
                    "The override mechanism isn't winning where it has the strongest "
                    "argument. Path (c) — enforcement hooks — is justified."
                )
        else:
            verdict = (
                f"**Sample too small ({sem_decisions} semantic-shape retrieval decisions "
                f"in window, {sem_q} total semantic-shape questions).** "
                "Either widen the window with `--window-days`, or wait for more sessions "
                "before drawing a conclusion. Note that the harness corpora "
                "(`bench/corpora/{ripgrep,nest}-eval.yaml`) are also a source of "
                "retrieval decisions — those don't count here because they're driven "
                "by the harness, not by real user questions."
            )
        out.append(f"- {verdict}")
        out.append("")

    return "\n".join(out)




def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--window-days", type=int, default=7)
    ap.add_argument(
        "--projects-root",
        type=Path,
        default=Path.home() / ".claude" / "projects",
    )
    ap.add_argument("--output", type=Path, default=None)
    ap.add_argument(
        "--terminality-window",
        dest="terminality_window",
        type=int,
        default=DEFAULT_TERMINALITY_WINDOW,
        help="tool calls after a codesage call that the terminality metric inspects (default 3)",
    )
    ap.add_argument(
        "--window",
        dest="terminality_window",
        type=int,
        default=DEFAULT_TERMINALITY_WINDOW,
        help=argparse.SUPPRESS,
    )
    args = ap.parse_args()
    if args.terminality_window < 1:
        ap.error("--terminality-window must be >= 1")

    min_mtime = (_dt.datetime.now() - _dt.timedelta(days=args.window_days)).timestamp()
    transcripts = iter_transcripts(args.projects_root, min_mtime)
    events_per_transcript = [extract_events(t) for t in transcripts]
    agg = aggregate(
        events_per_transcript,
        terminality_window=args.terminality_window,
        subagent_flags=[is_subagent_transcript(t) for t in transcripts],
    )
    now = _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    report = render(
        agg,
        window_days=args.window_days,
        transcripts=len(transcripts),
        subagent_transcripts=sum(1 for t in transcripts if is_subagent_transcript(t)),
        now=now,
    )
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(report, encoding="utf-8")
        print(f"wrote {args.output}", file=sys.stderr)
    else:
        sys.stdout.write(report)
    return 0


if __name__ == "__main__":
    sys.exit(main())
