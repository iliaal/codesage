#!/usr/bin/env python3
"""Regression tests for the terminality metric in analyze-codesage-quality.py.

Usage:
  python3 bench/test_analyze_quality.py

Bare-assert style — no pytest dependency. Exits 0 on success, 1 on failure.
Events are built in the exact dict shape `extract_events` emits.
"""

from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
failures: list[str] = []
skipped_blocks: list[str] = []


def _load(filename: str, modname: str):
    spec = importlib.util.spec_from_file_location(modname, HERE / filename)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def check(cond: bool, label: str) -> None:
    if not cond:
        failures.append(f"  {label}")


aq = _load("analyze-codesage-quality.py", "analyze_codesage_quality")
CS = aq.TOOL_PREFIX
ROOT = "/repo"

_counter = 0


def _next_id() -> str:
    global _counter
    _counter += 1
    return f"toolu_{_counter:04d}"


def use(tool: str, **inp):
    return {"kind": "tool_use", "tool": tool, "id": _next_id(), "input": inp, "ts": None}


def result(use_ev: dict, text: str):
    return {
        "kind": "tool_result",
        "id": use_ev["id"],
        "text": text,
        "ts": None,
        "pair_tool": use_ev["tool"],
        "pair_input": use_ev["input"],
    }


def cs_call(tool: str, text: str, **inp) -> list[dict]:
    u = use(CS + tool, project=ROOT, **inp)
    return [u, result(u, text)]


def native(tool: str, **inp) -> list[dict]:
    u = use(tool, **inp)
    return [u, result(u, "ok")]


def user(text: str) -> list[dict]:
    return [{"kind": "user", "text": text, "ts": None}]


FS_RESULT = json.dumps({"results": [{"name": "foo", "file_path": "crates/graph/src/search.rs", "line": 12}]})


def bucket(events, tool="find_symbol", window=3):
    return aq.terminality(events, window)["tools"][tool]


events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Bash", command="cargo test -p graph")
    + native("Edit", file_path="/repo/crates/graph/src/lib.rs", old_string="a", new_string="b")
)
b = bucket(events)
check(b["calls"] == 1 and b["terminal"] == 1 and b["non_terminal"] == 0 and b["chained"] == 0, "terminal: plain")
check(b["top_followups"] == [] and b["terminal_rate"] == 1.0 and b["terminal_rate_strict"] == 1.0, "terminal: rates")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Bash", command="cargo build")
    + native("Grep", pattern="foo", path="/repo")
)
t = aq.terminality(events)
b = t["tools"]["find_symbol"]
check(b["terminal"] == 0 and b["non_terminal"] == 1, "non-terminal: Grep after")
check(b["top_followups"] == [("Grep", 1)], "non-terminal: first follow-up is Grep")
check(t["evidence"] == [{"tool": "find_symbol", "followup": "Grep", "evidence": "foo"}], "non-terminal: evidence row")

events = cs_call("search", "{}", query="where is foo") + native("Glob", pattern="**/*.rs")
b = bucket(events, tool="search")
check(b["non_terminal"] == 1 and b["top_followups"] == [("Glob", 1)], "non-terminal: Glob after")

# Shell retrieval classification.
POSITIVE_BASH = [
    ("rtk proxy grep -rn foo crates/", "grep"),
    ("grep -rn foo", "grep"),  # -r with no path searches the working tree
    ("rg foo", "rg"),
    ("rg foo /repo/crates | head -20", "rg"),  # rg is the pipeline head
    ("grep -E 'a|b' src/", "grep"),  # quoted pipe is not a pipeline
    ("cd /repo && find . -name '*.rs'", "find"),
    ("find crates -path '*queries*' -type f", "find"),
    ("git -C /repo log --oneline -5 -- crates/graph", "git log"),
    ("git log -p -3", "git log"),
    ("git log -S needle --oneline", "git log"),
    ("git log --oneline -3 crates/graph/src/search.rs", "git log"),
    ("git blame crates/graph/src/search.rs", "git blame"),
    ("RUST_LOG=debug rtk proxy git grep foo", "git grep"),
    ("ls crates | xargs grep -l foo", "grep"),
    ("find . -name '*.rs' -print0 | xargs -0 grep -n foo", "find"),
    ("echo $(grep -rn foo crates/)", "grep"),
    ("wc -l $(rg -l foo src)", "rg"),
    ("grep -rn foo /repo/../repo/crates", "grep"),
    ("grep -rn foo crates/ 2>/dev/null > /tmp/hits.txt", "grep"),  # redirects ignored, path still counts
    ("find crates -name '*.rs' | xargs grep -n foo", "find"),
    # explicit pattern flags consumed, paths still judged.
    ("grep -rn -e foo crates/", "grep"),
    ("grep -rn -e foo", "grep"),  # -r with no path: working tree under the root
    ("grep -rne foo crates/", "grep"),
    ("grep --regexp=foo -r crates/", "grep"),
    ("rg -t rust foo", "rg"),
    ("rg -e foo -g '*.rs' crates", "rg"),
    # wrappers with flags / flag arguments.
    ("timeout 30 grep -rn foo crates/", "grep"),
    ("timeout -s KILL 5m rg foo", "rg"),
    ("sudo -u nobody grep -rn foo crates/", "grep"),
    ("env FOO=1 grep -rn foo crates/", "grep"),
    ("env -u HOME rg foo crates", "rg"),
    ("stdbuf -oL rg foo crates", "rg"),
    ("nohup grep -rn foo crates/ &", "grep"),
    ("xvfb-run -a git blame crates/a.rs", "git blame"),
    # cd tracked across statements.
    ("cd /repo && find . -name '*.rs'", "find"),
    ("cd crates && grep -rn foo .", "grep"),
    ("cd /tmp; cd /repo/crates && rg foo", "rg"),
    # pathspecs with a directory component or source extension.
    ("git log --oneline Cargo.toml", "git log"),
    ("git log -3 crates/", "git log"),
    # combined short flags carrying -r with a pattern flag.
    ("grep -rne foo", "grep"),
    ("grep -rn -e foo -e bar crates/", "grep"),
    ("grep -rn crates/ --regexp foo", "grep"),  # --regexp as last pair
    ("grep -r -efoo crates/", "grep"),
    # long wrapper flags with arguments.
    ("sudo --user nobody grep -rn foo crates/", "grep"),
    ("sudo --group wheel rg foo crates", "rg"),
    ("timeout --signal KILL 30 grep -rn foo crates/", "grep"),
    ("env --unset HOME rg foo crates", "rg"),
    ("env -C /repo grep -rn foo .", "grep"),
    ("env --chdir=/repo/crates rg foo", "rg"),
    # git grep parsed like grep.
    ("git grep -n foo_bar", "git grep"),
    # -exec with a read-only utility keeps the find as retrieval.
    ("find crates -name '*.rs' -exec cat {} \\;", "find"),
    ("find crates -name '*.rs' -exec grep -n foo {} \\;", "find"),
    ("find . -name '*.toml' -exec /usr/bin/head -5 {} +", "find"),
    ("find . -name '*.json' -execdir jq . {} \\;", "find"),
    ("find . -name '*.rs' -exec wc -l {} + | sort -n", "find"),
]
for cmd, label in POSITIVE_BASH:
    got = aq.bash_native_retrieval(cmd, ROOT)
    check(got == label, f"bash positive {cmd!r}: expected {label}, got {got}")

NEGATIVE_BASH = [
    "cargo test 2>&1 | grep -E 'error:'",  # pipe consumer is a filter
    "php -m | grep -i json",
    "valgrind ./bin 2>&1 | grep -v Warning | grep 'definitely lost'",
    "cargo build; echo done | grep done",
    "grep -n foo /tmp/claude-1000/tasks/abc.output",  # scratch/task output
    "grep -c error /proc/self/status",
    "rg panic /var/log/syslog",
    "grep -n ERROR daemon.log",
    "grep foo /home/other/project/src/a.rs",  # outside the project root
    "grep foo",  # stdin filter, no -r
    "find . -type f -newer Cargo.toml",  # find without a name/path predicate
    "find /tmp -name '*.sock'",
    # a find that acts on its matches is housekeeping, not retrieval.
    "find . -name __pycache__ -delete",
    "find crates -name '*.orig' -exec rm {} +",
    "find . -name '*.rs' -execdir touch {} \\;",
    "find . -name '*.log' -ok rm {} \\;",
    "find . -name '*.rs' -fprint /tmp/list.txt",
    "find . -name '*.rs' -exec sed -i 's/a/b/' {} \\;",  # sed is not read-only
    "find . -name '*.rs' -exec awk '{print}' {} \\;",
    "find . -name '*.rs' -exec /bin/mv {} /tmp/ \\;",
    # Keep the escaped semicolon within find so the trailing mutation is checked.
    "find crates -name '*.rs' -exec cat {} \\; -delete",
    "find crates -name '*.rs' -exec grep -l foo {} \\; -exec rm {} \\;",
    'until [ -x bin/php ] || grep -q "^rc=" /tmp/claude-1000/tasks/b3.output 2>/dev/null; do sleep 10; done',
    "grep -q foo /tmp/x.txt 2> /dev/null",  # two-token redirect
    "grep -q foo /tmp/x.txt > out.txt 2>&1",
    "cat /tmp/list.txt | xargs grep -l foo",  # xargs fed from scratch
    "ls /tmp/x | xargs -0 rg foo",
    # the pattern token is never a path.
    "grep -rn -e panic /var/log/syslog",
    "grep -q -e foo /tmp/x.txt",
    "grep --regexp panic /var/log/syslog",
    "grep --regexp=panic /var/log/syslog",
    "grep -f /tmp/patterns.txt /var/log/syslog",
    "grep -e foo",  # stdin, no -r
    "grep -rn 'fn x' -A 25 /tmp/task.output",  # -A's argument is not a path
    "rg -g '*.rs' foo /tmp/x",
    "grep foo /tmp/x.txt &",  # trailing & is not a path
    # wrapper flag arguments are not command words or paths.
    "sudo -u nobody cat crates/a.rs",
    "timeout 30 cargo test",
    "env FOO=1 cargo build",
    # cd into scratch, then a relative search.
    "cd /tmp && grep -rn foo .",
    "cd /tmp && find . -name '*.sock'",
    "cd /tmp/work; rg foo",
    "cd /other/project && grep -rn foo src/",
    # no -r in the combined token means stdin.
    "grep -ne foo",
    "grep -n -e foo -e bar",
    # pushd/popd and subshells lose the working directory.
    "pushd /tmp && grep foo src/",
    "pushd /repo && grep -rn foo src/",
    "popd; grep -rn foo src/",
    "( cd /tmp && grep foo src/ )",
    "(cd /repo && grep -rn foo src/)",
    "cd /repo; ( grep -rn foo src/ )",
    # env -C into scratch.
    "env -C /tmp grep -rn foo .",
    # slash-shaped and operator refs are not pathspecs.
    "git log origin/master",
    "git log -3 upstream/main",
    "git log refs/heads/master --oneline",
    "git log HEAD~2",
    "git log HEAD^ --stat",
    "git log @{u} --oneline",
    "git log master..origin/master",
    # refs are not pathspecs.
    "git log v0.26.1",
    "git log main..HEAD --oneline",
    "git log 1.2.3 -3",
    "git log --oneline -5",  # git log without pathspec or -p/-S/-G
    "git status",
    "git diff --stat",
    "cargo test",
    "ls crates/",
    "python3 bench/x.py",
    "echo 'grep -rn foo crates/'",  # quoted string, not a command
    "cat <<'EOF' > /tmp/x.sh\ngrep -rn foo crates/\nEOF",  # heredoc body
    "python3 - <<EOF\nimport os\nprint(os.popen('rg foo src').read())\nEOF",
    "",
]
for cmd in NEGATIVE_BASH:
    got = aq.bash_native_retrieval(cmd, ROOT)
    check(got is None, f"bash negative {cmd!r}: got {got}")
check(aq.bash_native_retrieval(None, ROOT) is None, "bash_native_retrieval: non-string")
check(aq.shell_retrieval("rtk proxy grep -rn foo_bar crates/", ROOT) == ("grep", "foo_bar"), "shell_retrieval: pattern")
check(aq.shell_retrieval("grep -rn -e needle crates/", ROOT) == ("grep", "needle"), "shell_retrieval: -e pattern")
check(aq.shell_retrieval("rg --regexp=needle crates", ROOT) == ("rg", "needle"), "shell_retrieval: --regexp= pattern")
check(aq.shell_retrieval("find . -name '*.rs'", ROOT) == ("find", None), "shell_retrieval: find has no pattern")
check(aq.shell_retrieval("cargo test | grep error", ROOT) == (None, None), "shell_retrieval: filter")
check(aq.shell_retrieval("git grep -n foo_bar -- crates", ROOT) == ("git grep", "foo_bar"), "shell_retrieval: git grep pattern")
check(aq.shell_retrieval("git -C /repo grep -e needle", ROOT) == ("git grep", "needle"), "shell_retrieval: git grep -e pattern")
check(aq.shell_retrieval("ls crates | xargs grep -l foo_bar", ROOT) == ("grep", "foo_bar"), "shell_retrieval: xargs grep pattern")
check(aq.shell_retrieval("grep -rn -e first -e second crates/", ROOT) == ("grep", "first"), "shell_retrieval: -e twice keeps the first")
check(aq.shell_retrieval("grep -rn crates/ --regexp last_one", ROOT) == ("grep", "last_one"), "shell_retrieval: --regexp as last pair")
check(aq.shell_retrieval("grep -r -efoo_bar crates/", ROOT) == ("grep", "foo_bar"), "shell_retrieval: attached -efoo")
check(aq.shell_retrieval("grep -rn -f /tmp/pats.txt crates/", ROOT) == ("grep", ""), "shell_retrieval: -f FILE has no pattern")
check(aq.shell_retrieval("git grep -f pats.txt", ROOT) == ("git grep", ""), "shell_retrieval: git grep -f has no pattern")
check(aq.shell_retrieval("git blame crates/a.rs", ROOT) == ("git blame", None), "shell_retrieval: blame has no pattern")
check(aq.last_absolute_cd("cd /other/project && ls") == "/other/project", "last_absolute_cd: absolute")
check(aq.last_absolute_cd("cd /a; cd /b/../c") == "/c", "last_absolute_cd: last one wins, normalized")
check(aq.last_absolute_cd("cd /a && cd sub") is None, "last_absolute_cd: relative after absolute is unknown")
check(aq.last_absolute_cd("cd /a; popd") is None, "last_absolute_cd: popd is unknown")
check(aq.last_absolute_cd("(cd /a)") is None, "last_absolute_cd: subshell cd does not leak")
check(aq.last_absolute_cd("cargo test") is None, "last_absolute_cd: no cd")
check(aq.last_absolute_cd("pushd /a && make") == "/a", "last_absolute_cd: pushd absolute")
check(aq.bash_native_retrieval("grep foo /repo/crates", None) is None, "bash: absolute path needs a project root")
check(aq.bash_native_retrieval("grep foo crates", None) == "grep", "bash: relative path needs no project root")

events = cs_call("find_symbol", FS_RESULT, name="foo") + native("Bash", command="cargo test 2>&1 | grep error")
check(bucket(events)["terminal"] == 1, "scorer: pipe-consumer grep is neutral")
events = cs_call("find_symbol", FS_RESULT, name="foo") + native("Bash", command="rtk proxy grep -rn foo crates/")
t = aq.terminality(events)
check(t["tools"]["find_symbol"]["top_followups"] == [("Bash(grep)", 1)], "scorer: Bash(grep) label")
check(t["evidence"][0]["evidence"] == "rtk proxy grep -rn foo crates/", "scorer: Bash evidence is the command")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Read", file_path="/repo/crates/cli/src/main.rs")
    + native("Edit", file_path="/repo/crates/cli/src/main.rs", old_string="a", new_string="b")
)
b = bucket(events)
check(b["terminal"] == 1 and b["policy_read"] == 1, "policy-read: Read then Edit same path")
check(b["follow_through_read"] == 0, "policy-read: not counted as follow-through")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Read", file_path="/repo/crates/cli/src/main.rs")
    + native("MultiEdit", file_path="/repo/crates/cli/src/main.rs", edits=[])
)
check(bucket(events)["policy_read"] == 1, "policy-read: MultiEdit is an edit")

events = cs_call("find_symbol", FS_RESULT, name="foo") + native(
    "Read", file_path="/repo/crates/cli/src/main.rs"
)
t = aq.terminality(events)
b = t["tools"]["find_symbol"]
check(b["non_terminal"] == 1 and b["policy_read"] == 0, "unexplained Read: non-terminal")
check(b["top_followups"] == [("Read", 1)], "unexplained Read: first follow-up is Read")
check(t["evidence"][0]["evidence"] == "/repo/crates/cli/src/main.rs", "unexplained Read: evidence is the path")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Read", file_path="/repo/crates/cli/src/main.rs")
    + native("Write", file_path="/repo/crates/cli/src/other.rs", content="x")
)
check(bucket(events)["non_terminal"] == 1, "policy-read: different path does not excuse")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Read", file_path="/repo/crates/cli/src/main.rs")
    + native("Bash", command="cargo build")
    + native("Bash", command="cargo build")
    + native("Edit", file_path="/repo/crates/cli/src/main.rs", old_string="a", new_string="b")
)
check(bucket(events)["non_terminal"] == 1, "policy-read: Edit past the window does not excuse")
check(bucket(events, window=4)["policy_read"] == 1, "policy-read: Edit inside window=4 excuses")

events = cs_call("find_symbol", FS_RESULT, name="foo") + native(
    "Read", file_path="/repo/crates/graph/src/search.rs"
)
b = bucket(events)
check(b["terminal"] == 1 and b["follow_through_read"] == 1, "follow-through: Read of result path")
check(b["policy_read"] == 0, "follow-through: not counted as policy-read")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Read", file_path="/repo/crates/graph/src/search.rs")
    + native("Edit", file_path="/repo/crates/graph/src/search.rs", old_string="a", new_string="b")
)
b = bucket(events)
check(b["follow_through_read"] == 1 and b["policy_read"] == 0, "follow-through precedence over policy-read")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Read", file_path="/repo/crates/graph/src/search.rs")
    + native("Grep", pattern="foo")
)
b = bucket(events)
check(b["non_terminal"] == 1 and b["follow_through_read"] == 1, "follow-through then Grep: non-terminal, still counted")

# Only structured path fields justify follow-through reads; snippets do not.
SEARCH_RESULT = json.dumps({
    "results": [
        {
            "file_path": "crates/parser/src/lib.rs",
            "content": "// see Cargo.toml and crates/parser/src/queries/rust.scm\ninclude_str!(\"queries/rust.scm\")",
            "symbols": ["load_query"],
        }
    ]
})
events = cs_call("search", SEARCH_RESULT, query="query loading") + native("Read", file_path="/repo/Cargo.toml")
b = bucket(events, tool="search")
check(b["non_terminal"] == 1 and b["follow_through_read"] == 0, "follow-through: Cargo.toml in a snippet is NOT excused")
events = cs_call("search", SEARCH_RESULT, query="query loading") + native(
    "Read", file_path="/repo/crates/parser/src/queries/rust.scm"
)
b = bucket(events, tool="search")
check(b["non_terminal"] == 1 and b["follow_through_read"] == 0, "follow-through: queries/rust.scm in a snippet is NOT excused")
events = cs_call("search", SEARCH_RESULT, query="query loading") + native("Read", file_path="/repo/crates/parser/src/lib.rs")
check(bucket(events, tool="search")["follow_through_read"] == 1, "follow-through: structured file_path IS excused")

paths = aq.extract_result_paths(json.dumps({"files": ["Cargo.toml", "crates/a.rs"], "max_risk_file": "crates/b.rs",
                                            "top_risk_files": [{"file": "crates/c.rs"}], "note": "see docs/x.md"}))
check(paths == {"Cargo.toml", "crates/a.rs", "crates/b.rs", "crates/c.rs"}, f"extract_result_paths: structured keys {paths}")
check(aq.read_hits_result("/repo/Cargo.toml", paths, ROOT), "read_hits_result: root Cargo.toml matches exactly")
check(not aq.read_hits_result("/repo/crates/x/Cargo.toml", paths, ROOT), "read_hits_result: nested Cargo.toml does not")
check(not aq.read_hits_result("/other/crates/a.rs", paths, ROOT), "read_hits_result: same suffix outside root does not")
check(aq.read_hits_result("/repo/crates/./a.rs", paths, ROOT), "read_hits_result: normalized")
check(aq.read_hits_result("/abs/repo/crates/a.rs", {"crates/a.rs"}), "read_hits_result: suffix without root")
check(not aq.read_hits_result("/abs/repo/xcrates/a.rs", {"crates/a.rs"}), "read_hits_result: component boundary")
check(not aq.read_hits_result("/abs/repo/x/Cargo.toml", {"Cargo.toml"}), "read_hits_result: bare basename never suffix-matches")

paths = aq.extract_result_paths("hit at ./crates/a.rs:12 (score 0.35, v0.26.1); also Cargo.toml e.g. foo")
check(paths == {"crates/a.rs"}, f"extract_result_paths: text fallback {paths}")
check(aq.extract_result_paths("") == set() and aq.extract_result_paths("[]") == set(), "extract_result_paths: empty")

# Broken JSON must not fall back to extracting paths from snippets.
fenced = "```json\n" + json.dumps({"results": [{"file_path": "crates/a.rs"}]}) + "\n```"
check(aq.extract_result_paths(fenced) == {"crates/a.rs"}, "extract_result_paths: fenced JSON")
prose = "Found 1 result:\n" + json.dumps({"results": [{"file_path": "crates/a.rs"}]}) + "\nDone."
check(aq.extract_result_paths(prose) == {"crates/a.rs"}, "extract_result_paths: prose-prefixed JSON")
broken = '{"results": [{"file_path": "crates/a.rs", "content": "see crates/parser/src/queries/rust.scm"'
check(aq.extract_result_paths(broken) == set(), "extract_result_paths: truncated JSON yields nothing")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + cs_call("find_references", json.dumps({"results": [{"file_path": "crates/x.rs"}]}), name="foo")
    + native("Grep", pattern="foo")
)
t = aq.terminality(events)
b1 = t["tools"]["find_symbol"]
check(b1["chained"] == 1 and b1["terminal"] == 0 and b1["non_terminal"] == 0, "window cut: first call chained")
check(b1["terminal_rate"] == 0.0 and b1["terminal_rate_strict"] is None, "window cut: rates with chained only")
check(t["tools"]["find_references"]["non_terminal"] == 1, "window cut: second call non-terminal")
check(t["all"]["calls"] == 2 and t["all"]["chained"] == 1 and t["all"]["non_terminal"] == 1, "window cut: overall row")
events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Bash", command="cargo build")
    + native("Bash", command="cargo build")
    + native("Bash", command="cargo build")
    + cs_call("search", "{}", query="q")
)
check(bucket(events)["terminal"] == 1 and bucket(events)["chained"] == 0, "window exhausted before codesage: terminal")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("TodoWrite", todos=[])
    + native("AskUserQuestion", questions=[])
    + native("Agent", prompt="x")
    + native("Skill", skill="x")
    + native("Monitor", command="x")
    + native("ListAgents")
    + native("SendMessage", to="x", message="y")
    + native("Grep", pattern="foo")
)
check(bucket(events)["non_terminal"] == 1, "non-slot tools: Grep after seven of them is still inside the window")
events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("ToolSearch", query="x")
    + native("TaskOutput", task_id="x")
    + native("TaskStop", task_id="x")
    + native("TaskUpdate", task_id="x")
    + native("Grep", pattern="foo")
)
check(bucket(events)["non_terminal"] == 1, "non-slot tools: ToolSearch/TaskOutput/TaskStop/TaskUpdate do not consume slots")
events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Bash", command="cargo build")
    + native("Bash", command="cargo build")
    + native("Bash", command="cargo build")
    + native("Grep", pattern="foo")
)
check(bucket(events)["terminal"] == 1, "non-slot tools: Bash still consumes a slot")

# Injected role=user envelopes must not end the question's retrieval window.
for injected in [
    "<system-reminder>\nThe TodoWrite tool hasn't been used recently.\n</system-reminder>",
    "<task-notification>\n<task-id>abc</task-id>\n<status>completed</status>\n</task-notification>",
    "<command-name>/codesage-reindex</command-name>\n<command-message>reindex</command-message>",
    "x" * 2001,
]:
    events = cs_call("find_symbol", FS_RESULT, name="foo") + user(injected) + native("Grep", pattern="foo")
    check(bucket(events)["non_terminal"] == 1, f"injected user envelope does not cut: {injected[:20]!r}")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + user("now something else: where is bar configured?")
    + native("Grep", pattern="bar")
)
b = bucket(events)
check(b["terminal"] == 1 and b["non_terminal"] == 0, "user turn cuts the window")
events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Bash", command="cargo build")
    + user("next")
    + native("Grep", pattern="bar")
    + native("Grep", pattern="baz")
)
check(bucket(events)["terminal"] == 1, "user turn cuts the window after a neutral call")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Bash", command="cargo build")
    + native("Bash", command="cargo build")
    + native("Bash", command="cargo build")
    + native("Grep", pattern="foo")
)
check(bucket(events, window=3)["terminal"] == 1, "window N=3: 4th call not inspected")
check(bucket(events, window=4)["non_terminal"] == 1, "window N=4: 4th call inspected")
check(aq.terminality(events, 4)["window"] == 4, "window recorded in result")

events = cs_call("find_symbol", "[]", name="foo") + native("Read", file_path="/repo/crates/graph/src/search.rs")
b = bucket(events)
check(b["non_terminal"] == 1 and b["follow_through_read"] == 0, "empty result: no follow-through")

u = use(CS + "search", project=ROOT, query="foo")
b = bucket([u, use("Grep", pattern="foo")], tool="search")
check(b["calls"] == 1 and b["non_terminal"] == 1, "pending result: still scored")

no_id = {"kind": "tool_use", "tool": CS + "search", "id": None, "input": {"project": ROOT, "query": "q"}, "ts": None}
stray = {"kind": "tool_result", "id": None, "text": FS_RESULT, "ts": None, "pair_tool": None, "pair_input": {}}
b = bucket([no_id, stray] + native("Read", file_path="/repo/crates/graph/src/search.rs"), tool="search")
check(b["calls"] == 1 and b["non_terminal"] == 1 and b["follow_through_read"] == 0, "missing id: scored, no pairing")

events = cs_call("find_symbol", FS_RESULT, name="foo") + native("Read", file_path=None)
check(bucket(events)["terminal"] == 1, "Read without a string path is neutral")

events = []
for tool_name, cnt in [("Grep", 3), ("Glob", 2), ("Read", 1)]:
    for _ in range(cnt):
        events += cs_call("search", "{}", query="q") + native(tool_name, pattern="x", file_path="/r/z.rs")
b = bucket(events, tool="search")
check(b["top_followups"] == [("Grep", 3), ("Glob", 2), ("Read", 1)], "top follow-ups ranked")

events = []
labelled = [
    ("Grep", native("Grep", pattern="x")),
    ("Glob", native("Glob", pattern="x")),
    ("Read", native("Read", file_path="/r/z.rs")),
    ("Bash(grep)", native("Bash", command="grep -rn foo crates/")),
    ("Bash(rg)", native("Bash", command="rg foo")),
    ("Bash(find)", native("Bash", command="find . -name '*.rs'")),
    ("Bash(git blame)", native("Bash", command="git blame crates/a.rs")),
]
for idx, (_label, follow) in enumerate(labelled):
    for _ in range(len(labelled) - idx):  # Grep×7, Glob×6, ... git blame×1
        events += cs_call("search", "{}", query="q") + follow
b = bucket(events, tool="search")
check(len(b["top_followups"]) == 5, f"top follow-ups: exactly 5 retained ({len(b['top_followups'])})")
check(
    b["top_followups"] == [("Grep", 7), ("Glob", 6), ("Read", 5), ("Bash(grep)", 4), ("Bash(rg)", 3)],
    f"top follow-ups: expected set {b['top_followups']}",
)
check(len(b["first_followups"]) == 7, "top follow-ups: all 7 labels kept in the raw counter")

t1 = aq.terminality(cs_call("search", "{}", query="q") + native("Grep", pattern="x"))
t2 = aq.terminality(cs_call("search", "{}", query="q") + native("Glob", pattern="x")
                    + cs_call("search", "{}", query="q") + native("Grep", pattern="y")
                    + cs_call("search", "{}", query="q") + cs_call("search", "{}", query="q"))
merged = aq.merge_terminality([t1, t2], 3)
s = merged["tools"]["search"]
check(s["calls"] == 5 and s["non_terminal"] == 3 and s["chained"] == 1 and s["terminal"] == 1, "merge: counts folded")
check(s["top_followups"] == [("Grep", 2), ("Glob", 1)], "merge: follow-ups re-ranked")
check(abs(s["terminal_rate"] - 0.2) < 1e-9 and abs(s["terminal_rate_strict"] - 0.25) < 1e-9, "merge: both rates")
check(merged["all"]["calls"] == 5 and len(merged["evidence"]) == 3, "merge: overall row and evidence")

agg = aq.aggregate([cs_call("find_symbol", FS_RESULT, name="foo") + native("Grep", pattern="foo")],
                   terminality_window=5)
check(agg["terminality"]["window"] == 5, "aggregate: window threaded through")
report = aq.render(agg, window_days=7, transcripts=3, now="now", subagent_transcripts=2)
check("## Terminality per CodeSage tool (native retrieval within 5 calls)" in report, "render: heading")
check("| `find_symbol` | 1 | 0 | 1 | 0 | 0.0% | 0.0% | 0 | 0 | `Grep`×1 |" in report, "render: tool row")
check("| **all** | 1 | 0 | 1 | 0 | 0.0% | 0.0% | 0 | 0 | `Grep`×1 |" in report, "render: all row")
check("- `find_symbol` → `Grep`: `foo`" in report, "render: evidence line")
check("Unscoped calls (no `project` argument; shell paths scored conservatively): 0 of 1." in report, "render: unscoped footer")
check("Grep tool + shell greps" in report, "render: grep section label")

no_root = use(CS + "search", query="q")
t = aq.terminality([no_root, result(no_root, "{}")] + native("Bash", command="grep -rn foo /repo/crates"))
check(t["all"]["unscoped"] == 1 and t["all"]["terminal"] == 1, "unscoped: counted and absolute path scored conservatively")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Bash", command="rtk proxy grep -rn foo_bar crates/")  # identifier-shaped shell grep
    + native("Bash", command="rtk proxy grep -rn foo crates/")  # follow-up on the codesage subject
    + native("Bash", command="rtk proxy grep -rn search crates/")  # second follow-up on the same result
    + native("Bash", command="cargo test 2>&1 | grep -E 'error:'")  # filter, not counted
    + native("Grep", pattern="baz_qux")
)
agg = aq.aggregate([events])
check(agg["grep_calls"] == 4 and agg["grep_tool_calls"] == 1 and agg["shell_grep_calls"] == 3, "aggregate: shell greps counted")
check(agg["grep_identifier_shaped"] == 4, "aggregate: shell grep patterns classified")
check(agg["followup_grep_after_codesage"] >= 1, "aggregate: shell grep follow-up on codesage subject")
check(agg["identifier_grep_in_codesage_sessions"] == 4, "aggregate: tool-selection denominator includes shell greps")
check(agg["followup_grep_after_codesage"] == 1 and agg["followup_grep_events"] == 2, f"aggregate: one follow-up per result {agg['followup_grep_after_codesage']}/{agg['followup_grep_events']}")
followup_report = aq.render(agg, window_days=7, transcripts=1, now="now")
check("followed by at least one Grep on a mentioned identifier within 5 tool_uses**: 1 (100.0%)" in followup_report, "render: follow-up percentage capped by results")
check("**Follow-up greps per followed result** (mean): 2.00 (2 greps)" in followup_report, "render: follow-up mean")
check("shell greps with no recoverable pattern" in followup_report and "session-scoped" in followup_report, "render: unparsed note and session-scoped rule")

# Pattern-file greps have no identifier denominator; cd re-roots later greps.
events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Bash", command="git grep -n alpha_one")
    + native("Bash", command="ls crates | xargs grep -l beta_two")
    + native("Bash", command="grep -rn -f /tmp/pats.txt crates/")
    + native("Bash", command="cd /other/project && grep -rn gamma_three src/")
    + native("Bash", command="grep -rn delta_four lib/")  # cwd persisted at /other/project
    + native("Bash", command="grep -rn epsilon_five /repo/crates")  # absolute, outside the new root
)
agg = aq.aggregate([events])
# gamma_three and delta_four run under /other/project, which is neither a
# session `project` nor onboarded on disk: detected (all) but not gated.
check(agg["shell_grep_calls"] == 2 and agg["shell_grep_calls_all"] == 4 and agg["shell_grep_unparsed"] == 1,
      f"aggregate e2e: {agg['shell_grep_calls']} gated / {agg['shell_grep_calls_all']} all / {agg['shell_grep_unparsed']} unparsed")
check(agg["grep_identifier_shaped"] == 2, f"aggregate e2e: identifier-shaped {agg['grep_identifier_shaped']}")
check(agg["grep_calls"] == 2, "aggregate e2e: unparsed and non-onboarded greps excluded from the denominator")
check(agg["identifier_grep_in_codesage_sessions_all"] == 4, "aggregate e2e: ungated denominator keeps them")

# Use $HOME: /tmp is rejected before the gate, and this repo would supply
# an inherited index.db, preventing the non-onboarded control.
_home = Path.home()
_home_onboarded = any((p / ".codesage" / "index.db").is_file() for p in [_home, *_home.parents])
if _home_onboarded:
    skipped_blocks.append("$HOME onboarded")
else:
    with tempfile.TemporaryDirectory(dir=_home, prefix=".codesage-gate-fixture-") as td:
        onboarded = Path(td) / "onboarded"
        (onboarded / ".codesage").mkdir(parents=True)
        (onboarded / ".codesage" / "index.db").write_bytes(b"")
        plain = Path(td) / "plain"
        plain.mkdir()
        events = (
            cs_call("find_symbol", FS_RESULT, name="foo")
            + native("Bash", command="grep -rn alpha_one crates/")  # session project root
            + native("Bash", command=f"cd {plain} && grep -rn beta_two src/")  # non-onboarded
            + native("Bash", command=f"cd {onboarded} && grep -rn gamma_three src/")  # index.db present
            + native("Bash", command="git grep -n delta_four")  # cwd persisted at the onboarded dir
            + native("Bash", command=f"cd {plain} && git grep -n epsilon_five")  # git grep outside
        )
        agg = aq.aggregate([events])
        check(agg["shell_grep_calls"] == 3 and agg["shell_grep_calls_all"] == 5, f"gate: {agg['shell_grep_calls']} gated / {agg['shell_grep_calls_all']} all")
        check(agg["identifier_grep_in_codesage_sessions"] == 3, "gate: gated denominator")
        check(agg["identifier_grep_in_codesage_sessions_all"] == 5, "gate: ungated denominator")
        check(agg["grep_calls"] == 3, "gate: grep_calls uses the gated count")
        gate_report = aq.render(agg, window_days=7, transcripts=1, now="now")
        check("**25.0%** (1 CodeSage / 4 total" in gate_report, "render: gated rate")
        check("including non-onboarded roots: rate 16.7% (5 identifier-shaped greps in CodeSage sessions)" in gate_report, "render: ungated line")
        check("**Grep tool + shell grep calls observed (onboarded roots)**: 3 (Grep tool 0, shell greps 3); all roots: 5 (Grep tool 0, shell greps 5)" in gate_report, "render: header shows gated and all-roots counts")
        check("Excluded from the denominator below: 0 Grep tool calls and 2 shell greps under non-onboarded roots, 0 shell greps with no recoverable pattern" in gate_report, "render: named exclusions")

        (onboarded / "crates" / "graph").mkdir(parents=True)
        (onboarded / ".claude" / "worktrees" / "agent-x").mkdir(parents=True)
        events = (
            cs_call("find_symbol", FS_RESULT, name="foo")
            + native("Bash", command=f"cd {onboarded}/crates/graph && grep -rn sub_one src/")
            + native("Bash", command=f"cd {onboarded}/.claude/worktrees/agent-x && rg sub_two crates")
            + native("Bash", command="cd /repo/crates/cli && grep -rn sub_three src/")  # under the session project
        )
        agg = aq.aggregate([events])
        check(agg["shell_grep_calls"] == 3 and agg["shell_grep_calls_all"] == 3, f"gate walk-up: {agg['shell_grep_calls']} of {agg['shell_grep_calls_all']}")

events = cs_call("find_symbol", FS_RESULT, name="foo") + native("Bash", command="grep -rn foo crates/ && cd /nowhere")
agg = aq.aggregate([events])
check(agg["shell_grep_calls"] == 1, "per-statement root: grep before a cd to nowhere counts")
events = cs_call("find_symbol", FS_RESULT, name="foo") + native("Bash", command="cd /nowhere && grep -rn foo crates/")
agg = aq.aggregate([events])
check(agg["shell_grep_calls_all"] == 1 and agg["shell_grep_calls"] == 0, "per-statement root: grep after cd /nowhere is re-rooted there and not onboarded")
check(aq.shell_retrieval_detail("grep -rn foo crates/ && cd /nowhere", ROOT, root_follows_cd=True) == ("grep", "foo", ROOT), "shell_retrieval_detail: root of the winning statement")
check(aq.shell_retrieval_detail("cd /repo/x && grep -rn foo .", "/elsewhere", root_follows_cd=True) == ("grep", "foo", "/repo/x"), "shell_retrieval_detail: re-rooted by an absolute cd")
check(aq.shell_retrieval_detail("cd /repo/x && grep -rn foo .", "/elsewhere") == (None, None, None), "shell_retrieval_detail: fixed root without root_follows_cd")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Bash", command="cd /elsewhere/proj")
    + native("Bash", command="grep -rn foo src/")
)
check(aq.aggregate([events])["shell_grep_calls_all"] == 1, "cwd carry-over: main session adopts the cd")
check(aq.aggregate([events], subagent_flags=[True])["shell_grep_calls_all"] == 1, "cwd carry-over: subagent starts at the session root, src/ still under /repo")
events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Bash", command="cd /elsewhere/proj")
    + native("Bash", command="grep -rn foo /elsewhere/proj/src")
)
check(aq.aggregate([events])["shell_grep_calls_all"] == 1, "cwd carry-over: absolute path under the carried root counts (all)")
check(aq.aggregate([events], subagent_flags=[True])["shell_grep_calls_all"] == 0, "cwd carry-over: subagent does not carry the cd")

events = (
    user("where is foo_bar defined?")
    + native("TodoWrite", todos=[])
    + native("Bash", command="cargo build")
    + native("Grep", pattern="foo_bar")
    + cs_call("find_symbol", FS_RESULT, name="foo")
)
shape = aq.aggregate([events])["question_shape_counts"]["identifier"]
check(shape["first_grep"] == 1 and shape["first_other_or_none"] == 0, f"shape walk: skips TodoWrite and cargo build {shape}")
events = user("where is foo_bar defined?") + native("Bash", command="cargo build") + cs_call("find_symbol", FS_RESULT, name="foo")
shape = aq.aggregate([events])["question_shape_counts"]["identifier"]
check(shape["first_codesage_retrieval"] == 1, f"shape walk: non-retrieval Bash is skipped, codesage call is the decision {shape}")
events = user("where is foo_bar defined?") + native("Bash", command="find . -name '*.rs'") + cs_call("find_symbol", FS_RESULT, name="foo")
shape = aq.aggregate([events])["question_shape_counts"]["identifier"]
check(shape["first_glob"] == 1, f"shape walk: shell find is first=Glob {shape}")
for cmd in ["git log -p -3 -- crates/", "git blame crates/graph/src/search.rs"]:
    events = user("where is foo_bar defined?") + native("Bash", command=cmd) + cs_call("find_symbol", FS_RESULT, name="foo")
    shape = aq.aggregate([events])["question_shape_counts"]["identifier"]
    check(shape["first_grep"] == 0 and shape["first_codesage_retrieval"] == 1, f"shape walk: {cmd!r} is not a decision {shape}")
events = user("where is foo_bar defined?") + native("Bash", command="git grep -n foo_bar") + cs_call("find_symbol", FS_RESULT, name="foo")
check(aq.aggregate([events])["question_shape_counts"]["identifier"]["first_grep"] == 1, "shape walk: git grep is first=Grep")
report_501 = aq.render(aq.aggregate([events]), window_days=7, transcripts=1, now="now")
check("shell grep (`grep`/`rg`/`git grep`, or `find -exec grep` with a recovered pattern) is filed under `first=Grep` and any other retrieval `find` under `first=Glob`" in report_501, "render: §2.3 legend names the Bash mapping")

events = (
    cs_call("find_symbol", FS_RESULT, name="foo")
    + native("Grep", pattern="alpha_one", path="/other/project/src")  # absolute, not onboarded
    + native("Grep", pattern="beta_two", path="/repo/crates")  # absolute, under the session project
    + native("Grep", pattern="gamma_three", path="crates")  # relative → session root
    + native("Grep", pattern="delta_four")  # absent → session root
)
agg = aq.aggregate([events])
check(agg["grep_tool_calls"] == 3 and agg["grep_tool_calls_all"] == 4, f"Grep tool gate: {agg['grep_tool_calls']} of {agg['grep_tool_calls_all']}")
check(agg["grep_identifier_shaped"] == 3 and agg["identifier_grep_in_codesage_sessions_all"] == 4, "Grep tool gate: identifier counters")
check(agg["grep_calls"] == 3, "Grep tool gate: grep_calls uses the gated count")
report_502 = aq.render(agg, window_days=7, transcripts=1, now="now")
check("(onboarded roots)**: 3 (Grep tool 3, shell greps 0); all roots: 4 (Grep tool 4, shell greps 0)" in report_502, "render: header carries both Grep tool halves")
check("Excluded from the denominator below: 1 Grep tool calls and 0 shell greps under non-onboarded roots" in report_502, "render: Grep tool exclusion named")
# Expand both tilde roots and paths before deciding whether the grep is onboarded.
events = cs_call("find_symbol", FS_RESULT, name="foo") + native("Grep", pattern="foo_bar", path="~/other-repo")
agg = aq.aggregate([events])
check(agg["grep_tool_calls"] == 0 and agg["grep_tool_calls_all"] == 1, "Grep tool gate: tilde path outside the project is not onboarded")
tilde_call = use(CS + "find_symbol", project="~/tilde-proj", name="foo")
events = [tilde_call, result(tilde_call, FS_RESULT)] + native("Grep", pattern="foo_bar", path="~/tilde-proj/src")
agg = aq.aggregate([events])
check(agg["grep_tool_calls"] == 1, "Grep tool gate: tilde session project matches a tilde path under it")
events = [tilde_call, result(tilde_call, FS_RESULT)] + native("Bash", command=f"grep -rn foo_bar {Path.home()}/tilde-proj/src")
check(aq.aggregate([events])["shell_grep_calls"] == 1, "shell grep gate: expanded absolute path matches a tilde session project")
agg = aq.aggregate([native("Grep", pattern="foo")])
check(agg["grep_tool_calls"] == 0 and agg["grep_tool_calls_all"] == 1, "Grep tool gate: no session root → not a decision")

check(aq.shell_retrieval_detail("env -C crates rg foo", ROOT, root_follows_cd=True) == ("rg", "foo", ROOT), "env -C relative keeps the root")
check(aq.shell_retrieval_detail("env -C /repo/crates rg foo", ROOT, root_follows_cd=True) == ("rg", "foo", "/repo/crates"), "env -C absolute re-roots")
check(aq.shell_retrieval_detail("env --chdir=crates rg foo", ROOT, root_follows_cd=True) == ("rg", "foo", ROOT), "env --chdir= relative keeps the root")
check(aq.shell_retrieval_detail("env --chdir=/repo/crates rg foo", ROOT, root_follows_cd=True) == ("rg", "foo", "/repo/crates"), "env --chdir= absolute re-roots")

check(aq.shell_retrieval("find crates -name '*.rs' -exec grep -n degree {} \\; ; echo done; grep -rn other_pat crates/", ROOT) == ("find", "degree"), "find -exec grep keeps its own pattern")
check(aq.shell_retrieval("find crates -name '*.rs' -exec grep -l -e degree {} +", ROOT) == ("find", "degree"), "find -exec grep -e pattern")
check(aq.shell_retrieval("find crates -name '*.rs' -exec cat {} \\;", ROOT) == ("find", None), "find -exec cat has no pattern")
check(aq.shell_retrieval("find crates -name '*.rs' -exec grep -A 3 {} \\;", ROOT) == ("find", None), "find -exec grep with no pattern: {} is not a pattern")
check(aq.shell_retrieval("find crates -name '*.rs' -exec grep -n {} +", ROOT) == ("find", None), "find -exec grep -n {} +: no pattern")
check(aq.shell_retrieval("find . -name '*.orig' -exec rm {} + ; grep -rn later_pat crates/", ROOT) == ("grep", "later_pat"), "rejected find, later grep judged on its own")
check(aq.shell_retrieval("find . -name '*.orig' -exec rm {} + ; cargo test", ROOT) == (None, None), "rejected find, nothing else")
events = cs_call("find_symbol", FS_RESULT, name="foo") + native("Bash", command="find crates -name '*.rs' -exec grep -n foo_bar {} \\;") + native("Bash", command="find crates -name '*.rs' -exec cat {} \\;")
agg = aq.aggregate([events])
check(agg["shell_grep_calls"] == 1 and agg["grep_identifier_shaped"] == 1, f"find -exec grep counted as a shell grep {agg['shell_grep_calls']}/{agg['grep_identifier_shaped']}")
events = user("where is foo_bar defined?") + native("Bash", command="find crates -name '*.rs' -exec grep -n foo_bar {} \\;") + cs_call("find_symbol", FS_RESULT, name="foo")
check(aq.aggregate([events])["question_shape_counts"]["identifier"]["first_grep"] == 1, "shape walk: find -exec grep is first=Grep")
events = user("where is foo_bar defined?") + native("Bash", command="find crates -name '*.rs' -exec cat {} \\;") + cs_call("find_symbol", FS_RESULT, name="foo")
check(aq.aggregate([events])["question_shape_counts"]["identifier"]["first_glob"] == 1, "shape walk: find -exec cat is first=Glob")

# Pin HOME so tilde expansion does not depend on the runner.
_real_home = os.environ.get("HOME")
os.environ["HOME"] = "/home/x"
try:
    check(aq.shell_retrieval_detail("grep -rn foo /home/x/tilde-proj/src", "~/tilde-proj") == ("grep", "foo", "/home/x/tilde-proj"), "shell_retrieval_detail: tilde root expanded")
    check(aq.shell_retrieval_detail("grep -rn foo /home/y/tilde-proj/src", "~/tilde-proj") == (None, None, None), "shell_retrieval_detail: tilde root expanded, other home rejected")
finally:
    if _real_home is None:
        del os.environ["HOME"]
    else:
        os.environ["HOME"] = _real_home

with tempfile.TemporaryDirectory() as td:
    (Path(td) / "myproj" / ".codesage").mkdir(parents=True)
    (Path(td) / "myproj" / ".codesage" / "index.db").write_bytes(b"")
    _cwd = os.getcwd()
    os.chdir(td)
    try:
        check(aq.is_onboarded_root("myproj", set(), {}) is False, "is_onboarded_root: relative root is False even when cwd/myproj is onboarded")
        check(aq.is_onboarded_root(str(Path(td) / "myproj" / "sub"), set(), {}) is True, "is_onboarded_root: the same root, absolute, walks up to index.db")
        check(aq.is_onboarded_root("myproj", {os.path.abspath("myproj")}, {}) is False, "is_onboarded_root: relative root never matches session roots")
    finally:
        os.chdir(_cwd)

rel_call = use(CS + "find_symbol", project="codesage", name="foo")
agg = aq.aggregate([[rel_call, result(rel_call, FS_RESULT)] + native("Grep", pattern="foo_bar") + native("Bash", command="grep -rn foo_bar crates/")])
check(agg["grep_tool_calls"] == 0 and agg["grep_tool_calls_all"] == 1, "relative project: Grep tool has no onboarded root")
check(agg["shell_grep_calls"] == 0 and agg["shell_grep_calls_all"] == 1, "relative project: shell grep has no onboarded root")
events = cs_call("find_symbol", FS_RESULT, name="foo") + native("Bash", command="grep -rn foo crates/")
check(bucket(events)["non_terminal"] == 1, "gate: terminality unchanged")

events = cs_call("find_symbol", FS_RESULT, name="foo") + native("Bash", command="pushd /repo && grep -rn foo src/")
check(bucket(events)["terminal"] == 1, "scorer: pushd loses cwd, relative grep not charged")
events = user("where is foo_bar defined?") + native("Bash", command="rg foo_bar crates") + cs_call("find_symbol", FS_RESULT, name="foo")
shape = aq.aggregate([events])["question_shape_counts"]["identifier"]
check(shape["first_grep"] == 1 and shape["first_other_or_none"] == 0, f"shape walk: shell grep is first=Grep {shape}")
check("**Transcripts scanned**: 3 (1 session, 2 subagent)" in report, "render: subagent count in header")
empty_report = aq.render(aq.aggregate([]), window_days=7, transcripts=0, now="now")
check("_No codesage tool calls in the window._" in empty_report, "render: empty placeholder")

# Claude Code puts tool results on role=user envelopes, alongside actual user turns.
rows = [
    {"type": "user", "timestamp": "t0", "message": {"role": "user", "content": "where is foo?"}},
    {"type": "assistant", "timestamp": "t1", "message": {"role": "assistant", "content": [
        {"type": "tool_use", "id": "toolu_a", "name": CS + "find_symbol",
         "input": {"project": ROOT, "name": "foo"}}]}},
    {"type": "user", "timestamp": "t2", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_a", "content": FS_RESULT}]}},
    {"type": "assistant", "timestamp": "t3", "message": {"role": "assistant", "content": [
        {"type": "tool_use", "id": "toolu_b", "name": "Read",
         "input": {"file_path": "/repo/crates/graph/src/search.rs"}}]}},
    {"type": "user", "timestamp": "t4", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_b",
         "content": [{"type": "text", "text": "1\tfn foo() {}"}]}]}},
]
with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False, encoding="utf-8") as fh:
    for r in rows:
        fh.write(json.dumps(r) + "\n")
    jsonl_path = Path(fh.name)
evs = aq.extract_events(jsonl_path)
jsonl_path.unlink()
kinds = [e["kind"] for e in evs]
check(kinds == ["user", "tool_use", "tool_result", "tool_use", "tool_result"], f"extract_events: kinds {kinds}")
check(evs[2]["pair_tool"] == CS + "find_symbol" and evs[2]["text"] == FS_RESULT, "extract_events: str result paired")
check(evs[4]["pair_tool"] == "Read" and evs[4]["text"] == "1\tfn foo() {}", "extract_events: list result paired")
b = aq.terminality(evs)["tools"].get("find_symbol") or {}
check(b.get("terminal") == 1 and b.get("follow_through_read") == 1, "extract_events -> terminality follow-through")
agg = aq.aggregate([evs])
check(agg["codesage_results_total"] == 1 and agg["quality"]["find_symbol"]["ok"] == 1, "extract_events -> quality counts")

with tempfile.TemporaryDirectory() as td:
    proj = Path(td) / "-home-x-proj"
    sub = proj / "sess1" / "subagents"
    sub.mkdir(parents=True)
    (proj / "sess1.jsonl").write_text("{}\n")
    (sub / "agent-a.jsonl").write_text("{}\n")
    (sub / "agent-b.jsonl").write_text("{}\n")
    (proj / "sess1" / "not-a-subagent.jsonl").write_text("{}\n")
    old = proj / "old.jsonl"
    old.write_text("{}\n")
    os.utime(old, (time.time() - 10 * 86400, time.time() - 10 * 86400))
    found = aq.iter_transcripts(Path(td), time.time() - 86400)
    names = sorted(p.name for p in found)
    check(names == ["agent-a.jsonl", "agent-b.jsonl", "sess1.jsonl"], f"iter_transcripts: {names}")
    check(sum(1 for p in found if aq.is_subagent_transcript(p)) == 2, "iter_transcripts: subagent count")


if failures:
    print(f"FAILED ({len(failures)}):")
    for f in failures:
        print(f)
    sys.exit(1)
suffix = ""
if skipped_blocks:
    suffix = f" ({len(skipped_blocks)} block skipped: {', '.join(skipped_blocks)})"
print(f"all analyze-quality terminality tests passed{suffix}")
sys.exit(0)
