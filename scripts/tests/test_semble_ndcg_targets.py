"""File-ranked NDCG counts each annotated file once, regardless of spans."""

import json
import math
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[2] / "bench" / "semble-ndcg-runner"


class NdcgTargetTests(unittest.TestCase):
    def score(self, relevant, secondary, returned, limit=10):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = root / "corpus" / "fixture"
            (repo / ".codesage").mkdir(parents=True)
            with sqlite3.connect(repo / ".codesage" / "index.db") as con:
                con.execute("CREATE TABLE semantic_files (file_path TEXT)")
                con.execute("INSERT INTO semantic_files VALUES ('src/gold.rs')")
            annotations = root / "annotations"
            annotations.mkdir()
            (annotations / "fixture.json").write_text(json.dumps([
                {"query": "fixture query", "relevant": relevant, "secondary": secondary}
            ]))
            repos = root / "repos.json"
            repos.write_text(json.dumps([{"name": "fixture", "language": "rust"}]))
            binary = root / "codesage-fixture"
            binary.write_text(
                f"#!{sys.executable}\nimport json, sys\n"
                "if '--version' in sys.argv:\n"
                "    print('codesage 0.0.0 (fixture)'); sys.exit(0)\n"
                f"paths = {returned!r}\n"
                "print(json.dumps({'results': [{'file_path': p} for p in paths]}))\n"
            )
            binary.chmod(0o755)
            artifact = root / "score.json"
            proc = subprocess.run([
                sys.executable, str(SCRIPT), "--corpus", str(root / "corpus"),
                "--annotations", str(annotations), "--repos", str(repos),
                "--codesage-bin", str(binary), "--json", str(artifact),
                "--limit", str(limit),
            ], capture_output=True, text=True, timeout=20)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            report = json.loads(proc.stdout)
            self.assertEqual(report, json.loads(artifact.read_text()))
            return report, proc.stderr

    def assert_score(self, relevant, secondary, returned, expected, limit=10):
        report, stderr = self.score(relevant, secondary, returned, limit)
        metric = f"ndcg@{limit}"
        repo = report["per_repo"]["fixture"]
        query = repo["query_results"][0]
        self.assertEqual(query["status"], "ok")
        self.assertAlmostEqual(query[metric], expected)
        self.assertEqual(repo[metric], round(expected, 4))
        self.assertEqual(report["by_language"]["rust"][metric], round(expected, 4))
        self.assertIn(f"{metric}={expected:.3f}", stderr)
        self.assertEqual(repo["queries"], 1)
        self.assertEqual(report["warnings"], [])

    def test_duplicate_annotations_do_not_lower_a_perfect_file_hit(self):
        cases = [
            (["src/gold.rs", "src/gold.rs"], []),
            (["src/gold.rs:1-5", {"path": "src/gold.rs:9-12"}], []),
            ([{"path": "src/gold.rs:1-5"}], ["src/gold.rs:9-12"]),
            ([], ["src/gold.rs", {"path": "src/gold.rs"}]),
            (["src\\gold.rs"], ["src/gold.rs"]),
        ]
        for relevant, secondary in cases:
            with self.subTest(relevant=relevant, secondary=secondary):
                self.assert_score(relevant, secondary, ["src/gold.rs"], 1.0)

    def test_duplicates_do_not_change_partial_or_cutoff_scores(self):
        ideal = 1 + 1 / math.log2(3)
        for limit in (1, 2, 3):
            with self.subTest(limit=limit):
                # The repeated found file must not outweigh a distinct missing file.
                score = 0.0 if limit == 1 else (1 / math.log2(3)) / ideal
                self.assert_score(
                    ["src/gold.rs:1-5", "src/missing.rs"],
                    [{"path": "src/gold.rs:9-12"}],
                    ["noise.rs", "src/gold.rs"], score, limit,
                )

    def test_distinct_targets_and_result_deduplication_are_unchanged(self):
        cases = [
            (["src/gold.rs"], [], ["src/gold.rs"], 1.0, 10),
            (["src/gold.rs"], ["src/other.rs"],
             ["src/gold.rs", "src/gold.rs", "src/other.rs"], 1.0, 2),
            (["src/gold.rs"], ["src/other.rs"], ["src/gold.rs"],
             1 / (1 + 1 / math.log2(3)), 10),
            (["src/gold.rs"], ["other/gold.rs"], ["src/gold.rs"],
             1 / (1 + 1 / math.log2(3)), 10),
            (["src/gold.rs"], [], ["noise.rs"], 0.0, 10),
        ]
        for case in cases:
            with self.subTest(case=case):
                self.assert_score(*case)


if __name__ == "__main__":
    unittest.main()
