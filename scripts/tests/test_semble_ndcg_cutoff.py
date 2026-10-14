"""The NDCG report must name the cutoff it actually scores."""
import json
import math
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[2] / "bench" / "semble-ndcg-runner"


class NdcgCutoffTests(unittest.TestCase):
    def test_cli_labels_all_score_sinks_with_actual_cutoff(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus = root / "corpus" / "fixture"
            (corpus / ".codesage").mkdir(parents=True)
            with sqlite3.connect(corpus / ".codesage" / "index.db") as con:
                con.execute("CREATE TABLE semantic_files (file_path TEXT)")
                con.execute("INSERT INTO semantic_files VALUES ('gold.rs')")
            annotations = root / "annotations"
            annotations.mkdir()
            (annotations / "fixture.json").write_text(json.dumps([
                {"query": str(rank), "relevant": ["gold.rs"]}
                for rank in (1, 9, 10, 11)
            ]))
            repos = root / "repos.json"
            repos.write_text(json.dumps([{"name": "fixture", "language": "rust"}]))
            binary = root / "codesage-fixture"
            binary.write_text(
                "#!/usr/bin/env python3\nimport json, sys\n"
                "if '--version' in sys.argv:\n"
                "    print('codesage 0.0.0 (fixture)'); sys.exit(0)\n"
                "limit = int(sys.argv[sys.argv.index('--limit') + 1])\n"
                "rank = int(sys.argv[-1])\n"
                "paths = [f'noise-{i}.rs' for i in range(rank - 1)] + ['gold.rs']\n"
                "print(json.dumps({'results': [{'file_path': p} for p in paths[:limit]]}))\n"
            )
            binary.chmod(0o755)
            for cutoff in (None, 1, 9, 10, 11):
                with self.subTest(cutoff=cutoff):
                    k = 10 if cutoff is None else cutoff
                    artifact = root / "score.json"
                    command = [sys.executable, str(SCRIPT), "--corpus", str(root / "corpus"),
                               "--annotations", str(annotations), "--repos", str(repos),
                               "--codesage-bin", str(binary), "--json", str(artifact)]
                    if cutoff is not None:
                        command += ["--limit", str(cutoff)]
                    proc = subprocess.run(command, capture_output=True, text=True,
                                          check=True, timeout=20)
                    report = json.loads(proc.stdout)
                    self.assertEqual(report, json.loads(artifact.read_text()))
                    self.assertEqual(report["provenance"]["limit"], k)
                    metric = f"ndcg@{k}"
                    repo = report["per_repo"]["fixture"]
                    language = report["by_language"]["rust"]
                    expected = [1 / math.log2(rank + 1) if rank <= k else 0.0
                                for rank in (1, 9, 10, 11)]
                    for row, score in zip(repo["query_results"], expected):
                        self.assertEqual([key for key in row if key.startswith("ndcg@")], [metric])
                        self.assertAlmostEqual(row[metric], score)
                    for row in (repo, language):
                        self.assertEqual([key for key in row if key.startswith("ndcg@")], [metric])
                        self.assertEqual(row[metric], round(sum(expected) / len(expected), 4))
                    self.assertIn(f"{metric}={sum(expected) / len(expected):.3f}", proc.stderr)
                    if k != 10:
                        self.assertNotIn("ndcg@10", proc.stdout + proc.stderr)


if __name__ == "__main__":
    unittest.main()
