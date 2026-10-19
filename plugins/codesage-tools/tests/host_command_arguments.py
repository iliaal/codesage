#!/usr/bin/env python3
"""Opt-in installed Claude host check; runs six authenticated, isolated sessions."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import uuid


PLUGIN = Path(__file__).resolve().parents[1]


def check_case(root, command, allowed):
    case = root / f"{command}-{'allowed' if allowed else 'denied'}"
    case.mkdir()
    plugin = case / "plugin with spaces"
    shutil.copytree(PLUGIN, plugin)
    record = case / "record.json"
    marker = f"FORBIDDEN_{uuid.uuid4().hex}"
    values = ["--corpus-dir", "ordinary space", "--runner", f"semi; touch {marker}",
              "--codesage-bin", f"$(touch {marker})", "--corpus-dir", f"and && touch {marker}",
              "--runner", f"pipe | touch {marker}", "--codesage-bin", f"`touch {marker}`"]
    if command != "bench":
        project = case / "project space"
        project.mkdir()
        values.insert(0, str(project))
    executable = plugin / "bin" / f"codesage-{command}"
    executable.write_text(
        "#!/bin/bash\n"
        "python3 - \"$@\" <<'PY'\n"
        "import json, pathlib, sys\n"
        f"pathlib.Path({str(record)!r}).write_text(json.dumps(sys.argv[1:]))\n"
        "print('ARGUMENTS_RECORDED')\n"
        "PY\n")
    settings = case / "settings.json"
    settings.write_text(json.dumps({"enabledPlugins": {}, "disableAllHooks": True}))
    prompt = f"/codesage-tools:codesage-{command} {shlex.join(values)}\n\n" + (
        "Exercise only the script invocation step, then stop. The installed test plugin's "
        "target script is an inert argv recorder; prerequisite discovery and later reporting "
        "steps are outside this invocation test. Do not edit the plugin. "
        "If a required tool is unavailable or denied, report that and stop; do not use an alternate tool."
    )
    args = ["claude", "-p", prompt, "--plugin-dir", str(plugin), "--setting-sources", "",
            "--settings", str(settings), "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
            "--tools", "Bash,Read,Write,Skill", "--allowedTools",
            "Bash,Read,Write,Skill" if allowed else "Read,Write,Skill",
            "--permission-mode", "dontAsk", "--no-session-persistence", "--output-format",
            "stream-json", "--verbose"]
    if not allowed:
        args += ["--disallowedTools", "Bash"]
    with (case / "host.jsonl").open("w") as output:
        result = subprocess.run(args, cwd=case, stdout=output, stderr=subprocess.STDOUT,
                                timeout=180,
                                env={**os.environ, "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"})
    events = [json.loads(line) for line in (case / "host.jsonl").read_text().splitlines()
              if line.startswith("{")]
    calls = [block for event in events for block in event.get("message", {}).get("content", [])
             if isinstance(block, dict) and block.get("type") == "tool_use"]
    host_tools = next(event["tools"] for event in events if event.get("subtype") == "init")
    summary = {"command": command, "bash_allowed": allowed, "returncode": result.returncode,
               "host_tools": host_tools,
               "calls": [{"name": call["name"], "input": call["input"]} for call in calls],
               "result": next((event.get("result", "") for event in reversed(events)
                               if event.get("type") == "result"), ""),
               "expected_argv": values, "actual_argv": json.loads(record.read_text()) if record.exists() else None}
    (case / "summary.json").write_text(json.dumps(summary, indent=2))
    if result.returncode != 0:
        raise AssertionError(f"host failed: {case / 'host.jsonl'}")
    if (case / marker).exists():
        raise AssertionError(f"payload executed: {case / marker}")
    if allowed:
        if "Bash" not in host_tools:
            raise AssertionError(f"Bash-allowed case lacks Bash: {case / 'summary.json'}")
        if summary["actual_argv"] != values:
            raise AssertionError(f"argv mismatch: {case / 'summary.json'}")
        dispatches = [call for call in calls if call["name"] == "Bash"
                      and "codesage-invoke" in call["input"].get("command", "")]
        if len(dispatches) != 1:
            raise AssertionError(f"expected one dispatcher invocation: {case / 'summary.json'}")
        shell = dispatches[0]["input"]["command"]
        if marker in shell or "ARGUMENTS_RECORDED" in shell:
            raise AssertionError(f"argument data entered shell source: {case / 'summary.json'}")
        words = shlex.split(shell)
        if len(words) != 4 or words[0] != "python3" or words[2] != command:
            raise AssertionError(f"dispatcher was not a single command: {case / 'summary.json'}")
        writes = [call for call in calls if call["name"] == "Write"]
        if not any(json.loads(call["input"]["content"]) == values for call in writes):
            raise AssertionError(f"expected native file write of argv: {case / 'summary.json'}")
    elif "Bash" in host_tools or record.exists() or any(call["name"] == "Bash" for call in calls):
        raise AssertionError(f"Bash-denied case executed: {case / 'summary.json'}")
    print(f"{command}: Bash {'allowed' if allowed else 'denied'} passed", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="new directory for host transcripts and summaries")
    args = parser.parse_args()
    args.output.mkdir()
    version = subprocess.check_output(["claude", "--version"], text=True).strip()
    hashes = {str(path.relative_to(PLUGIN)): hashlib.sha256(path.read_bytes()).hexdigest()
              for path in [PLUGIN / "bin/codesage-invoke",
                           PLUGIN / "references/command-arguments.md",
                           *(PLUGIN / "commands" / f"codesage-{name}.md"
                             for name in ("onboard", "eval", "bench"))]}
    (args.output / "provenance.json").write_text(json.dumps({"host": version, "sha256": hashes}, indent=2))
    for command in ("bench", "eval", "onboard"):
        for allowed in (True, False):
            check_case(args.output, command, allowed)


if __name__ == "__main__":
    main()
