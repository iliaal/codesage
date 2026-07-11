#!/usr/bin/env python3
"""Cheap flag-sweep preflight for bench/agent-task-runner's WITH arm.

The agent-task A/B kept producing INVALID reports because the WITH arm's tool
set was wrong under headless `claude -p` (CLI 2.1.x): the bare `mcp__codesage`
grant coincided with the built-in `Grep` going "not enabled", `--setting-sources
user` pulled the operator's `bypassPermissions` default + hooks into the run,
and `--bare` stripped the built-ins entirely (leaving only Task/SendMessage/…).

Rather than discover each quirk via a $5 benchmark run, this sweeps a handful of
flag combinations with a TRIVIAL prompt (~$0.01 each) and prints, per combo, the
tool set Claude Code actually initialized. Pick the row that shows
`Grep+Read+Glob present, codesage server configured, Bash absent` and set the
harness's build_command to match.

Usage (NON-NESTED, plain terminal):
  bench/probe-claude-tools.py --model opus
  bench/probe-claude-tools.py --model fable --project /path/to/repo

Note on MCP: at the `init` event the codesage server is usually `status:pending`
(it connects a beat later), so "codesage tools present" reads 0 at init even when
the server is configured correctly — judge by the server being LISTED, plus the
built-ins being present. The benchmark's own cs-usage gate catches a server that
never actually connects.
"""

from __future__ import annotations

import argparse
import importlib.machinery
import importlib.util
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
BASE_ALLOW = ["Grep", "Read", "Glob"]
DISALLOW = "Bash,Edit,Write,NotebookEdit,WebFetch,WebSearch"

# name -> extra knobs. grant: the codesage allow-list entry; permission_mode /
# setting_sources: optional flags. E is the original (contaminated) config as a
# control so the table shows the before/after.
CONFIGS = [
    ("A default+wildcard", dict(grant="mcp__codesage__*", permission_mode="default")),
    ("B default+bare", dict(grant="mcp__codesage", permission_mode="default")),
    ("C dontAsk+wildcard", dict(grant="mcp__codesage__*", permission_mode="dontAsk")),
    ("D project-settings", dict(grant="mcp__codesage__*", permission_mode="default", setting_sources="project")),
    ("E original(control)", dict(grant="mcp__codesage", setting_sources="user")),
]


def _load_runner():
    loader = importlib.machinery.SourceFileLoader("agent_task_runner", str(HERE / "agent-task-runner"))
    spec = importlib.util.spec_from_loader("agent_task_runner", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


def build_argv(mcp_cfg, model, effort, *, grant, permission_mode=None, setting_sources=None):
    allow = ",".join(BASE_ALLOW + [grant])
    a = [
        "claude", "-p", "Reply with the single word: ok",
        "--allowedTools", allow,
        "--disallowedTools", DISALLOW,
        "--output-format", "stream-json", "--verbose",
        "--model", model, "--effort", effort,
        "--max-turns", "1", "--max-budget-usd", "0.10",
        "--strict-mcp-config", "--mcp-config", mcp_cfg,
    ]
    if permission_mode:
        a += ["--permission-mode", permission_mode]
    if setting_sources:
        a += ["--setting-sources", setting_sources]
    return a


def parse_init(stdout: str):
    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if obj.get("type") == "system" and obj.get("subtype") == "init":
            return obj.get("tools") or [], obj.get("mcp_servers") or []
    return None, None


def main() -> int:
    ap = argparse.ArgumentParser(description="Sweep claude -p flag combos for the WITH-arm tool set.")
    ap.add_argument("--model", default="opus")
    ap.add_argument("--effort", default="high")
    ap.add_argument("--project", type=Path, default=Path.cwd())
    ap.add_argument("--codesage-bin", default=None)
    args = ap.parse_args()

    m = _load_runner()
    project = args.project.expanduser().resolve()
    codesage_bin = m.resolve_codesage_bin(args.codesage_bin)
    mcp_cfg = m.mcp_config_with(codesage_bin, project)

    print(f"project={project}  model={args.model}  ({len(CONFIGS)} trivial runs, ~$0.01 each)\n")
    print(f"{'config':22} {'built-ins':10} {'cs-server':22} {'bash':5} tools")
    print("-" * 92)
    winner = None
    for name, knobs in CONFIGS:
        argv = build_argv(mcp_cfg, args.model, args.effort, **knobs)
        try:
            r = subprocess.run(argv, cwd=str(project), capture_output=True, text=True,
                               timeout=120, env=m.clean_child_env())
        except subprocess.TimeoutExpired:
            print(f"{name:22} TIMEOUT")
            continue
        tools, servers = parse_init(r.stdout)
        if tools is None:
            print(f"{name:22} NO-INIT  rc={r.returncode}  {(r.stderr or '')[-160:].strip()}")
            continue
        builtins = all(b in tools for b in ("Grep", "Read", "Glob"))
        cs = next((s for s in servers if str(s.get("name")) == "codesage"), None)
        cs_str = f"{cs.get('status')}" if cs else "MISSING"
        bash = "Bash" in tools
        ok = builtins and cs is not None and not bash
        if ok and winner is None:
            winner = name
        print(f"{name:22} {str(builtins):10} {cs_str:22} {str(bash):5} {','.join(map(str, tools[:8]))}")

    print()
    if winner:
        print(f"WINNER: {winner} — set the harness build_command to match this combo.")
    else:
        print("No combo gave built-ins + codesage server + no Bash. Attach this table to a "
              "claude-code issue; fallback: list codesage tools individually in --allowedTools.")
    return 0 if winner else 1


if __name__ == "__main__":
    sys.exit(main())
