"""Exercise runner selection from a checkout unrelated to HOME or cwd."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SOURCE = Path(__file__).parents[2] / "bench" / "ablation.py"


class AblationRunnerPathTests(unittest.TestCase):
    def run_case(self, override):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            checkout = root / "checkout with spaces" / "bench"
            checkout.mkdir(parents=True)
            script = checkout / "ablation.py"
            shutil.copyfile(SOURCE, script)
            elsewhere = root / "elsewhere"
            elsewhere.mkdir()
            home = root / "home"
            home.mkdir()
            capture = root / "calls.jsonl"
            sibling = checkout / "codesage-bench-runner"
            custom = root / "custom runner.py"
            for runner, label in ((sibling, "sibling"), (custom, "custom")):
                runner.write_text(
                    "import json, os, sys\n"
                    "with open(os.environ['CAPTURE'], 'a') as f:\n"
                    f"    f.write(json.dumps([{label!r}, sys.argv[1:]]) + '\\n')\n"
                    "print('<!-- METRICS: miss_rate=0 median_first=1 r5=1 r10=1 search_failures=0 -->')\n",
                    encoding="utf-8",
                )
            args = [sys.executable, str(script), "corpus.yaml", "--arms", "baseline"]
            if override:
                args += ["--runner", str(custom)]
            result = subprocess.run(args, cwd=elsewhere,
                                    env={**os.environ, "HOME": str(home), "CAPTURE": str(capture)},
                                    text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = [json.loads(line) for line in capture.read_text().splitlines()]
            self.assertEqual(calls, [["custom" if override else "sibling",
                                     ["corpus.yaml", "--codesage-bin", "codesage", "--limit", "10"]]])

    def test_default_uses_runner_beside_script(self):
        self.run_case(False)

    def test_explicit_runner_still_overrides_sibling(self):
        self.run_case(True)


if __name__ == "__main__":
    unittest.main()
