"""Reject invalid retrieval depths before invoking the external CRG engine."""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[2] / "bench" / "code-review-graph-bench-runner"


def load_runner():
    loader = importlib.machinery.SourceFileLoader("crg_bench_limit", str(SCRIPT))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


class CrgBenchLimitTests(unittest.TestCase):
    def test_cli_rejects_nonpositive_limits_before_reading_corpus(self):
        with tempfile.TemporaryDirectory() as directory:
            for limit in (0, -1, -10):
                with self.subTest(limit=limit):
                    proc = subprocess.run([
                        sys.executable, str(SCRIPT), str(Path(directory) / "missing.yaml"),
                        "--limit", str(limit),
                    ], capture_output=True, text=True, timeout=20)
                    self.assertEqual(proc.returncode, 2)
                    self.assertIn("--limit must be a positive integer", proc.stderr)
                    self.assertNotIn("Traceback", proc.stderr)
                    self.assertEqual(proc.stdout, "")

    def test_limits_are_checked_before_engine_calls_and_preserve_valid_runs(self):
        runner = load_runner()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus = root / "corpus.yaml"
            corpus.write_text(json.dumps({
                "project_root": str(root),
                "cases": [{"id": "one", "query": "find gold", "expected_files": ["gold.py"]}],
            }))
            for limit in (0, -1, None, 1, 10, 20):
                with self.subTest(limit=limit):
                    argv = [str(SCRIPT), str(corpus)]
                    if limit is not None:
                        argv += ["--limit", str(limit)]
                    stdout, stderr = io.StringIO(), io.StringIO()
                    with patch.object(sys, "argv", argv), \
                            patch.object(runner, "crg_version", return_value="fixture") as version, \
                            patch.object(runner, "project_head_sha", return_value="fixture"), \
                            patch.object(runner, "project_size", return_value=(1, 1)), \
                            patch.object(runner, "CrgMcpSession") as session, \
                            contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                        session.return_value.search.return_value = [str(root / "gold.py")]
                        if limit is not None and limit < 1:
                            with self.assertRaises(SystemExit) as raised:
                                runner.main()
                            self.assertEqual(raised.exception.code, 2)
                            version.assert_not_called()
                            session.assert_not_called()
                            self.assertEqual(stdout.getvalue(), "")
                        else:
                            self.assertEqual(runner.main(), 0)
                            k = 10 if limit is None else limit
                            session.return_value.search.assert_called_once_with("find gold", k)
                            session.return_value.close.assert_called_once_with()
                            self.assertIn(f"top-{k},", stdout.getvalue())
                            self.assertIn("miss_rate=0.0000 median_first=1 r5=1.0000 r10=1.0000", stdout.getvalue())


if __name__ == "__main__":
    unittest.main()
