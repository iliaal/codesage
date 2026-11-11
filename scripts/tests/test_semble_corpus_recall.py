"""File recall must not weight files by their number of annotated spans."""

import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "bench" / "semble-corpus-runner"


class SembleCorpusRecallTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.runner = runpy.run_path(str(SCRIPT))

    def test_repeated_hit_and_miss_annotations_have_equal_file_weight(self):
        for expected in (["a.py", "a.py", "b.py"],
                         ["a.py", "b.py", "b.py"],
                         ["a.py", "b.py", "a.py", "a.py"]):
            for returned in (["a.py"], ["b.py"], ["a.py", "a.py"]):
                with self.subTest(expected=expected, returned=returned):
                    before = expected.copy()
                    self.assertEqual(self.runner["recall_at"](returned, expected, 10), 0.5)
                    self.assertEqual(expected, before)

    def test_unique_targets_empty_gold_and_suffix_matching_are_unchanged(self):
        cases = [
            ([], [], 0.0),
            (["a.py"], [], 0.0),
            ([], ["a.py"], 0.0),
            (["noise.py"], ["a.py", "b.py"], 0.0),
            (["a.py", "b.py"], ["a.py", "b.py"], 1.0),
            (["/repo/src/a.py"], ["src/a.py", "b.py"], 0.5),
            (["a.py"], ["src/a.py", "b.py"], 0.5),
            (["othera.py"], ["a.py"], 0.0),
            (["src/a.py"], ["src/a.py", "other/a.py"], 0.5),
        ]
        for returned, expected, recall in cases:
            with self.subTest(returned=returned, expected=expected):
                self.assertEqual(self.runner["recall_at"](returned, expected, 10), recall)

    def test_duplicate_targets_respect_both_recall_cutoffs(self):
        for k in (5, 10):
            for rank in (k - 1, k, k + 1):
                with self.subTest(k=k, rank=rank):
                    returned = [f"noise{i}.py" for i in range(rank - 1)] + ["a.py"]
                    self.assertEqual(
                        self.runner["recall_at"](returned, ["a.py", "a.py", "b.py"], k),
                        0.5 if rank <= k else 0.0,
                    )

    def test_score_rows_keep_annotations_but_use_file_recall(self):
        relevant = ["src/a.py", {"path": "src/a.py", "lines": [1, 2]},
                    {"path": "src/a.py", "lines": [5, 8]}, "src/b.py"]
        records = [{"query": "spans", "relevant": relevant, "secondary": ["c.py"]}]
        with mock.patch("subprocess.run", return_value=subprocess.CompletedProcess(
                [], 0, json.dumps([{"file_path": "/repo/src/a.py"}]))):
            result = self.runner["score_corpus"]("codesage", ROOT, records, 10, {})
        self.assertEqual(result["rows"], [{
            "query": "spans", "expected": ["src/a.py"] * 3 + ["src/b.py"],
            "returned_top3": ["/repo/src/a.py"], "rank": 1, "r5": 0.5, "r10": 0.5,
        }])
        self.assertEqual(records[0]["relevant"], relevant)
        self.assertEqual(result["mean_r5"], 0.5)
        self.assertEqual(result["mean_r10"], 0.5)

    def test_cli_reports_unique_file_recall_for_both_arms(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".codesage").mkdir()
            (root / ".codesage" / "index.db").touch()
            annotations = root / "annotations.json"
            annotations.write_text(json.dumps([{
                "query": "spans", "relevant": [
                    "src/a.py", {"path": "src/a.py", "lines": [5, 8]}, "src/b.py",
                ],
            }]), encoding="utf-8")
            binary = root / "codesage"
            binary.write_text(f"#!{sys.executable}\n" +
                              "import json, os\n"
                              "arm = os.environ['CODESAGE_QUALIFIED_NAME_BOOST']\n"
                              "paths = ['src/a.py'] if arm == '0' else ['src/b.py']\n"
                              "print(json.dumps([{'file_path': p} for p in paths]))\n",
                              encoding="utf-8")
            binary.chmod(0o755)
            result = subprocess.run(
                [str(SCRIPT), str(root), str(annotations), "--codesage-bin", str(binary)],
                env=os.environ.copy(), text=True, capture_output=True, timeout=30,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stderr, "")
            expected_metrics = {
                "n": 1, "hits": 1, "miss": 0, "miss_rate": 0.0,
                "mean_first_hit": 1.0, "mean_r5": 0.5, "mean_r10": 0.5,
            }
            for label in ("baseline", "cfg2"):
                self.assertIn(
                    f"{label:8s} | n=  1 | miss= 0 (0.0%) | mean first-hit 1.00 | "
                    "r@5 0.500 | r@10 0.500\n", result.stdout,
                )
            metrics = json.loads(result.stdout.split("METRICS_JSON ", 1)[1])
            self.assertEqual(metrics, {
                "repo": root.name, "baseline": expected_metrics,
                "cfg2": expected_metrics, "moves": [],
            })


if __name__ == "__main__":
    unittest.main()
