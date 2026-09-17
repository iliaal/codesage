import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "bin" / "codesage-onboard"


class OnboardBinaryTests(unittest.TestCase):
    def test_relative_binary_with_spaces_survives_project_cwd_and_registration(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "different project"
            project.mkdir()
            (project / ".git").mkdir()
            binary = root / "tools with spaces" / "codesage"
            binary.parent.mkdir()
            log = root / "calls.jsonl"
            binary.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
with open(os.environ["CALL_LOG"], "a") as stream:
    stream.write(json.dumps(["codesage", os.getcwd(), sys.argv[1:]]) + "\\n")
if sys.argv[1] == "init":
    pathlib.Path(".codesage").mkdir()
    pathlib.Path(".codesage/config.toml").write_text('device = "gpu"\\n')
''')
            binary.chmod(0o755)
            claude = binary.parent / "claude"
            claude.write_text('''#!/usr/bin/env python3
import json, os, sys
with open(os.environ["CALL_LOG"], "a") as stream:
    stream.write(json.dumps(["claude", os.getcwd(), sys.argv[1:]]) + "\\n")
''')
            claude.chmod(0o755)
            env = {**os.environ, "PATH": str(binary.parent) + os.pathsep + os.environ["PATH"],
                   "CALL_LOG": str(log)}
            result = subprocess.run(
                ["bash", str(SCRIPT), str(project), "--codesage-bin",
                 "tools with spaces/codesage", "--device", "cpu", "--no-hint"],
                cwd=root, env=env, capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            registration = next(row for row in calls if row[0] == "claude" and row[2][:2] == ["mcp", "add"])
            self.assertEqual(registration[2], ["mcp", "add", "--scope", "user", "codesage", "--", str(binary), "mcp"])
            operations = [row for row in calls if row[0] == "codesage"]
            self.assertTrue(operations)
            self.assertEqual(operations[0][2], ["init"])
            self.assertTrue(all(row[1] == str(project) for row in operations))
            self.assertEqual((project / ".codesage/config.toml").read_text(), 'device = "cpu"\n')


if __name__ == "__main__":
    unittest.main()
