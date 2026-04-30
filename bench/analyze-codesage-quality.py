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

No forward instrumentation; all signal is extracted from transcripts at
`~/.claude/projects/*/*.jsonl`.

Usage:
  bench/analyze-codesage-quality.py [--window-days 7] [--projects-root PATH] [--output PATH]

Stdlib only.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import re
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any

TOOL_PREFIX = "mcp__codesage__"

# CodeSage MCP tools that answer *retrieval* questions (where is X defined,
# where is X used, what does this module depend on, bundle this context).
# Excludes pure risk/coupling tools like `assess_risk` because those are a
# different workflow — the tool-selection-rate metric is specifically about
# "when the agent wants to find code, does it pick codesage or Grep?"
RETRIEVAL_CODESAGE_TOOLS = {
    "search",
    "find_symbol",
    "find_references",
    "impact_analysis",
    "export_context",
    "list_dependencies",
}

# Tool names we care about for utility pairing. These are the Claude Code
# built-ins the agent reaches for when it bypasses MCP retrieval.
FALLBACK_TOOLS = {"Grep", "Read", "Glob"}

# Identifier shape — matches ASCII code tokens: function names, type names,
# constants. Permissive on length so short tokens like `fd` and `pt` count.
IDENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")

# Regex metacharacters that, if present in a Grep pattern, mean the agent
# wanted something more than a literal-identifier search. `|` and whitespace
# are excluded because pipe-joined identifiers / multi-word keywords (`fn foo`)
# are still identifier-shaped lookups from CodeSage's perspective.
REGEX_META_RE = re.compile(r"[.\\\[\](){}^$*+?]")


# -----------------------------------------------------------------------------
# Extraction (mirrors analyze-codesage-usage.py walker)
# -----------------------------------------------------------------------------


def iter_transcripts(root: Path, min_mtime: float) -> list[Path]:
    out: list[Path] = []
    if not root.is_dir():
        return out
    for project in root.iterdir():
        if not project.is_dir():
            continue
        for f in project.glob("*.jsonl"):
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

            # User text turn: role=user with string content, or role=user
            # with a list whose entries are plain text (not tool_result).
            if role == "user":
                user_text = ""
                if isinstance(content, str):
                    user_text = content
                elif isinstance(content, list):
                    parts = []
                    has_tool_result = False
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


# -----------------------------------------------------------------------------
# Quality signals
# -----------------------------------------------------------------------------


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
    # Try JSON parse. Empty arrays and empty {results: []} -like shapes are
    # treated as empty responses.
    try:
        data = json.loads(t)
    except (json.JSONDecodeError, ValueError):
        return "ok"
    if data is None:
        return "empty"
    if isinstance(data, list) and not data:
        return "empty"
    if isinstance(data, dict):
        # A result dict is "empty" if every top-level collection is empty.
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


# Tighter than the first draft after sampling real transcripts:
# the broad keyword list (`auth|config|...`) over-matched conversational
# messages, and pasted slash-command bodies were being treated as user
# questions. Now we require an actual question shape AND filter out
# pasted bodies / task notifications before classifying.
SEMANTIC_QUESTION_RES = [
    # Formal "where does X happen?" / "how does X work?" phrasings.
    re.compile(r"\bwhere\s+(does|is|are|do\s+we)\b.*\b(handle|happen|live|loaded|defined|managed|stored|implemented|done|fire|trigger)",
               re.IGNORECASE),
    re.compile(r"\bhow\s+(does|do\s+we|is)\b.*\b(work|handle|implement|done|done\b)", re.IGNORECASE),
    re.compile(r"\bfind\s+(the\s+)?(file|place|spot|code|spot)\s+(that|where)\b", re.IGNORECASE),
    re.compile(r"\bwhich\s+file\s+(handles|implements|contains|holds|owns)", re.IGNORECASE),
    re.compile(r"\bwhat\s+(handles|implements|does)\b", re.IGNORECASE),
    # Implementation-request shapes that imply retrieval first ("look at the
    # auth flow", "show me where config is loaded", "fix the search code").
    # These are the way real users actually phrase concept-shaped requests
    # in conversational sessions — formal "where does X?" phrasing is rare.
    re.compile(r"\b(look|take\s+a\s+look)\s+at\s+(the\s+)?\w+\s+(flow|module|code|pipeline|logic|path|handler|system|layer)",
               re.IGNORECASE),
    re.compile(r"\bshow\s+me\s+(where|the|how)\s+\w+", re.IGNORECASE),
    re.compile(r"\bin\s+the\s+\w+\s+(flow|module|pipeline|handler|layer|system)\b", re.IGNORECASE),
    re.compile(r"\bfix\s+(the\s+)?\w+\s+(flow|code|logic|pipeline|handler|bug)", re.IGNORECASE),
    re.compile(r"\bexplain\s+(the\s+|how\s+)?\w+\s+(flow|works|module|code|logic)", re.IGNORECASE),
]
IDENTIFIER_QUESTION_RES = [
    # Backtick-quoted identifiers: `Foo`, `do_the_thing`, `Foo::bar`.
    re.compile(r"`[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*`"),
    # "where is Foo defined / declared / called / used"
    re.compile(r"\bwhere\s+is\s+[A-Za-z_][A-Za-z0-9_]{3,}\s+(defined|declared|called|used|implemented)",
               re.IGNORECASE),
    # "find references to Foo" / "find callers of Foo"
    re.compile(r"\bfind\s+(all\s+)?(references?|callers?|callees?)\s+(to|of)\s+[A-Za-z_]"),
    # "what calls X" / "who calls X"
    re.compile(r"\b(what|who)\s+calls\s+[A-Za-z_]"),
]
LITERAL_QUESTION_RES = [
    re.compile(r"\b(TODO|FIXME|XXX|HACK)\b"),
    re.compile(r"\bgrep(\s+for)?\s+[\"']", re.IGNORECASE),
    re.compile(r"\bsearch\s+for\s+[\"']", re.IGNORECASE),
    re.compile(r"\b(error|warning|log)\s+message\b", re.IGNORECASE),
]
# Skip-patterns: these aren't real user questions, they're pasted text
# from slash-command bodies, hook events, or system notifications that
# happen to arrive on a `role=user` envelope. Bucketing them inflates
# every shape and dilutes the rate.
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
        # Real user questions are short. Long bodies are almost always
        # pasted command/skill text.
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
    # Strip outer quoting artifacts (some transcripts include surrounding quotes).
    p = pattern.strip()
    # Fast reject on regex metacharacters.
    if REGEX_META_RE.search(p):
        return False
    # Split on top-level `|`. Each alternative must be either a single
    # identifier or a whitespace-separated list of identifiers.
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
    # Be permissive: even if the pattern isn't identifier-shaped overall,
    # pull out any bareword tokens that happen to be present. A mixed
    # pattern like `'foo\b|bar'` still tells us the agent cared about
    # `foo` and `bar` as symbols.
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
    # Cheap peek into response text for small payloads.
    text = event.get("text") or ""
    if text and len(text) < 8192:
        for token in re.findall(r"[A-Za-z_][A-Za-z0-9_]*", text):
            if len(token) >= 4:
                subjects.add(token)
    return subjects


# -----------------------------------------------------------------------------
# Aggregation
# -----------------------------------------------------------------------------


def aggregate(events_per_transcript: list[list[dict[str, Any]]]) -> dict[str, Any]:
    # Per-tool result categorization
    quality: dict[str, dict[str, int]] = defaultdict(lambda: {"ok": 0, "empty": 0, "error": 0})

    # Utility signals
    grep_calls = 0
    grep_identifier_shaped = 0
    grep_multi_ident = 0  # pipe-joined or whitespace-split → multiple codesage calls would be needed
    followup_grep_after_codesage = 0
    codesage_results_total = 0

    # Tool-selection signals (recommendations doc §2.3 active half):
    # count retrieval-class calls by tool so "when the agent faced a
    # retrieval decision, which tool did it pick?" is directly answerable.
    codesage_retrieval_calls: dict[str, int] = defaultdict(int)
    # Sessions that had at least one codesage MCP tool attached (i.e. the
    # agent could have used codesage if it wanted to). Isolates the
    # availability-bias case: rate on sessions where codesage wasn't even
    # registered would trivially be 0 and would mask signal.
    sessions_with_codesage_available = 0
    sessions_total = 0
    identifier_grep_in_codesage_sessions = 0
    codesage_retrieval_in_codesage_sessions = 0

    # Ratio: after each codesage tool_result, how many Reads before the next
    # non-Read, non-Grep event? Accumulated as a list for distribution stats.
    reads_after_codesage: list[int] = []

    # Per-question-shape breakdown (recommendations doc §2.3 path-(b) follow-up,
    # 2026-04-30 — option (2) of "do agents skip CodeSage on questions where
    # it would actually win?"). Each user message that prompts a retrieval-
    # shape first action is bucketed by its question shape (semantic /
    # identifier / literal / other). The "first retrieval action" is the
    # first tool_use of {Grep, Read, Glob, mcp__codesage__*} after the
    # user message and before the next user message — this is the agent's
    # tool-selection decision in response to the question.
    # Counters: per shape, count first tools chosen.
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

    # Retrieval-class first tools: these are the ones we count as a
    # "retrieval decision". Read counts because the agent may have
    # decided to read a specific file rather than search — that's still
    # a retrieval choice, just one CodeSage doesn't compete with directly
    # (Read needs a known path).
    def first_retrieval_tool(events: list[dict[str, Any]], start: int) -> tuple[str | None, int]:
        """From `start`, scan forward until the first tool_use OR the
        next user message, whichever comes first. Return (tool_name, idx)
        or (None, end-of-window-idx). Tool name returned verbatim; the
        caller decides whether it counts as retrieval-shape.
        """
        n = len(events)
        j = start
        while j < n:
            e = events[j]
            if e.get("kind") == "user":
                return None, j
            if e.get("kind") == "tool_use":
                return (e.get("tool") or ""), j
            j += 1
        return None, n

    def shape_walk(events: list[dict[str, Any]], session_had_codesage: bool) -> None:
        """For each *real* user question (excluding pasted command bodies
        and system notifications), classify by shape and record the
        first tool the agent reached for in response. Only sessions
        where codesage was available are counted.

        The denominator the rendered output cares about is
        `retrieval_decisions` per shape — questions that resulted in a
        retrieval-class first action. That excludes turns where the
        agent immediately wrote code, ran a Bash command, or reasoned
        without a tool — none of which are retrieval choices.
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
            first_tool, _next = first_retrieval_tool(events, i + 1)
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

    for events in events_per_transcript:
        sessions_total += 1
        # Detect whether codesage was registered in this session at all.
        # Any `tool_use` naming a codesage tool is proof the tool was
        # available; without at least one call we can't tell from the
        # transcript whether the agent *could* have reached for codesage.
        had_codesage = any(
            ev["kind"] == "tool_use" and (ev.get("tool") or "").startswith(TOOL_PREFIX)
            for ev in events
        )
        if had_codesage:
            sessions_with_codesage_available += 1

        # Question-shape pass (does not depend on the streaming detail walk
        # below — keeps the new code self-contained and easier to remove if
        # the metric is rotated out later).
        shape_walk(events, had_codesage)

        # Track recent codesage subject-sets with a tiny buffer. A follow-up
        # Grep within 5 subsequent tool_uses on any tracked subject counts
        # as "codesage didn't satisfy" — 5 is a heuristic, short enough to
        # stay topical, long enough to survive incidental intermediate
        # actions like Read.
        recent_codesage_subjects: list[set[str]] = []
        # Stream events in order.
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
                    recent_codesage_subjects.append(subj)
                    if len(recent_codesage_subjects) > 5:
                        recent_codesage_subjects.pop(0)
            elif e["kind"] == "tool_result" and (e.get("pair_tool") or "").startswith(TOOL_PREFIX):
                tool = e["pair_tool"][len(TOOL_PREFIX):]
                cat = classify_codesage_result(e.get("text") or "")
                quality[tool][cat] += 1
                codesage_results_total += 1
                # Also update subject set with whatever the response names.
                subj = extract_codesage_subject(e)
                if subj:
                    if recent_codesage_subjects:
                        recent_codesage_subjects[-1] = recent_codesage_subjects[-1] | subj
                    else:
                        recent_codesage_subjects.append(subj)
                # Count Reads until next non-(Read|Grep) tool_use.
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
                            # Grep after codesage counts as a pile-on too; but
                            # we stop the Read-counter here since the agent
                            # switched tactics.
                            break
                        break
                    j += 1
                reads_after_codesage.append(reads)
            elif e["kind"] == "tool_use" and e.get("tool") == "Grep":
                grep_calls += 1
                pattern = (e.get("input") or {}).get("pattern", "")
                if isinstance(pattern, str):
                    if identifier_shaped_grep(pattern):
                        grep_identifier_shaped += 1
                        if had_codesage:
                            identifier_grep_in_codesage_sessions += 1
                        # Multi-identifier patterns are strictly stronger: the
                        # agent would have needed N find_symbol calls, not one.
                        if "|" in pattern or len(pattern.split()) > 1:
                            grep_multi_ident += 1
                    # Follow-up Grep on a recent codesage subject?
                    grep_idents = extract_grep_identifiers(pattern)
                    if grep_idents and any(grep_idents & s for s in recent_codesage_subjects):
                        followup_grep_after_codesage += 1
            i += 1

    return {
        "quality": quality,
        "grep_calls": grep_calls,
        "grep_identifier_shaped": grep_identifier_shaped,
        "grep_multi_ident": grep_multi_ident,
        "followup_grep_after_codesage": followup_grep_after_codesage,
        "codesage_results_total": codesage_results_total,
        "reads_after_codesage": reads_after_codesage,
        "codesage_retrieval_calls": dict(codesage_retrieval_calls),
        "sessions_total": sessions_total,
        "sessions_with_codesage_available": sessions_with_codesage_available,
        "identifier_grep_in_codesage_sessions": identifier_grep_in_codesage_sessions,
        "codesage_retrieval_in_codesage_sessions": codesage_retrieval_in_codesage_sessions,
        "question_shape_counts": {k: dict(v) for k, v in shape_counts.items()},
    }


# -----------------------------------------------------------------------------
# Rendering
# -----------------------------------------------------------------------------


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
) -> str:
    out: list[str] = []
    q: dict[str, dict[str, int]] = agg["quality"]

    out.append("# CodeSage MCP quality + utility analysis")
    out.append("")
    out.append(f"**Window**: last {window_days} days  ")
    out.append(f"**Transcripts scanned**: {transcripts}  ")
    out.append(f"**Run at**: {now}  ")
    out.append(f"**CodeSage tool_results analyzed**: {agg['codesage_results_total']}  ")
    out.append(f"**Grep calls observed (any context)**: {agg['grep_calls']}")
    out.append("")

    # ------------------------------------------------------------------ quality
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

    # ----------------------------------------------------------------- utility
    out.append("## Utility: Grep that CodeSage would have answered")
    out.append("")
    gc = agg["grep_calls"]
    gi = agg["grep_identifier_shaped"]
    gm = agg["grep_multi_ident"]
    out.append(f"- **Grep calls in window**: {gc}")
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
        f"- **Identifier-shaped Grep calls in same sessions**: {id_grep_in_avail}"
    )
    if avail_decisions > 0:
        rate = 100.0 * retr_in_avail / avail_decisions
        out.append(
            f"- **Tool-selection rate** (retrieval-class picks that went to CodeSage "
            f"over Grep, sessions where codesage was available): "
            f"**{rate:.1f}%** ({retr_in_avail} CodeSage / {avail_decisions} total "
            f"retrieval-shape decisions)"
        )
    else:
        out.append("- **Tool-selection rate**: n/a (no retrieval-shape decisions in window)")
    out.append("")
    out.append(
        "Interpretation: in sessions where the agent *could have* used CodeSage, "
        "retrieval-class picks went to either a CodeSage MCP tool or to an "
        "identifier-shaped Grep pattern that CodeSage would have answered. High "
        "rate (>70%) means the agent reaches for CodeSage on merit. Near-zero "
        "rate means the tool affordances (descriptions, CLAUDE.md directives) "
        "are not winning — escalation path is either stronger prompts or "
        "hook-based steering à la the landscape's LSP-enforcement-kit."
    )
    out.append("")

    out.append("## Utility: follow-up Grep after a CodeSage result")
    out.append("")
    fg = agg["followup_grep_after_codesage"]
    cr = agg["codesage_results_total"]
    out.append(
        f"- **Codesage results followed by a Grep on a mentioned identifier within 5 "
        f"tool_uses**: {fg} ({pct(fg, cr)})"
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

    # --------------------------------------------------------- verdict section
    out.append("## Verdict")
    out.append("")
    # Verdict is deliberately short and conservative; thresholds reflect the
    # dominant failure modes the analyzer was designed to detect.
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
                f"**Grep-vs-codesage gap is large**: {ident_pct:.0f}% of Grep calls were "
                "identifier-shaped — codesage would have answered them in one call. The "
                "tool-selection affordances (CLAUDE.md directives, MCP tool descriptions) "
                "are not winning yet. Consider the next escalation: stronger directives or "
                "hook-based steering."
            )
        elif ident_pct >= 10:
            notes.append(
                f"**Grep-vs-codesage gap is moderate**: {ident_pct:.0f}% of Grep calls were "
                "identifier-shaped. Watch the trend; if it doesn't fall in the next sweep, "
                "the current affordances aren't enough."
            )
        else:
            notes.append(
                f"**Grep-vs-codesage gap is small**: {ident_pct:.0f}% of Grep calls were "
                "identifier-shaped. Current affordances appear to be doing their job."
            )
    for n in notes:
        out.append(f"- {n}")
    out.append("")

    # ----- Per-question-shape breakdown (§2.3 path-(b) follow-up, option 2)
    shape_counts: dict[str, dict[str, int]] = agg.get("question_shape_counts") or {}
    if shape_counts:
        out.append("## Tool selection by user-question shape")
        out.append("")
        out.append(
            "Each user message in a session that had codesage available is bucketed by "
            "question shape (`semantic`, `identifier`, `literal`, `other`). "
            "The first tool the agent reached for in response is recorded. "
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
            q = row.get("questions", 0)
            cs = row.get("first_codesage_retrieval", 0)
            cs_search = row.get("first_codesage_search", 0)
            gp = row.get("first_grep", 0)
            rd = row.get("first_read", 0)
            gl = row.get("first_glob", 0)
            other = row.get("first_other_or_none", 0)
            decisions = cs + gp + rd + gl  # retrieval-class subset
            rate = pct(cs, decisions)
            out.append(
                f"| {shape} | {q} | {decisions} | {cs} | {cs_search} | {gp} | {rd} | {gl} | {other} | {rate} |"
            )
        out.append("")

        # Explicit interpretation note: the metric that decides path (c).
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


# -----------------------------------------------------------------------------
# CLI
# -----------------------------------------------------------------------------


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--window-days", type=int, default=7)
    ap.add_argument(
        "--projects-root",
        type=Path,
        default=Path.home() / ".claude" / "projects",
    )
    ap.add_argument("--output", type=Path, default=None)
    args = ap.parse_args()

    min_mtime = (_dt.datetime.now() - _dt.timedelta(days=args.window_days)).timestamp()
    transcripts = iter_transcripts(args.projects_root, min_mtime)
    events_per_transcript = [extract_events(t) for t in transcripts]
    agg = aggregate(events_per_transcript)
    now = _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    report = render(
        agg,
        window_days=args.window_days,
        transcripts=len(transcripts),
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
