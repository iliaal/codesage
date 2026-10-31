import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[2] / "bench/cleanup-orphan-models.sh"


class CleanupOrphanModelsTests(unittest.TestCase):
    def test_binary_selection_and_cleanup_context(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            caller = root / "caller space"
            target = caller / "target repo"
            (target / ".codesage").mkdir(parents=True)
            tools = caller / "tools space"
            tools.mkdir()
            binary = tools / "fake-codesage"
            binary.write_text('''#!/usr/bin/env python3
import json, os, sys
with open(os.environ["CALL_LOG"], "w") as stream:
    json.dump([os.getcwd(), sys.argv[1:]], stream)
sys.exit(int(os.environ["CHILD_STATUS"]))
''')
            binary.chmod(0o755)
            (tools / "codesage").symlink_to(binary.name)
            log = root / "call.json"
            relative = "./tools space/fake-codesage"
            # Only this capture executable runs; no index or model is touched.
            cases = [
                ("relative-option", ["--codesage-bin", relative], "missing", 0),
                ("relative-env", [], relative, 0),
                ("default-path", [], "", 0),
                ("absolute-env", [], str(binary), 0),
                ("absolute-option", ["--codesage-bin", str(binary)], "missing", 0),
                ("path-option", ["--codesage-bin", "fake-codesage"], "missing", 0),
                ("path-env", [], "fake-codesage", 0),
                ("child-failure", ["--codesage-bin", relative], "missing", 42),
            ]
            for name, options, env_binary, status in cases:
                with self.subTest(name=name):
                    log.unlink(missing_ok=True)
                    result = subprocess.run(
                        ["bash", str(SCRIPT), "target repo", "--dry-run", *options],
                        cwd=caller, capture_output=True, text=True,
                        env={**os.environ, "CODESAGE_BIN": env_binary,
                             "CALL_LOG": str(log), "CHILD_STATUS": str(status),
                             "PATH": str(tools) + os.pathsep + os.environ["PATH"]},
                    )
                    self.assertEqual(result.returncode, status, result.stdout + result.stderr)
                    self.assertEqual(json.loads(log.read_text()),
                                     [str(target), ["cleanup", "--dry-run"]])


if __name__ == "__main__":
    unittest.main()
