#!/usr/bin/env python3
import importlib.util
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

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


BRANCH_PAYLOAD = (
    'branch overlap: "feature/topic" edits "src/alpha.py" '
    '(same-file, HEAD...branch; merge base ' + 'a' * 40 + ')\n'
    'Branch overlap: 1 matching branch(es); scanned 2/3 refs '
    '(local and remote-tracking refs; same-tip matches counted once; not a liveness check).\n'
)


class ScorerTest(unittest.TestCase):
    def test_branch_context_preserves_mixed_digest_and_action(self):
        payload = "tests: tests/test_alpha.py\n" + BRANCH_PAYLOAD
        events = [hook_event(payload.rstrip("\n")),
                  hook_event(payload.rstrip("\n"), kind="hook_additional_context"),
                  tool("Bash", command="pytest tests/test_alpha.py"),
                  hook_event(payload, "edit-2")]
        hits = analyze.payload_occurrences(events)[analyze.fnv1a64(payload)]
        self.assertEqual(hits, [(0, payload), (3, payload)])
        tests, coupled = analyze.parse_payload(payload)
        self.assertEqual((tests, coupled), (["tests/test_alpha.py"], []))
        self.assertEqual(analyze.score_serve(events, hits[0][0], tests, coupled), "acted")
        self.assertEqual(analyze.score_serve(events, hits[1][0], tests, coupled), "no-op")

    def test_branch_render_variants_preserve_full_payload(self):
        branch = BRANCH_PAYLOAD.splitlines()[0]
        extended = (
            'hotspot: churn percentile 95%, 2 of 4 commits were fixes\n'
            'changes with: src/beta.py\n'
            + branch.replace('"feature/topic"', '"feature/quoted\\\"name"') + '\n'
            + branch.replace('a' * 40, 'b' * 64).replace('"src/alpha.py"', '"src/a b.py", "src/beta.py"') + '\n'
            + BRANCH_PAYLOAD.splitlines()[1].replace('1 matching', '3 matching')
            + ' Showing 2 branches. Branch overlap incomplete: Git time budget exhausted.\n'
        )
        hits = analyze.payload_occurrences([hook_event(extended)])[analyze.fnv1a64(extended)]
        self.assertEqual(hits, [(0, extended)])
        self.assertEqual(analyze.parse_payload(extended), ([], ["src/beta.py"]))

    def test_branch_only_exposure_is_not_scoreable(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            project = root / "projects" / analyze.munge_project("/repo")
            project.mkdir(parents=True)
            row = {"t": 100, "s": "session", "p": "/repo", "f": "alpha.py",
                   "d": "served", "h": analyze.fnv1a64(BRANCH_PAYLOAD)}
            (root / analyze.FIRE_LOG).write_text(json.dumps(row) + "\n")
            events = [hook_event(BRANCH_PAYLOAD), tool("Read", file_path="/repo/src/alpha.py")]
            (project / "session.jsonl").write_text("".join(json.dumps(e) + "\n" for e in events))
            result = subprocess.run(["python3", str(Path(__file__).with_name("analyze.py")),
                                     "--ledger-dir", str(root), "--projects-dir", str(root / "projects"),
                                     "--json"], text=True, capture_output=True, check=True)
            report = json.loads(result.stdout)
            self.assertEqual(report["served_scored"]["verdicts"], {"branch-only": 1})
            self.assertEqual(report["served_scored"]["scoreable_n"], 0)
            self.assertFalse(report["observational_sample_ready"])
            self.assertFalse(report["default_on_ready"])
            self.assertNotIn("unmatched_reason", report["serves"][0])
            text_result = subprocess.run(["python3", str(Path(__file__).with_name("analyze.py")),
                                          "--ledger-dir", str(root), "--projects-dir", str(root / "projects")],
                                         text=True, capture_output=True, check=True)
            self.assertIn("branch-only: 1", text_result.stdout)

    def test_branch_exposure_requires_valid_hook_and_complete_payload(self):
        failed = hook_event(BRANCH_PAYLOAD)
        failed["attachment"]["exitCode"] = 1
        malformed = hook_event(BRANCH_PAYLOAD)
        malformed["attachment"]["stdout"] = '{"hookSpecificOutput":'
        self.assertFalse(analyze.payload_occurrences([
            tool("Bash", command=BRANCH_PAYLOAD),
            {"type": "user", "message": {"content": BRANCH_PAYLOAD}},
            {"attachment": {"type": "other", "additionalContext": BRANCH_PAYLOAD}},
            failed, malformed,
        ]))
        for payload in (
            BRANCH_PAYLOAD.splitlines()[0] + "\n",
            BRANCH_PAYLOAD.splitlines()[1] + "\n",
            BRANCH_PAYLOAD.replace("merge base", "not a merge base"),
        ):
            self.assertNotIn(analyze.fnv1a64(payload),
                             analyze.payload_occurrences([hook_event(payload)]), payload)

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
            self.assertEqual(report["served_scored"]["scoreable_n"], 51)
            self.assertTrue(report["observational_sample_ready"])
            self.assertEqual(report["required_scoreable_serves"], 50)
            self.assertFalse(report["default_on_ready"])

    def test_raw_serves_do_not_satisfy_observational_threshold(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            project = root / "projects" / analyze.munge_project("/repo")
            project.mkdir(parents=True)
            payload = "tests: tests/test_alpha.py\n"
            hotspot = "hotspot: churn percentile 99\n"
            rows = [
                {"t": i, "s": "session", "p": "/repo", "f": "a.py",
                 "d": "served", "h": analyze.fnv1a64(payload)} for i in range(50)
            ]
            rows.append({"t": 51, "s": "missing", "p": "/repo", "f": "b.py",
                         "d": "served", "h": analyze.fnv1a64(payload)})
            rows.append({"t": 52, "s": "session", "p": "/repo", "f": "c.py",
                         "d": "served", "h": analyze.fnv1a64(hotspot)})
            (root / analyze.FIRE_LOG).write_text("".join(json.dumps(r) + "\n" for r in rows))
            (project / "session.jsonl").write_text(json.dumps(hook_event(hotspot)) + "\n")
            result = subprocess.run(["python3", str(Path(__file__).with_name("analyze.py")),
                                     "--ledger-dir", str(root), "--projects-dir", str(root / "projects"),
                                     "--min-served", "3", "--json"],
                                    text=True, capture_output=True, check=True)
            report = json.loads(result.stdout)
            self.assertEqual(report["served_scored"]["n"], 52)
            self.assertEqual(report["served_scored"]["scoreable_n"], 0)
            self.assertFalse(report["observational_sample_ready"])
            self.assertEqual(report["required_scoreable_serves"], 3)
            self.assertFalse(report["default_on_ready"])
            self.assertEqual(report["serves"][0]["unmatched_reason"], "exposure-not-found")
            self.assertEqual(report["serves"][50]["unmatched_reason"], "transcript-not-resolved")
            self.assertEqual(report["serves"][51]["verdict"], "hotspot-only")
            self.assertNotIn("unmatched_reason", report["serves"][51])

    def test_identical_same_second_fires_are_distinct(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            row = {"t": 100, "s": "session", "p": "/repo", "f": "a.py", "d": "repeat"}
            (root / analyze.FIRE_LOG).write_text((json.dumps(row) + "\n") * 2)
            self.assertEqual(len(analyze.load_fires(root, root)), 2)

    @unittest.skipUnless(os.name == "posix" and os.geteuid() != 0,
                         "permission denial requires a non-root POSIX user")
    def test_unreadable_transcript_is_not_missing_exposure(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            project = root / "projects" / analyze.munge_project("/repo")
            project.mkdir(parents=True)
            payload = "tests: tests/test_alpha.py\n"
            row = {"t": 100, "s": "session", "p": "/repo", "f": "a.py",
                   "d": "served", "h": analyze.fnv1a64(payload)}
            (root / analyze.FIRE_LOG).write_text(json.dumps(row) + "\n")
            transcript = project / "session.jsonl"
            transcript.write_text(json.dumps(hook_event(payload)) + "\n")
            transcript.chmod(0)
            try:
                result = subprocess.run(["python3", str(Path(__file__).with_name("analyze.py")),
                                         "--ledger-dir", str(root), "--projects-dir", str(root / "projects"),
                                         "--json"], text=True, capture_output=True, check=True)
            finally:
                transcript.chmod(0o600)
            report = json.loads(result.stdout)
            self.assertIn("cannot read transcript", result.stderr)
            self.assertEqual(report["serves"][0]["unmatched_reason"], "transcript-read-failed")
            self.assertEqual(report["served_scored"]["scoreable_n"], 0)
            self.assertFalse(report["observational_sample_ready"])

    def test_partial_transcript_read_discards_exposure_and_base_rate(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            project = root / "projects" / analyze.munge_project("/repo")
            project.mkdir(parents=True)
            payload = "tests: tests/test_alpha.py\n"
            row = {"t": 100, "s": "session", "p": "/repo", "f": "alpha.py",
                   "d": "served", "h": analyze.fnv1a64(payload)}
            (root / analyze.FIRE_LOG).write_text(json.dumps(row) + "\n")
            transcript = project / "session.jsonl"
            transcript.touch()
            events = [hook_event(payload), tool("Edit", file_path="/repo/alpha.py"),
                      tool("Bash", command="pytest tests/test_alpha.py")]

            class InterruptedRead:
                def __enter__(self):
                    return self

                def __exit__(self, *args):
                    return False

                def __iter__(self):
                    yield from (json.dumps(event) + "\n" for event in events)
                    raise OSError("interrupted transcript read")

            original_open = Path.open

            def open_path(path, *args, **kwargs):
                if path == transcript:
                    return InterruptedRead()
                return original_open(path, *args, **kwargs)

            stdout, stderr = io.StringIO(), io.StringIO()
            argv = ["analyze.py", "--ledger-dir", str(root),
                    "--projects-dir", str(root / "projects"), "--json"]
            with patch("sys.argv", argv), patch.object(Path, "open", open_path), \
                    contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                self.assertEqual(analyze.main(), 0)
            report = json.loads(stdout.getvalue())
            self.assertIn("interrupted transcript read", stderr.getvalue())
            self.assertEqual(report["serves"][0]["unmatched_reason"], "transcript-read-failed")
            self.assertEqual(report["served_scored"]["scoreable_n"], 0)
            self.assertEqual(report["base_rate"]["edits"], 0)

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
