"""Reject invalid A/B retrieval depths before index reads or engine calls."""
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

SCRIPT = Path(__file__).resolve().parents[2] / "bench" / "semble-corpus-runner"


def load_runner():
    loader = importlib.machinery.SourceFileLoader("semble_corpus_limit", str(SCRIPT))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


class SembleCorpusLimitTests(unittest.TestCase):
    def test_cli_rejects_invalid_limits_before_reading_index_or_annotations(self):
        with tempfile.TemporaryDirectory() as directory:
            for limit in ("-1", "0", "-10", "not-an-integer"):
                with self.subTest(limit=limit):
                    proc = subprocess.run([
                        sys.executable, str(SCRIPT), directory,
                        str(Path(directory) / "missing.json"),
                        "--codesage-bin", "missing-codesage", "--limit", limit,
                    ], capture_output=True, text=True, timeout=20)
                    self.assertEqual(proc.returncode, 2)
                    self.assertIn("--limit", proc.stderr)
                    if limit != "not-an-integer":
                        self.assertIn("--limit must be a positive integer", proc.stderr)
                    self.assertNotIn("Traceback", proc.stderr)
                    self.assertNotIn("no codesage index", proc.stderr)
                    self.assertEqual(proc.stdout, "")

    def test_rejection_precedes_engine_calls_and_positive_limits_preserve_both_arms(self):
        runner = load_runner()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".codesage").mkdir()
            (root / ".codesage" / "index.db").touch()
            annotations = root / "annotations.json"
            annotations.write_text(json.dumps([
                {"query": "find gold", "relevant": ["gold.py"]},
            ]))
            for limit in (0, -1, None, 1, 2, 10):
                with self.subTest(limit=limit):
                    argv = [str(SCRIPT), str(root), str(annotations),
                            "--codesage-bin", "fixture-codesage"]
                    if limit is not None:
                        argv += ["--limit", str(limit)]
                    stdout, stderr = io.StringIO(), io.StringIO()
                    with patch.object(sys, "argv", argv), \
                            patch.object(runner.subprocess, "run") as run, \
                            contextlib.redirect_stdout(stdout), \
                            contextlib.redirect_stderr(stderr):
                        # Both supported search response shapes preserve deduplication.
                        rows = [{"file_path": "gold.py"}, {"file_path": "gold.py"}]
                        run.side_effect = [
                            subprocess.CompletedProcess([], 0, json.dumps(rows)),
                            subprocess.CompletedProcess([], 0, json.dumps({"results": rows})),
                        ]
                        if limit is not None and limit < 1:
                            with self.assertRaises(SystemExit) as raised:
                                runner.main()
                            self.assertEqual(raised.exception.code, 2)
                            run.assert_not_called()
                            self.assertEqual(stdout.getvalue(), "")
                        else:
                            self.assertEqual(runner.main(), 0)
                            self.assertEqual(run.call_count, 2)
                            for call, gate in zip(run.call_args_list, ("0", "1")):
                                self.assertEqual(call.args[0], [
                                    "fixture-codesage", "search", "--json", "--limit",
                                    str(10 if limit is None else limit), "find gold",
                                ])
                                self.assertEqual(call.kwargs["env"]["CODESAGE_QUALIFIED_NAME_BOOST"], gate)
                            payload = stdout.getvalue().split("METRICS_JSON ", 1)[1]
                            report = json.loads(payload)
                            self.assertEqual(report["moves"], [])
                            for arm in ("baseline", "cfg2"):
                                self.assertEqual(report[arm], {
                                    "n": 1, "hits": 1, "miss": 0, "miss_rate": 0.0,
                                    "mean_first_hit": 1.0, "mean_r5": 1.0, "mean_r10": 1.0,
                                })


if __name__ == "__main__":
    unittest.main()
