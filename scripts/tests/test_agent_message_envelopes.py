"""Malformed assistant envelopes must not hide other stream-json records."""

import contextlib
import importlib.machinery
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]


def load(filename, name):
    loader = importlib.machinery.SourceFileLoader(name, str(ROOT / "bench" / filename))
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


task_runner = load("agent-task-runner", "envelope_task_runner")
selection = load("agent-tool-selection-harness.py", "envelope_selection")
NON_OBJECTS = (None, False, True, 0, 1, 1.5, "", "text", [], ["text"])


def assistant(tool, tokens):
    return {"type": "assistant", "message": {
        "content": [{"type": "tool_use", "name": tool}],
        "usage": {"input_tokens": tokens, "output_tokens": "2",
                  "cache_read_input_tokens": 3, "cache_creation_input_tokens": 4},
    }}


def parse(module, rows, *, with_codesage=True, returncode=0, stderr=""):
    # Keep blank lines, non-JSON logging and invalid JSON on the existing path.
    stdout = "log line\n{bad json\n[]\n" + "\n\n".join(json.dumps(row) for row in rows)
    if module is task_runner:
        return module.parse_run_output(stdout, stderr, returncode, 1.25,
                                       with_codesage=with_codesage)
    completed = subprocess.CompletedProcess([], returncode, stdout, stderr)
    # No real CLI, model session, credentials, or network is used by these tests.
    with patch.object(module.subprocess, "run", return_value=completed) as run:
        with patch.object(module.time, "time", side_effect=[10, 11.25]):
            with contextlib.redirect_stderr(io.StringIO()):
                result = module.run_task(ROOT, "synthetic query", with_codesage)
    run.assert_called_once()
    return result


class AgentMessageEnvelopeTests(unittest.TestCase):
    def rows(self):
        return [assistant("Read", 10), assistant("mcp__codesage__search", 20),
                {"type": "result", "subtype": "success", "result": " found.py ",
                 "total_cost_usd": "0.25"}]

    def test_valid_envelopes_preserve_metrics(self):
        result = parse(task_runner, self.rows())
        self.assertEqual(result, {
            "error": None, "raced": False, "diag": "", "duration_s": 1.25,
            "tool_calls": 2, "codesage_calls": 1, "grep_calls": 0, "read_calls": 1,
            "tokens": 48, "cost_usd": 0.25, "result_text": "found.py", "stderr": "",
        })
        result = parse(selection, self.rows())
        self.assertEqual(result, {
            "error": None, "duration_s": 1.25, "first_tool": "Read",
            "tool_calls": ["Read", "mcp__codesage__search"], "used_codesage": True,
            "codesage_count": 1, "grep_count": 0, "unexpected_tools": [],
            "cost_usd": 0.25, "result_text": "found.py", "stderr": "",
        })

    def test_absent_empty_and_falsey_envelopes_remain_ignored(self):
        for module in (task_runner, selection):
            expected = parse(module, self.rows())
            for envelope in ({}, {"message": {}}, *({"message": value} for value in
                                                   (None, False, 0, "", []))):
                with self.subTest(parser=module.__name__, envelope=envelope):
                    rows = [self.rows()[0], {"type": "assistant", **envelope},
                            *self.rows()[1:]]
                    self.assertEqual(parse(module, rows), expected)

    def test_non_object_envelopes_preserve_surrounding_records(self):
        for module in (task_runner, selection):
            for with_codesage in (False, True):
                expected = parse(module, self.rows(), with_codesage=with_codesage)
                for value in NON_OBJECTS:
                    for position in range(4):
                        with self.subTest(parser=module.__name__, value=value,
                                          position=position, with_codesage=with_codesage):
                            rows = self.rows()
                            rows.insert(position, {"type": "assistant", "message": value})
                            self.assertEqual(parse(module, rows, with_codesage=with_codesage),
                                             expected)

    def test_malformed_envelope_cannot_erase_process_failure(self):
        for module in (task_runner, selection):
            for value in NON_OBJECTS:
                with self.subTest(parser=module.__name__, value=value):
                    rows = [{"type": "assistant", "message": value}, *self.rows()]
                    result = parse(module, rows, returncode=7, stderr="failed")
                    self.assertEqual(result["error"], "rc=7")
                    self.assertEqual(result["stderr"], "failed")
                    self.assertEqual(result["result_text"], "found.py")

    def test_task_runner_missing_and_failed_results_remain_errors(self):
        for value in NON_OBJECTS:
            with self.subTest(value=value):
                rows = [{"type": "assistant", "message": value}]
                self.assertEqual(parse(task_runner, rows)["error"], "missing_result")
                rows.append({"type": "result", "result": "", "is_error": True})
                self.assertEqual(parse(task_runner, rows)["error"], "result_error")

    def test_task_runner_race_detection_keeps_raw_stream(self):
        rows = [{"type": "assistant", "message": "No such tool available: mcp__codesage"}]
        self.assertTrue(parse(task_runner, rows)["raced"])
        self.assertIn("No such tool available", parse(task_runner, rows)["diag"])
        self.assertFalse(parse(task_runner, rows, with_codesage=False)["raced"])


if __name__ == "__main__":
    unittest.main()
