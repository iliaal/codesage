#!/usr/bin/env python3
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("analyze", Path(__file__).with_name("analyze.py"))
analyze = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(analyze)


def hook_event(payload, tool_id="edit-1", kind="hook_success"):
    return {"type": "attachment", "attachment": {
        "type": kind, "toolUseID": tool_id, "exitCode": 0,
        "stdout": json.dumps({"hookSpecificOutput": {"additionalContext": payload}}),
    }}


def tool(name, **kwargs):
    return {"type": "assistant", "message": {"content": [
        {"type": "tool_use", "name": name, "input": kwargs},
    ]}}


class ScorerTest(unittest.TestCase):
    def test_observed_actions_never_satisfy_default_on_gate(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            project = root / "projects" / analyze.munge_project("/repo")
            project.mkdir(parents=True)
            payload = "tests: tests/test_alpha.py\n"
            row = {"t": 100, "s": "session", "p": "/repo", "f": "a.py",
                   "d": "served", "h": analyze.fnv1a64(payload)}
            (root / analyze.FIRE_LOG).write_text((json.dumps(row) + "\n") * 51)
            events = [hook_event(payload, f"edit-{i}") for i in range(51)]
            events.append(tool("Bash", command="pytest tests/test_alpha.py"))
            (project / "session.jsonl").write_text("".join(json.dumps(e) + "\n" for e in events))
            result = subprocess.run(["python3", str(Path(__file__).with_name("analyze.py")),
                                     "--ledger-dir", str(root), "--projects-dir", str(root / "projects"),
                                     "--json"], text=True, capture_output=True, check=True)
            report = json.loads(result.stdout)
            self.assertEqual(report["served_scored"]["verdicts"], {"acted": 51})
            self.assertFalse(report["default_on_ready"])

    def test_identical_same_second_fires_are_distinct(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            row = {"t": 100, "s": "session", "p": "/repo", "f": "a.py", "d": "repeat"}
            (root / analyze.FIRE_LOG).write_text((json.dumps(row) + "\n") * 2)
            self.assertEqual(len(analyze.load_fires(root, root)), 2)

    def test_serialized_context_is_one_exposure_per_tool(self):
        payload = "tests: tests/test_alpha.py\n"
        events = [hook_event(payload), hook_event(payload, kind="hook_additional_context"),
                  tool("Bash", command="pytest tests/test_alpha.py"), hook_event(payload, "edit-2")]
        hits = analyze.payload_occurrences(events)[analyze.fnv1a64(payload)]
        self.assertEqual([i for i, _ in hits], [0, 3])
        self.assertEqual(analyze.score_serve(events, 0, ["tests/test_alpha.py"], []), "acted")
        self.assertEqual(analyze.score_serve(events, 3, ["tests/test_alpha.py"], []), "no-op")

    def test_quoted_or_failed_payload_is_not_exposure(self):
        payload = "tests: tests/test_alpha.py\n"
        failed = hook_event(payload)
        failed["attachment"]["exitCode"] = 1
        self.assertFalse(analyze.payload_occurrences([
            tool("Bash", command=payload), {"type": "user", "message": {"content": payload}}, failed,
        ]))

    def test_named_test_requires_execution_not_mention(self):
        test = "tests/test_alpha.py"
        for command in (f"echo pytest {test}", f"cat {test}", f"pytest {test}.bak", f"printf '{test}'",
                        f"pytest --collect-only {test}", f"pytest --help {test}", f"python3 -V {test}"):
            self.assertFalse(analyze.runs_named_test(command, [test]), command)
        for command in (f"false && pytest {test}", f"cat <<EOF\npytest {test}\nEOF"):
            self.assertFalse(analyze.runs_named_test(command, [test]), command)
        for command in (f"pytest {test}", f"rtk proxy python3 {test}", f"cd /repo && pytest {test}"):
            self.assertTrue(analyze.runs_named_test(command, [test]), command)

    def test_redirect_targets_are_not_test_selectors(self):
        test = "tests/test_alpha.py"
        for redirect in (">", ">>", "<", "2>", "2>>", "&>", "<>"):
            command = f"pytest unrelated.py {redirect} {test}"
            self.assertFalse(analyze.runs_named_test(command, [test]), command)
        for command in (f"pytest unrelated.py >{test}", f"pytest unrelated.py <{test}",
                        f"pytest {test} > output.log"):
            self.assertFalse(analyze.runs_named_test(command, [test]), command)

    def test_runner_options_do_not_turn_arguments_into_executed_tests(self):
        test = "tests/test_alpha.py"
        rejected = [f"python3 unrelated.py {test}", f"python3 -V {test}",
                    f"python3 -m unrelated {test}", f"pytest --setup-only {test}",
                    f"pytest -hvs {test}", f"pytest --collect-only=true {test}",
                    f"pytest -k {test}", f"pytest --tb={test}",
                    f"phpunit --filter {test}", f"jest --testNamePattern {test}",
                    f"vitest list {test}", f"node --help {test}",
                    f"cargo test {test}", f"npm test -- {test}", f"go test {test}"]
        for command in rejected:
            self.assertFalse(analyze.runs_named_test(command, [test]), command)
        accepted = [f"python3 {test}", f"python3 -B -m pytest -q {test}",
                    f"python3 -m unittest -v {test}", f"pytest -vs {test}::test_case",
                    f"pytest -k alpha --maxfail=1 {test}", f"phpunit --filter alpha {test}",
                    f"jest --runInBand {test}", f"vitest run {test}", f"node --test {test}"]
        for command in accepted:
            self.assertTrue(analyze.runs_named_test(command, [test]), command)

    def test_ranged_and_suffix_reads_are_not_acted(self):
        events = [hook_event("changes with: src/alpha.py"), tool("Read", file_path="/repo/src/alpha.py", limit=10)]
        self.assertEqual(analyze.score_serve(events, 0, [], ["src/alpha.py"]), "ambiguous")
        events[1] = tool("Read", file_path="/repo/othersrc/alpha.py")
        self.assertEqual(analyze.score_serve(events, 0, [], ["src/alpha.py"]), "no-op")

    def test_transcript_in_different_cwd_requires_unique_session(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "other-cwd").mkdir()
            match = root / "other-cwd/session.jsonl"
            match.touch()
            self.assertEqual(analyze.transcript_path(root, "/project", "session"), match)
            (root / "another-cwd").mkdir()
            (root / "another-cwd/session.jsonl").touch()
            self.assertIsNone(analyze.transcript_path(root, "/project", "session"))
            self.assertIsNone(analyze.transcript_path(root, "/project", "../session"))


class HookTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / ".codesage").mkdir()
        (self.root / ".codesage/index.db").touch()
        self.source = self.root / "alpha.py"
        self.source.write_text("pass\n")
        self.env = dict(os.environ, CODESAGE_BRIEF_CANARY="1")
        self.stub = self.root / "bin"
        self.stub.mkdir()
        executable = self.stub / "codesage"
        executable.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$BRIEF_ARGS\"\nprintf 'tests: tests/test_alpha.py\\n'\n")
        executable.chmod(0o755)
        self.args = self.root / "args"
        self.env.update(PATH=str(self.stub) + ":" + os.environ["PATH"], BRIEF_ARGS=str(self.args))

    def run_hook(self, session="canary-1", enabled="1", **extra):
        value = {"tool_name": "Edit", "tool_input": {"file_path": str(self.source)}, **extra}
        if session is not None:
            value["session_id"] = session
        result = subprocess.run(["bash", str(ROOT / "plugins/codesage-tools/hooks/brief-hook.sh")],
                                input=json.dumps(value), text=True, capture_output=True,
                                env=dict(self.env, CODESAGE_BRIEF_CANARY=enabled), cwd="/", timeout=3)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stderr, "")
        return result.stdout

    def test_disabled_and_invalid_session_never_call_binary(self):
        self.assertEqual(self.run_hook(enabled="0"), "")
        for session in (None, "", "../escape", "x" * 129):
            self.assertEqual(self.run_hook(session=session), "")
        self.assertFalse(self.args.exists())

    def test_enabled_context_always_uses_session_gate(self):
        result = json.loads(self.run_hook())
        self.assertEqual(result, {"hookSpecificOutput": {
            "hookEventName": "PreToolUse", "additionalContext": "tests: tests/test_alpha.py",
        }})
        self.assertEqual(self.args.read_text().splitlines(), ["brief", "--session", "canary-1", "--", "alpha.py"])

    def test_non_edit_and_nested_session_are_silent(self):
        self.assertEqual(self.run_hook(tool_name="Read"), "")
        self.assertEqual(self.run_hook(session=None, metadata={"session_id": "nested"}), "")
        self.assertFalse(self.args.exists())


if __name__ == "__main__":
    unittest.main()
