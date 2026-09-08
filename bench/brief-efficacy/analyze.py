#!/usr/bin/env python3
"""Efficacy analysis for `codesage brief` hook fires.

Reads the fire ledger (`brief-fires.jsonl` plus its rotation sibling
`brief-fires.jsonl.1`) from the CodeSage state dir, summarizes the
decision mix, pairs `served` fires with Claude Code session transcripts,
and scores each serve as acted / ambiguous / no-op. Also computes an
unconditioned base rate of "runs file-named tests after editing a file"
across the same transcripts. See README.md for the measurement design,
scoring rules, and the decision rule.

Stdlib only. Usage:

    python3 analyze.py [--ledger-dir DIR] [--projects-dir DIR]
                       [--min-served 50] [--json]
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import shlex
import sys
import tempfile
from collections import Counter, defaultdict
from pathlib import Path

FIRE_LOG = "brief-fires.jsonl"
DECISIONS = ("served", "empty", "repeat", "cooldown", "budget", "unavailable", "error")

# One rendered-payload line, mirroring render_brief() in
# crates/cli/src/commands/query.rs.
PAYLOAD_LINE = re.compile(r"^(hotspot: churn percentile |tests: |changes with: )")

TEST_RUN_HINT = re.compile(
    r"\b(cargo (nextest|test)|pytest|phpunit|go test|npm (run )?test|pnpm test|"
    r"yarn test|jest|vitest|make test|php run-tests|artisan test|paratest)\b"
)


def fnv1a64(s: str) -> str:
    """Matches digest() in crates/cli/src/brief_gate.rs: FNV-1a over UTF-8,
    formatted `{:x}` (lowercase hex, no zero padding)."""
    h = 0xCBF29CE484222325
    for b in s.encode("utf-8"):
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return format(h, "x")


def candidate_runtime_dirs() -> list[Path]:
    """Mirrors candidate_runtime_dirs_from() in crates/cli/src/daemon.rs. A
    hook fired from a process without XDG_RUNTIME_DIR wrote its rows to the
    /tmp candidate, so a legacy read has to cover every one."""
    dirs: list[Path] = []
    env = os.environ.get("CODESAGE_DAEMON_RUNTIME_DIR")
    if env:
        dirs.append(Path(env))
    xdg = os.environ.get("XDG_RUNTIME_DIR")
    if xdg:
        dirs.append(Path(xdg) / "codesage")
    uid = os.getuid()
    for tmp in (Path("/tmp"), Path(tempfile.gettempdir())):
        d = tmp / f"codesage-{uid}"
        if d not in dirs:
            dirs.append(d)
    return dirs


def default_runtime_dir() -> Path:
    return candidate_runtime_dirs()[0]


def default_ledger_dir() -> Path:
    """Mirrors ledger_dir() in crates/cli/src/brief_gate.rs: the state dir,
    not the tmpfs runtime dir, so the ledger survives a reboot."""
    xdg = os.environ.get("XDG_STATE_HOME")
    if xdg and Path(xdg).is_absolute():
        return Path(xdg) / "codesage"
    home = os.environ.get("HOME")
    if home and Path(home).is_absolute():
        return Path(home) / ".local" / "state" / "codesage"
    return default_runtime_dir()


def valid_fire(rec) -> bool:
    """Ledger rows are best-effort writes; validate before trusting a field.
    `t` numeric (bool is an int in Python — exclude it), the string fields
    strings, `h`/`tok` optional but typed when present."""
    if not isinstance(rec, dict):
        return False
    t = rec.get("t")
    if isinstance(t, bool) or not isinstance(t, (int, float)):
        return False
    for key in ("s", "p", "f", "d"):
        if not isinstance(rec.get(key), str):
            return False
    if "h" in rec and not isinstance(rec["h"], str):
        return False
    return True


def read_generation(p: Path, seen_files: set[tuple[int, int]] | None = None) -> list[dict]:
    try:
        with p.open(encoding="utf-8", errors="replace") as stream:
            stat = os.fstat(stream.fileno())
            identity = (stat.st_dev, stat.st_ino)
            if seen_files is not None:
                if identity in seen_files:
                    return []
                seen_files.add(identity)
            raw = stream.read()
    except FileNotFoundError:
        return []
    except OSError as e:
        # A legacy candidate under world-writable /tmp can be another user's
        # dir or a plain file; one unreadable candidate must not take the
        # state-dir rows down with it.
        print(f"skipping unreadable ledger {p}: {e}", file=sys.stderr)
        return []
    fires = []
    for line in raw.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if valid_fire(rec):
            fires.append(rec)
    return fires


def load_fires(*dirs: Path) -> list[dict]:
    """Union of physical ledgers in `dirs`, time-ordered. More than one
    dir is the normal case after the move to the state dir: rows written before
    it sit in the runtime dir until the next reboot, and the ledger is a
    denominator, so both halves count."""

    fires: list[dict] = []
    seen_files: set[tuple[int, int]] = set()
    for d in dirs:
        for path in (d / (FIRE_LOG + ".1"), d / FIRE_LOG):
            fires.extend(read_generation(path, seen_files))
    fires.sort(key=lambda r: r["t"])
    return fires


def munge_project(path: str) -> str:
    """Claude Code project dir name: non-alphanumerics become '-'."""
    return re.sub(r"[^A-Za-z0-9]", "-", path)


def transcript_path(projects_dir: Path, project: str, session: str) -> Path | None:
    if not re.fullmatch(r"[A-Za-z0-9_-][A-Za-z0-9_.-]{0,127}", session):
        return None
    d = projects_dir / munge_project(project)
    p = d / f"{session}.jsonl"
    if p.is_file():
        return p
    matches = list(projects_dir.glob(f"*/{session}.jsonl"))
    return matches[0] if len(matches) == 1 else None


def payload_candidates(text: str):
    """Runs of consecutive brief-shaped lines inside a transcript string."""
    lines = text.split("\n")
    run: list[str] = []
    for line in lines + [""]:
        if PAYLOAD_LINE.match(line):
            run.append(line)
        elif run:
            block = "\n".join(run)
            yield block + "\n"  # render_brief terminates every line
            yield block
            run = []


def payload_occurrences(events: list[dict]) -> dict[str, list[tuple[int, str]]]:
    """digest -> [(event index, payload text), ...] in transcript order.

    Only successful hook attachments count as exposure. Two
    files can be served byte-identical payloads (same digest, distinct ledger
    rows), so a serve must consume occurrences in order rather than always
    taking the first match — the caller pairs the nth ledger row carrying a
    digest with the nth transcript occurrence of it. One event contributes at
    most one occurrence per digest (a payload echoed twice inside a single
    event is one injection, not two)."""
    occ: dict[str, list[tuple[int, str]]] = defaultdict(list)
    injections: set[tuple[str, str]] = set()
    for i, ev in enumerate(events):
        if not isinstance(ev, dict):
            continue
        attachment = ev.get("attachment")
        if not isinstance(attachment, dict) or attachment.get("type") not in (
            "hook_success", "hook_additional_context",
        ):
            continue
        if attachment.get("exitCode", 0) != 0:
            continue
        strings = []
        for key in ("stdout", "content", "additionalContext"):
            value = attachment.get(key)
            if isinstance(value, str):
                try:
                    decoded = json.loads(value)
                except ValueError:
                    decoded = None
                if isinstance(decoded, dict):
                    output = decoded.get("hookSpecificOutput", {})
                    if isinstance(output, dict) and isinstance(output.get("additionalContext"), str):
                        strings.append(output["additionalContext"])
                else:
                    strings.append(value)
            elif key == "content" and isinstance(value, list):
                strings.extend(s for s in value if isinstance(s, str))
        seen_here: set[str] = set()
        for s in strings:
            if (
                "hotspot: churn percentile" not in s
                and "changes with: " not in s
                and "tests: " not in s
            ):
                continue
            for cand in payload_candidates(s):
                d = fnv1a64(cand)
                if d not in seen_here:
                    seen_here.add(d)
                    tool_id = attachment.get("toolUseID")
                    if isinstance(tool_id, str) and tool_id:
                        key = (tool_id, d)
                        if key in injections:
                            continue
                        injections.add(key)
                    occ[d].append((i, cand))
    return occ


def parse_payload(payload: str) -> tuple[list[str], list[str]]:
    tests, coupled = [], []
    for line in payload.splitlines():
        if line.startswith("tests: "):
            tests = [p.strip() for p in line[len("tests: "):].split(",") if p.strip()]
        elif line.startswith("changes with: "):
            coupled = [p.strip() for p in line[len("changes with: "):].split(",") if p.strip()]
    return tests, coupled


def tool_uses(events: list[dict]):
    """(event index, tool name, input dict) for every assistant tool_use."""
    for i, ev in enumerate(events):
        if not isinstance(ev, dict) or ev.get("type") != "assistant":
            continue
        msg = ev.get("message")
        if not isinstance(msg, dict):
            continue
        content = msg.get("content")
        if not isinstance(content, list):
            continue
        for item in content:
            if isinstance(item, dict) and item.get("type") == "tool_use":
                name = item.get("name", "")
                inp = item.get("input", {})
                if isinstance(inp, dict):
                    yield i, name, inp


def runner_test_paths(runner: str, args: list[str]) -> list[str]:
    if runner in ("python", "python3"):
        while args and args[0] in ("-u", "-B", "-E", "-I", "-s", "-S", "-O", "-OO"):
            args = args[1:]
        if args[:2] in (["-m", "pytest"], ["-m", "unittest"]):
            runner, args = args[1], args[2:]
        elif len(args) == 1 and not args[0].startswith("-"):
            return args
        else:
            return []

    flags, values, short_flags = set(), set(), ""
    if runner == "pytest":
        flags = {"--disable-warnings", "--strict-markers", "--strict-config", "--no-header", "--no-summary"}
        values = {"-k", "-m", "--maxfail", "--tb", "--capture", "--color"}
        short_flags = "vqxs"
    elif runner == "unittest":
        flags = {"--verbose", "--quiet", "--failfast", "--buffer", "--catch"}
        short_flags = "vqfbc"
    elif runner == "phpunit":
        flags = {"--testdox", "--stop-on-failure", "--stop-on-error", "--no-coverage"}
        values = {"--filter", "--configuration", "-c", "--bootstrap", "--colors"}
    elif runner in ("jest", "vitest"):
        if runner == "vitest":
            if args[:1] != ["run"]:
                return []
            args = args[1:]
        flags = {"--runInBand", "--runTestsByPath", "--silent", "--bail"} if runner == "jest" else {"--silent"}
        values = {"--testNamePattern", "-t"}
    elif runner == "node":
        if args[:1] != ["--test"]:
            return []
        args = args[1:]
    else:
        return []

    paths = []
    index = 0
    while index < len(args):
        arg = args[index]
        option, equals, _ = arg.partition("=")
        if arg == "--":
            paths.extend(args[index + 1:])
            break
        if option in values:
            if not equals:
                index += 1
                if index >= len(args) or args[index].startswith("-"):
                    return []
        elif arg in flags:
            pass
        elif arg.startswith("-"):
            if not short_flags or not re.fullmatch(f"-[{short_flags}]+", arg):
                return []
        else:
            paths.append(arg)
        index += 1
    return paths


def runs_named_test(command: str, tests: list[str]) -> bool:
    if any(marker in command for marker in ("<<", "$(", "`")):
        return False
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|()<>\n")
        lexer.whitespace = " \t\r"
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return False
    if len(tokens) >= 4 and tokens[0] == "cd" and tokens[2] == "&&":
        tokens = tokens[3:]
    if any(token and all(c in ";&|()<>\n" for c in token) for token in tokens):
        return False
    words = tokens
    if words[:2] == ["rtk", "proxy"]:
        words = words[2:]
    elif words[:1] == ["rtk"]:
        words = words[1:]
    if not words:
        return False
    runner = Path(words[0]).name
    paths = runner_test_paths(runner, words[1:])
    return any(path.split("::", 1)[0] == test or path.split("::", 1)[0].endswith("/" + test)
               for path in paths for test in tests)


def score_serve(events: list[dict], start: int, tests: list[str], coupled: list[str]) -> str:
    """Strict scoring: see README.md. `acted` needs a Bash run of a served
    test path or a full Read/Edit of a served co-change file after the serve.
    Ranged reads are ambiguous, never acted."""
    verdict = "no-op"
    for i, name, inp in tool_uses(events):
        if i <= start:
            continue
        if name == "Bash":
            cmd = inp.get("command", "")
            if isinstance(cmd, str) and runs_named_test(cmd, tests):
                return "acted"
        elif name in ("Read", "Edit", "Write", "MultiEdit"):
            fp = inp.get("file_path", "")
            if not isinstance(fp, str):
                continue
            if any(fp == c or fp.endswith("/" + c) for c in coupled):
                if name == "Read" and ("offset" in inp or "limit" in inp):
                    verdict = "ambiguous"
                else:
                    return "acted"
    return verdict


# --- base rate ---------------------------------------------------------------

TEST_NAME_TEMPLATES = (
    "{stem}Test",       # PHP
    "test_{stem}",      # Python
    "{stem}_test",      # Python / Go
    "{stem}.test.",     # JS/TS
    "{stem}.spec.",     # JS/TS
)


def file_named_test_tokens(file_path: str) -> list[str]:
    stem = Path(file_path).stem
    if not stem or len(stem) < 3:  # too generic to match against ("db", "io")
        return []
    return [t.format(stem=stem) for t in TEST_NAME_TEMPLATES]


def base_rate_for_transcript(events: list[dict]) -> tuple[int, int]:
    """(edits, edits followed by a file-named or stem-targeted test run)."""
    uses = list(tool_uses(events))
    edits = [
        (i, inp["file_path"])
        for i, name, inp in uses
        if name in ("Edit", "Write", "MultiEdit") and isinstance(inp.get("file_path"), str)
    ]
    bash = [(i, inp.get("command", "")) for i, name, inp in uses if name == "Bash"]
    total = followed = 0
    seen: set[str] = set()
    for i, fp in edits:
        if fp in seen:  # count each file once per session, like a serve would
            continue
        seen.add(fp)
        tokens = file_named_test_tokens(fp)
        if not tokens:
            continue
        total += 1
        stem = Path(fp).stem
        for j, cmd in bash:
            if j <= i or not isinstance(cmd, str):
                continue
            if any(t in cmd for t in tokens) or (TEST_RUN_HINT.search(cmd) and stem in cmd):
                followed += 1
                break
    return total, followed


def two_proportion_z(x1: int, n1: int, x2: int, n2: int) -> float | None:
    if n1 == 0 or n2 == 0:
        return None
    p1, p2 = x1 / n1, x2 / n2
    p = (x1 + x2) / (n1 + n2)
    se = math.sqrt(p * (1 - p) * (1 / n1 + 1 / n2))
    if se == 0:
        return None
    return (p1 - p2) / se


# --- main --------------------------------------------------------------------


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--ledger-dir", "--runtime-dir", dest="ledger_dir", type=Path,
                    default=None,
                    help="directory holding brief-fires.jsonl (default: "
                         "$XDG_STATE_HOME/codesage or ~/.local/state/codesage; "
                         "--runtime-dir is the legacy alias from when the ledger "
                         "lived in the runtime dir)")
    ap.add_argument("--projects-dir", type=Path, default=Path.home() / ".claude" / "projects")
    ap.add_argument("--min-served", type=int, default=50,
                    help="served-fire count the decision rule requires (default 50)")
    ap.add_argument("--json", action="store_true", help="machine-readable output")
    args = ap.parse_args()

    explicit = args.ledger_dir is not None
    if explicit:
        ledger_dirs = [args.ledger_dir]
    else:
        # Rows written before the ledger moved to the state dir sit in the
        # runtime dir until the next reboot; read both so the denominator
        # keeps them. An explicit --ledger-dir is honoured as given.
        ledger_dirs = [default_ledger_dir()]
        for legacy in candidate_runtime_dirs():
            if legacy not in ledger_dirs:
                ledger_dirs.append(legacy)
    ledger_label = " + ".join(str(d) for d in ledger_dirs)
    fires = load_fires(*ledger_dirs)
    if not fires:
        print(f"no fire ledger found under {ledger_label}", file=sys.stderr)
        return 1

    # (a) decision mix
    by_session: dict[str, Counter] = defaultdict(Counter)
    by_project: dict[str, Counter] = defaultdict(Counter)
    total = Counter()
    for f in fires:
        d = f.get("d", "?")
        total[d] += 1
        by_session[f.get("s", "?")][d] += 1
        by_project[f.get("p", "?")][d] += 1

    # (b) pair served fires with transcripts and score
    transcripts: dict[Path, list[dict]] = {}

    def load_transcript(p: Path) -> list[dict]:
        if p not in transcripts:
            evs = []
            try:
                # Stream line-by-line: transcripts can be tens of MB and
                # only JSON lines are needed; a whole-file read would
                # hold two copies (text + split list) at once.
                with p.open(encoding="utf-8", errors="replace") as f:
                    for line in f:
                        try:
                            evs.append(json.loads(line))
                        except json.JSONDecodeError:
                            continue
            except OSError as e:
                print(f"[warn] cannot read transcript {p}: {e}", file=sys.stderr)
            transcripts[p] = evs
        return transcripts[p]

    occurrences: dict[Path, dict[str, list[tuple[int, str]]]] = {}
    consumed: Counter = Counter()  # (transcript path, digest) -> occurrences used

    serves = []
    for f in fires:
        if f.get("d") != "served" or "h" not in f:
            continue
        row = {
            "t": f.get("t"), "session": f.get("s"), "project": f.get("p"),
            "file": f.get("f"), "digest": f.get("h"), "verdict": "unmatched",
        }
        tp = transcript_path(args.projects_dir, f.get("p", ""), f.get("s", ""))
        if tp is not None:
            events = load_transcript(tp)
            if tp not in occurrences:
                occurrences[tp] = payload_occurrences(events)
            # Fires are time-sorted, so the nth ledger row with this digest in
            # this transcript takes the nth occurrence. A row with no
            # occurrence left stays unmatched — it never inherits another
            # serve's position.
            slot = consumed[(tp, f["h"])]
            hits = occurrences[tp].get(f["h"], [])
            if slot < len(hits):
                consumed[(tp, f["h"])] += 1
                idx, payload = hits[slot]
                tests, coupled = parse_payload(payload)
                if tests or coupled:
                    row["verdict"] = score_serve(events, idx, tests, coupled)
                else:
                    # hotspot-only payload: nothing actionable was named, so
                    # there is no action to detect. Not part of the acted/no-op
                    # denominator.
                    row["verdict"] = "hotspot-only"
        serves.append(row)

    verdicts = Counter(r["verdict"] for r in serves)
    scored_n = sum(verdicts[v] for v in ("acted", "ambiguous", "no-op"))

    # (c) base rate across every transcript of every project in the ledger
    base_edits = base_followed = 0
    for proj in by_project:
        d = args.projects_dir / munge_project(proj)
        if not d.is_dir():
            continue
        for tp in d.glob("*.jsonl"):
            e, fl = base_rate_for_transcript(load_transcript(tp))
            base_edits += e
            base_followed += fl

    z = two_proportion_z(verdicts["acted"], scored_n, base_followed, base_edits)

    report = {
        "ledger_dirs": [str(d) for d in ledger_dirs],
        "fires_total": sum(total.values()),
        "decision_mix": dict(total),
        "by_project": {p: dict(c) for p, c in by_project.items()},
        "sessions": len(by_session),
        "served_scored": {
            "n": len(serves),
            "verdicts": dict(verdicts),
            "acted_rate": round(verdicts["acted"] / scored_n, 3) if scored_n else None,
        },
        "base_rate": {
            "edits": base_edits,
            "followed_by_file_named_test": base_followed,
            "rate": round(base_followed / base_edits, 3) if base_edits else None,
        },
        "z_score": round(z, 2) if z is not None else None,
        "default_on_ready": False,
        "decision": "Controlled A/B or randomized exposure is required; observational rates do not establish efficacy.",
        "serves": serves,
    }

    if args.json:
        print(json.dumps(report, indent=2))
        return 0

    print(f"ledger: {FIRE_LOG} (+ .1 if rotated) under {ledger_label}")
    print(f"fires: {sum(total.values())} across {len(by_session)} sessions, "
          f"{len(by_project)} projects")
    print("decision mix: " + ", ".join(f"{d}={total[d]}" for d in DECISIONS if total[d]))
    print()
    for p, c in sorted(by_project.items()):
        print(f"  {p}: " + ", ".join(f"{d}={c[d]}" for d in DECISIONS if c[d]))
    print()
    print(f"served fires: {len(serves)}")
    for v in ("acted", "ambiguous", "no-op", "hotspot-only", "unmatched"):
        if verdicts[v]:
            print(f"  {v}: {verdicts[v]}")
    if scored_n:
        print(f"  acted rate (of {scored_n} scoreable): {verdicts['acted'] / scored_n:.1%}")
    if base_edits:
        print(f"base rate: {base_followed}/{base_edits} = {base_followed / base_edits:.1%} "
              "(unconditioned file-named-test-after-edit; see README for bias)")
    if z is not None:
        print(f"descriptive two-proportion z = {z:.2f} (non-equivalent populations; not causal evidence)")
    if scored_n < args.min_served:
        print(f"\ndecision rule: not yet evaluable — {scored_n} scoreable serves, "
              f"need >= {args.min_served}.")
    else:
        print(f"\ndecision rule ({args.min_served}+ serves reached): "
              "observational sample available; a controlled A/B or randomized exposure "
              "is still required before default-on.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
