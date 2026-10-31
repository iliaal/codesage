import os
from pathlib import Path
import subprocess
import tempfile
import unittest


BIN = Path(__file__).resolve().parents[1] / "bin"
METRICS = "<!-- METRICS: cases=2 miss_rate=0.0 median_first=1 r5=1.0 r10=1.0 -->"


class RunnerStatusTests(unittest.TestCase):
    def run_case(self, command, save, status):
        with tempfile.TemporaryDirectory(prefix="runner status ") as directory:
            root = Path(directory)
            project = root / "project"
            project.mkdir()
            corpus = root / "corpora"
            corpus.mkdir()
            source = corpus / "project-session-eval.yaml"
            source.write_text("existing corpus\n")
            runner = root / "runner"
            runner.write_text(f"#!/bin/bash\nprintf '%s\\n' '{METRICS}'\nprintf 'runner detail\\n' >&2\nexit {status}\n")
            runner.chmod(0o755)
            args = ["bash", str(BIN / command)]
            if command == "codesage-eval":
                args += [str(project), "--no-extract"]
            args += ["--corpus-dir", str(corpus), "--runner", str(runner)]
            if not save:
                args.append("--no-save")
            result = subprocess.run(args, env={**os.environ, "HOME": str(root / "home")},
                                    capture_output=True, text=True)
            output = result.stdout + result.stderr
            self.assertEqual(result.returncode, 1 if status else 0, output)
            if status:
                diagnostic = (f"RUNNER FAILED rc={status}" if command == "codesage-bench"
                              else f"bench runner failed (rc={status})")
                self.assertIn(diagnostic, output)
                self.assertNotIn("miss_rate=0.0", result.stdout)
                self.assertNotIn("effectiveness summary", result.stdout)
            else:
                self.assertNotIn("FAILED", output)
                self.assertNotIn("runner failed", output)
                self.assertIn("miss_rate", result.stdout)
            history = list((corpus / "history").glob("*.md"))
            self.assertEqual(len(history), int(save))
            if save:
                self.assertEqual(history[0].read_text(), METRICS + "\nrunner detail\n")
            self.assertEqual(source.read_text(), "existing corpus\n")

    def test_failure_status_in_both_commands_and_history_modes(self):
        for command in ("codesage-bench", "codesage-eval"):
            for save in (False, True):
                for status in (7, 42, 143):
                    with self.subTest(command=command, save=save, status=status):
                        self.run_case(command, save, status)

    def test_success_in_both_commands_and_history_modes(self):
        for command in ("codesage-bench", "codesage-eval"):
            for save in (False, True):
                with self.subTest(command=command, save=save):
                    self.run_case(command, save, 0)


if __name__ == "__main__":
    unittest.main()
