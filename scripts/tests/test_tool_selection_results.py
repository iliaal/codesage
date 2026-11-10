"""Failed Claude sessions must not become successful tool-selection measurements."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "bench" / "agent-tool-selection-harness.py"
SPEC = importlib.util.spec_from_file_location("tool_selection_results", SCRIPT)
harness = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(harness)


def assistant(tool="Read"):
    return {"type": "assistant", "message": {
        "content": [{"type": "tool_use", "name": tool}],
    }}


def terminal(**extra):
    return {"type": "result", "result": " target.py ",
            "total_cost_usd": "0.25", **extra}


def completed(rows, returncode=0):
    # The subprocess is always synthetic: no CLI, credentials, or model run.
    stdout = "log line\n{bad json\n" + "\n".join(json.dumps(row) for row in rows)
    return subprocess.CompletedProcess([], returncode, stdout, "diagnostic")


def parse(rows, *, returncode=0, with_codesage=True):
    with (patch.object(harness.subprocess, "run", return_value=completed(rows, returncode)) as run,
          patch.object(harness.time, "time", side_effect=[10, 11.25]),
          contextlib.redirect_stderr(io.StringIO())):
        result = harness.run_task(ROOT, "find target", with_codesage)
    run.assert_called_once()
    return result


class ToolSelectionResultTests(unittest.TestCase):
    def test_successful_results_keep_metrics_and_can_be_scored(self):
        expected = {
            "error": None, "duration_s": 1.25, "first_tool": "Read",
            "tool_calls": ["Read"], "used_codesage": False,
            "codesage_count": 0, "grep_count": 0, "unexpected_tools": [],
            "cost_usd": 0.25, "result_text": "target.py", "stderr": "diagnostic",
        }
        for fields in ({}, {"subtype": None}, {"subtype": "success", "is_error": False}):
            for with_codesage in (False, True):
                with self.subTest(fields=fields, with_codesage=with_codesage):
                    result = parse([assistant(), terminal(**fields)], with_codesage=with_codesage)
                    self.assertEqual(result, expected)
                    self.assertTrue(harness.score_task(result, ["target.py"])["found_expected"])

    def test_successful_empty_answer_is_a_miss_not_a_failed_session(self):
        result = parse([terminal(subtype="success", result="")])
        self.assertIsNone(result["error"])
        self.assertFalse(harness.score_task(result, ["target.py"])["found_expected"])

    def test_result_error_flag_rejects_even_a_correct_answer(self):
        for fields in ({"is_error": True}, {"is_error": True, "subtype": "success"}):
            with self.subTest(fields=fields):
                result = parse([assistant(), terminal(**fields)])
                self.assertEqual(result["error"], "result_error")
                self.assertEqual(result["result_text"], "target.py")
                self.assertEqual(result["cost_usd"], 0.25)
                self.assertEqual(result["tool_calls"], ["Read"])
                with self.assertRaisesRegex(ValueError, "cannot score failed run: result_error"):
                    harness.score_task(result, ["target.py"])

    def test_non_success_subtypes_reject_even_without_error_flag(self):
        for subtype in ("error_max_turns", "error_max_budget_usd", "error_during_execution"):
            for flag in ({}, {"is_error": False}):
                for with_codesage in (False, True):
                    with self.subTest(subtype=subtype, flag=flag, with_codesage=with_codesage):
                        result = parse([assistant(), terminal(subtype=subtype, **flag)],
                                       with_codesage=with_codesage)
                        self.assertEqual(result["error"], "result_error")

    def test_missing_terminal_record_is_not_a_valid_measurement(self):
        for rows in ([], [assistant()], [assistant(), {"type": "system", "subtype": "success"}]):
            with self.subTest(rows=rows):
                result = parse(rows)
                self.assertEqual(result["error"], "missing_result")
                with self.assertRaisesRegex(ValueError, "cannot score failed run: missing_result"):
                    harness.score_task(result, ["target.py"])

    def test_process_failure_keeps_precedence(self):
        for rows in ([terminal(subtype="success")], [terminal(is_error=True)], [assistant()]):
            with self.subTest(rows=rows):
                result = parse(rows, returncode=7)
                self.assertEqual(result["error"], "rc=7")
                self.assertEqual(result["stderr"], "diagnostic")

    def test_unexpected_tool_diagnostic_is_retained_with_each_error(self):
        for rows, expected in (([terminal()], "unexpected_tools=Bash"),
                               ([terminal(is_error=True)], "result_error;unexpected_tools=Bash"),
                               ([], "missing_result;unexpected_tools=Bash")):
            with self.subTest(expected=expected):
                result = parse([assistant("Bash"), *rows])
                self.assertEqual(result["error"], expected)
                self.assertEqual(result["unexpected_tools"], ["Bash"])

    def test_main_aborts_before_scoring_or_writing_failed_session(self):
        for rows, expected in (([assistant(), terminal(is_error=True)], "result_error"),
                               ([assistant(), terminal(subtype="error_max_turns")], "result_error"),
                               ([assistant()], "missing_result")):
            with self.subTest(expected=expected, rows=rows), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                corpus = root / "corpus.yaml"
                corpus.write_text(json.dumps({"project_root": tmp, "cases": [
                    {"id": "first", "query": "find target", "expected_files": ["target.py"]},
                    {"id": "second", "query": "find other", "expected_files": ["other.py"]},
                ]}))
                output = root / "reports"
                stdout, stderr = io.StringIO(), io.StringIO()
                with (patch.object(sys, "argv", [str(SCRIPT), str(corpus), "--output-dir", str(output)]),
                      patch.object(harness.subprocess, "run", return_value=completed(rows)) as run,
                      patch.object(harness, "score_task", wraps=harness.score_task) as score,
                      patch.object(harness, "render_scorecard", wraps=harness.render_scorecard) as render,
                      contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr)):
                    self.assertEqual(harness.main(), 1)
                run.assert_called_once()
                score.assert_not_called()
                render.assert_not_called()
                self.assertIn(f"task failed ({expected}); aborting", stderr.getvalue())
                self.assertEqual(stdout.getvalue(), "")
                self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
