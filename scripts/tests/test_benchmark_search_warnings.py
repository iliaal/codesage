"""Exercise query-labelled diagnostics without a model or a 120-second wait."""

import contextlib
import hashlib
import io
from pathlib import Path
import runpy
import subprocess
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
QUERIES = ("first", "second", "quote'\nnext\t文")


class BenchmarkSearchWarningTests(unittest.TestCase):
    def test_semble_failures_identify_query_and_reason(self):
        runner = runpy.run_path(str(ROOT / "bench/semble-corpus-runner"))
        cases = (
            (subprocess.TimeoutExpired("codesage", 120), "failed", "timed out after 120s"),
            (subprocess.SubprocessError("broken\nprocess"), "errored", repr("broken\nprocess")),
        )
        for query in QUERIES:
            for error, label, reason in cases:
                with self.subTest(query=query, error=error):
                    stderr = io.StringIO()
                    with mock.patch("subprocess.run", side_effect=error), contextlib.redirect_stderr(stderr):
                        result = runner["run_search"]("codesage", ROOT, query, 10, {})
                    self.assertEqual(result, [])
                    self.assertEqual(stderr.getvalue(),
                                     f"[warn] codesage search {label}: query={query!r}; reason={reason}\n")

    def test_controlled_failures_preserve_results_and_runtime(self):
        runner = runpy.run_path(str(ROOT / "bench/codesage-bench-runner"))
        cases = (
            ("timeout", "codesage search failed", "timed out after 120s", "embedding model loaded\n"),
            ("error", "codesage search errored", repr("broken\nprocess"), ""),
            ("rc=7", "codesage search failed rc=7", repr("failed\nprocess"), "failed\nprocess"),
            ("invalid-runtime", "query runtime", repr("private embedding initialization was not observed"), ""),
            ("degraded", "degraded search", repr("embedding model loaded\ncross-encoder rerank failed\n"),
             "embedding model loaded\ncross-encoder rerank failed\n"),
        )
        for query in QUERIES:
            for status, label, reason, child_stderr in cases:
                with self.subTest(query=query, status=status):
                    child = mock.Mock(pid=42, returncode=7 if status == "rc=7" else 0)
                    child.communicate.return_value = ("partial-output", child_stderr)
                    if status == "timeout":
                        child.communicate.side_effect = [subprocess.TimeoutExpired("codesage", 120),
                                                         ("partial-output", child_stderr)]
                    stderr = io.StringIO()
                    with (tempfile.TemporaryDirectory() as project,
                          mock.patch("subprocess.Popen", return_value=child) as popen,
                          contextlib.redirect_stderr(stderr)):
                        if status == "error":
                            popen.side_effect = OSError("broken\nprocess")
                        output, error, runtime = runner["run_codesage_search"]("/codesage", Path(project), query, 10)
                    self.assertEqual(error, status)
                    self.assertEqual(output, "" if status == "error" else "partial-output")
                    self.assertEqual(stderr.getvalue(),
                                     f"[warn] {label}: query={query!r}; reason={reason}\n")
                    self.assertEqual(runtime["command"], ["/codesage", "search", "--json", "--limit", "10", query])
                    if status != "error":
                        self.assertEqual(runtime["stderr"], child_stderr)
                        self.assertEqual(runtime["stderr_sha256"], hashlib.sha256(child_stderr.encode()).hexdigest())
                    if status == "timeout":
                        child.kill.assert_called_once_with()
                        self.assertEqual(child.communicate.call_args_list, [mock.call(timeout=120), mock.call()])
                    else:
                        child.kill.assert_not_called()

    def test_clean_search_keeps_stdout_and_has_no_warning(self):
        runner = runpy.run_path(str(ROOT / "bench/codesage-bench-runner"))
        child = mock.Mock(pid=42, returncode=0)
        child.communicate.return_value = ('[{"file_path":"target.py"}]', "embedding model loaded\n")
        stderr = io.StringIO()
        with (tempfile.TemporaryDirectory() as project, mock.patch("subprocess.Popen", return_value=child),
              contextlib.redirect_stderr(stderr)):
            output, error, _ = runner["run_codesage_search"]("/codesage", Path(project), "good", 10)
        self.assertEqual(output, '[{"file_path":"target.py"}]')
        self.assertIsNone(error)
        self.assertEqual(stderr.getvalue(), "")


if __name__ == "__main__":
    unittest.main()
