import json
import os
from pathlib import Path
import shutil
import shlex
import subprocess
import sys
import tempfile
import unittest
import uuid


PLUGIN = Path(__file__).resolve().parents[1]


class CommandArgumentTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.plugin = self.root / "installed plugin 'quoted'"
        shutil.copytree(PLUGIN, self.plugin)
        self.marker = f"FORBIDDEN_{uuid.uuid4().hex}"
        self.record = self.root / "arguments.json"
        self.payload = self.root / "payload.json"

    def invoke(self, command, values):
        self.payload.write_text(json.dumps(values), encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(self.plugin / "bin/codesage-invoke"), command,
             str(self.payload)], cwd=self.root, capture_output=True, text=True,
            env={**os.environ, "ARGUMENT_RECORD": str(self.record)}, timeout=10)

    def recorder(self, command, status=0):
        (self.plugin / "bin" / f"codesage-{command}").write_text(
            "#!/bin/bash\n"
            "python3 - \"$@\" <<'PY'\n"
            "import json, os, pathlib, sys\n"
            "pathlib.Path(os.environ['ARGUMENT_RECORD']).write_text(json.dumps(sys.argv[1:]))\n"
            "PY\n"
            f"exit {status}\n")

    def test_all_commands_preserve_literal_argv_from_installed_plugin(self):
        values = ["ordinary", "space value", "", "single' double\" quote", "Ω\\path\nline",
                  f"x; touch {self.marker}", f"x && touch {self.marker}",
                  f"x | touch {self.marker}", f"$(touch {self.marker})",
                  f"`touch {self.marker}`", "--no-save"]
        for command in ("onboard", "eval", "bench"):
            with self.subTest(command=command):
                self.recorder(command)
                result = self.invoke(command, values)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(json.loads(self.record.read_text()), values)
                self.assertFalse((self.root / self.marker).exists())

    def test_injection_detector_observes_unsafe_shell_interpolation(self):
        self.recorder("bench")
        unsafe = shlex.join(["bash", str(self.plugin / "bin/codesage-bench"), "ordinary"])
        unsafe += f"; touch {self.marker}"
        subprocess.run(["bash", "-c", unsafe], cwd=self.root, check=True,
                       env={**os.environ, "ARGUMENT_RECORD": str(self.record)})
        self.assertTrue((self.root / self.marker).is_file())
        self.assertEqual(json.loads(self.record.read_text()), ["ordinary"])

    def test_invalid_argument_documents_never_launch_command(self):
        self.recorder("bench")
        for values in ({"argv": []}, "--no-save", [None], [42], ["nul\u0000value"],
                       ["surrogate\ud800"]):
            with self.subTest(values=repr(values)):
                result = self.invoke("bench", values)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn("arguments must be a JSON array of UTF-8 strings without NUL", result.stderr)
                self.assertFalse(self.record.exists())

    def test_unknown_command_never_launches_a_program(self):
        result = self.invoke("../anything", [])
        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid choice", result.stderr)
        self.assertFalse(self.record.exists())

    def test_missing_and_malformed_json_fail_without_traceback(self):
        self.payload.write_text("not json")
        for path in (self.payload, self.root / "missing.json"):
            with self.subTest(path=path):
                result = subprocess.run(
                    [sys.executable, str(self.plugin / "bin/codesage-invoke"), "bench", str(path)],
                    capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 2)
                self.assertIn("cannot read argument file", result.stderr)
                self.assertNotIn("Traceback", result.stderr)

    def test_command_exit_status_is_preserved(self):
        self.recorder("eval", status=23)
        result = self.invoke("eval", [])
        self.assertEqual(result.returncode, 23)

    def test_real_scripts_accept_paths_through_dispatcher(self):
        project = self.root / f"project space; $(touch {self.marker})"
        project.mkdir()
        corpus = self.root / "corpus 'quoted'"
        corpus.mkdir()
        corpus_file = corpus / f"{project.name}-session-eval.yaml"
        corpus_file.write_text("existing corpus\n")
        runner = self.root / f"runner && touch {self.marker}"
        runner.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, pathlib, sys\n"
            "pathlib.Path(os.environ['ARGUMENT_RECORD']).write_text(json.dumps(sys.argv[1:]))\n"
            "print('<!-- METRICS: cases=2 miss_rate=0.0 median_first=1 r5=1.0 r10=1.0 -->')\n")
        runner.chmod(0o755)
        values = ["--corpus-dir", str(corpus), "--runner", str(runner), "--no-save"]
        for command, arguments in (("bench", values),
                                   ("eval", [str(project), *values, "--no-extract"])):
            with self.subTest(command=command):
                result = self.invoke(command, arguments)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(json.loads(self.record.read_text()), [str(corpus_file)])
                self.assertIn("2", result.stdout)
                self.assertEqual(corpus_file.read_text(), "existing corpus\n")
        result = self.invoke("onboard", [str(project), "--codesage-bin", "/bin/false", "--hint-only"])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        hint = (project / ".claude/CLAUDE.md").read_text()
        project_line = next(line for line in hint.splitlines() if line.startswith("Project argument (JSON string):"))
        self.assertEqual(json.loads(project_line.split(": ", 1)[1]), str(project))
        self.assertFalse((self.root / self.marker).exists())
        self.assertFalse((project / self.marker).exists())

    def test_command_guidance_uses_file_transport_without_raw_shell_arguments(self):
        for command in ("onboard", "eval", "bench"):
            with self.subTest(command=command):
                text = (PLUGIN / "commands" / f"codesage-{command}.md").read_text()
                self.assertIn("references/command-arguments.md", text)
                self.assertNotRegex(text, r"(?m)^.*bin/codesage-[a-z-]+ \$ARGUMENTS$")


if __name__ == "__main__":
    unittest.main()
