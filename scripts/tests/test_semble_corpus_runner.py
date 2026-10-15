"""Exercise the A/B runner through real subprocesses without an inference model."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
KNOB = "CODESAGE_QUALIFIED_NAME_BOOST"


class SembleCorpusRunnerTests(unittest.TestCase):
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
