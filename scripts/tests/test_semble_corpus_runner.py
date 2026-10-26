"""Exercise the A/B runner through real subprocesses without an inference model."""

import json
import contextlib
import io
import runpy
from unittest import mock
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
KNOB = "CODESAGE_QUALIFIED_NAME_BOOST"


class SembleCorpusRunnerTests(unittest.TestCase):
    def test_result_page_validation(self):
        runner = runpy.run_path(str(ROOT / "bench/semble-corpus-runner"))
        bad_rows = [None, [], "row", 7, False, {}, {"file_path": ""}]
        bad_rows += [{"file_path": value} for value in
                     (None, False, True, 0, 1, 1.5, [], ["a.py"], {}, {"x": 1})]
        pages = [None, True, 1, "page", {}, {"results": None},
                 {"results": {}}, {"results": "page"}, {"results": 1}]
        for row in bad_rows:
            for rows in ([row], [{"file_path": "target.py"}, row],
                         [row, {"file_path": "target.py"}]):
                pages.extend((rows, {"results": rows}))
        for page in pages:
            with self.subTest(page=page):
                stderr = io.StringIO()
                response = subprocess.CompletedProcess([], 0, json.dumps(page), "")
                with mock.patch("subprocess.run", return_value=response), contextlib.redirect_stderr(stderr):
                    paths = runner["run_search"]("codesage", ROOT, "query", 10, {})
                self.assertEqual(paths, [])
                self.assertEqual(stderr.getvalue(),
                                 "[warn] codesage search returned malformed results: 'query'\n")

    def test_valid_result_pages_preserve_path_order(self):
        runner = runpy.run_path(str(ROOT / "bench/semble-corpus-runner"))
        paths = ["target.py", " ", "src/é.py", "./target.py", "with spaces.py"]
        rows = [{"file_path": p} for p in paths + paths[:2]]
        for page, expected in (([], []), ({"results": []}, []),
                               (rows, paths), ({"results": rows}, paths)):
            with self.subTest(page=page):
                stderr = io.StringIO()
                response = subprocess.CompletedProcess([], 0, json.dumps(page), "")
                with mock.patch("subprocess.run", return_value=response), contextlib.redirect_stderr(stderr):
                    actual = runner["run_search"]("codesage", ROOT, "query", 10, {})
                self.assertEqual(actual, expected)
                self.assertEqual(stderr.getvalue(), "")

    def test_malformed_page_discards_partial_hit_and_continues_both_arms(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / ".codesage").mkdir()
            (root / ".codesage/index.db").touch()
            annotations = root / "annotations.json"
            annotations.write_text(json.dumps([
                {"query": "bad", "relevant": ["target.py"]},
                {"query": "good", "relevant": ["target.py"]},
            ]))
            binary = root / "codesage"
            binary.write_text(f"#!{sys.executable}\n" +
                              "import json, sys\n"
                              "rows = [{'file_path': 'target.py'}]\n"
                              "if sys.argv[-1] == 'bad': rows.append({'file_path': []})\n"
                              "print(json.dumps({'results': rows}))\n")
            binary.chmod(0o755)
            result = subprocess.run(
                [sys.executable, str(ROOT / "bench/semble-corpus-runner"),
                 str(root), str(annotations), "--codesage-bin", str(binary)],
                text=True, capture_output=True, timeout=30,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stderr.count("returned malformed results: 'bad'"), 2)
            metrics = json.loads(next(line.removeprefix("METRICS_JSON ")
                                     for line in result.stdout.splitlines()
                                     if line.startswith("METRICS_JSON ")))
            for arm in ("baseline", "cfg2"):
                self.assertEqual(metrics[arm], {
                    "n": 2, "hits": 1, "miss": 1, "miss_rate": 0.5,
                    "mean_first_hit": 1.0, "mean_r5": 0.5, "mean_r10": 0.5,
                })
            self.assertEqual(metrics["moves"], [])

    def test_each_arm_overrides_inherited_qualified_name_boost(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / ".codesage").mkdir()
            (root / ".codesage/index.db").touch()
            annotations = root / "annotations.json"
            annotations.write_text(json.dumps([
                {"query": "first query", "relevant": ["target.py"]},
                {"query": "second query", "relevant": ["target.py"]},
            ]))
            calls = root / "calls.jsonl"
            binary = root / "codesage"
            binary.write_text(f"#!{sys.executable}\n" + '''import json
import os
import sys
from pathlib import Path
knob = os.environ.get("CODESAGE_QUALIFIED_NAME_BOOST")
with Path(os.environ["TEST_CALLS"]).open("a") as log:
    log.write(json.dumps({"knob": knob, "query": sys.argv[-1],
                          "other": os.environ.get("CODESAGE_MENTION_ANCHOR")}) + "\\n")
# Mirror the product's accepted opt-in spellings for a deterministic rank oracle.
enabled = knob in {"1", "true", "TRUE", "True", "yes"}
paths = ["target.py", "other.py"] if enabled else ["other.py", "target.py"]
print(json.dumps({"results": [{"file_path": p} for p in paths]}))
''')
            binary.chmod(0o755)
            for inherited in (None, "", "0", "false", "1", "true", "TRUE", "True", "yes"):
                with self.subTest(inherited=inherited):
                    calls.write_text("")
                    env = {**os.environ, "TEST_CALLS": str(calls),
                           "CODESAGE_MENTION_ANCHOR": "0"}
                    env.pop(KNOB, None)
                    if inherited is not None:
                        env[KNOB] = inherited
                    result = subprocess.run(
                        [sys.executable, str(ROOT / "bench/semble-corpus-runner"),
                         str(root), str(annotations), "--codesage-bin", str(binary)],
                        env=env, text=True, capture_output=True, timeout=30,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    observed = [json.loads(line) for line in calls.read_text().splitlines()]
                    self.assertEqual(observed, [
                        {"knob": value, "query": query, "other": "0"}
                        for value in ("0", "1")
                        for query in ("first query", "second query")
                    ])
                    metrics = json.loads(next(line.removeprefix("METRICS_JSON ")
                                              for line in result.stdout.splitlines()
                                              if line.startswith("METRICS_JSON ")))
                    self.assertEqual(metrics["baseline"]["mean_first_hit"], 2)
                    self.assertEqual(metrics["cfg2"]["mean_first_hit"], 1)
                    self.assertEqual(metrics["moves"], [
                        {"query": query, "before": 2, "after": 1}
                        for query in ("first query", "second query")
                    ])


if __name__ == "__main__":
    unittest.main()
