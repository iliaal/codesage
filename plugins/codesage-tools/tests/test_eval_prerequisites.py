import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "bin" / "codesage-eval"


class EvalPrerequisiteTests(unittest.TestCase):
    def test_no_extract_scores_without_extractor_or_session_history(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            project.mkdir()
            corpus = root / "corpora"
            corpus.mkdir()
            yaml = corpus / "project-session-eval.yaml"
            yaml.write_text("existing corpus\n")
            runner = root / "runner"
            runner.write_text('''#!/usr/bin/env python3
import pathlib, sys
assert pathlib.Path(sys.argv[1]).read_text() == "existing corpus\\n"
print("<!-- METRICS: cases=2 miss_rate=0.0 median_first=1 r5=1.0 r10=1.0 -->")
''')
            runner.chmod(0o755)
            env = {**os.environ, "HOME": str(root / "empty home")}
            args = ["bash", str(SCRIPT), str(project), "--corpus-dir", str(corpus),
                    "--runner", str(runner), "--extractor", str(root / "missing.py"), "--no-save"]
            result = subprocess.run([*args, "--no-extract"], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("cases mined:  2", result.stdout)
            self.assertIn("recall@10:    1.0", result.stdout)
            self.assertEqual(yaml.read_text(), "existing corpus\n")
            extraction = subprocess.run(args, env=env, capture_output=True, text=True)
            self.assertNotEqual(extraction.returncode, 0)
            self.assertIn("extract-eval-cases.py not found", extraction.stderr)

    def test_extraction_uses_extractor_and_requires_history(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            project.mkdir()
            corpus = root / "corpora"
            runner = root / "runner"
            runner.write_text('''#!/usr/bin/env python3
import pathlib, sys
assert pathlib.Path(sys.argv[1]).read_text() == "mined corpus\\n"
print("<!-- METRICS: cases=3 miss_rate=0.0 median_first=1 r5=1.0 r10=1.0 -->")
''')
            runner.chmod(0o755)
            extractor = root / "extractor.py"
            extractor.write_text('''import pathlib, sys
assert pathlib.Path(sys.argv[1]).is_dir()
assert pathlib.Path(sys.argv[2]).is_dir()
pathlib.Path(sys.argv[sys.argv.index("--yaml") + 1]).write_text("mined corpus\\n")
''')
            home = root / "home"
            args = ["bash", str(SCRIPT), str(project), "--corpus-dir", str(corpus),
                    "--runner", str(runner), "--extractor", str(extractor), "--no-save"]
            env = {**os.environ, "HOME": str(home)}
            missing = subprocess.run(args, env=env, capture_output=True, text=True)
            self.assertNotEqual(missing.returncode, 0)
            self.assertIn("no Claude Code session history", missing.stderr)
            (home / ".claude" / "projects" / str(project).replace("/", "-")).mkdir(parents=True)
            result = subprocess.run(args, env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("cases mined:  3", result.stdout)
            self.assertEqual((corpus / "project-session-eval.yaml").read_text(), "mined corpus\n")


if __name__ == "__main__":
    unittest.main()
