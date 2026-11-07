import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "refresh-onboarded-repos.sh"


class RefreshOnboardedReposTests(unittest.TestCase):
    def test_hint_refresh_preserves_config_and_index_policy(self):
        cases = (([], 0), (["--no-index"], 0),
                 (["--full", "--no-semantic"], 0), ([], 75))
        for arguments, index_status in cases:
            with self.subTest(arguments=arguments, index_status=index_status), \
                    tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                home = root / "home"
                project = home / "projects" / "target with spaces"
                subprocess.run(["git", "init", "-q", str(project)], check=True)
                (project / ".codesage").mkdir()
                config = project / ".codesage/config.toml"
                original_config = b'[project]\nname = "target"\n\n[embedding]\ndevice = "cpu"\n'
                config.write_bytes(original_config)
                hint = project / ".claude/CLAUDE.md"
                hint.parent.mkdir()
                hint.write_text("outdated hint\n")
                copied = root / "checkout/scripts" / SCRIPT.name
                copied.parent.mkdir(parents=True)
                shutil.copyfile(SCRIPT, copied)
                # Exercise the real helper: a no-op stub hides its init/index
                # side effects and default GPU configuration rewrite.
                shutil.copytree(SCRIPT.parents[1] / "plugins/codesage-tools",
                                root / "checkout/plugins/codesage-tools")
                tools = root / "tools with spaces"
                tools.mkdir()
                binary = tools / "codesage"
                binary.write_text('''#!/usr/bin/env python3
import json, os, sys
with open(os.environ["CALL_LOG"], "a") as stream:
    stream.write(json.dumps([os.getcwd(), sys.argv[1:]]) + "\\n")
if sys.argv[1] == "index":
    sys.exit(int(os.environ["INDEX_STATUS"]))
''')
                binary.chmod(0o755)
                log = root / "calls.jsonl"
                result = subprocess.run(
                    ["bash", str(copied), *arguments], cwd=root,
                    capture_output=True, text=True, timeout=30,
                    env={**os.environ, "HOME": str(home), "CALL_LOG": str(log),
                         "INDEX_STATUS": str(index_status),
                         "PATH": "tools with spaces" + os.pathsep + os.environ["PATH"]},
                )
                self.assertEqual(config.read_bytes(), original_config)
                self.assertEqual(result.returncode, index_status, result.stdout + result.stderr)
                expected = [] if arguments == ["--no-index"] else [["index", *arguments]]
                expected += [["install-hooks"], ["doctor"]]
                calls = [json.loads(line) for line in log.read_text().splitlines()]
                self.assertEqual(calls, [[str(project), command] for command in expected])
                self.assertNotIn("outdated hint", hint.read_text())
                self.assertIn("Project argument (JSON string)", hint.read_text())

    def test_preserves_selected_binary_and_project_paths(self):
        for entry in ("tools", "./tools", "tools with spaces", ".", "", "absolute", "newline"):
            with self.subTest(path_entry=entry), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                caller = root / "caller"
                caller.mkdir()
                home = root / "home"
                project = home / "projects" / ("target\nwith newline\n" if entry == "newline" else "target")
                (project / ".codesage").mkdir(parents=True)
                (project / ".git").mkdir()
                copied = root / "checkout" / "scripts" / SCRIPT.name
                copied.parent.mkdir(parents=True)
                shutil.copyfile(SCRIPT, copied)
                onboard = root / "checkout/plugins/codesage-tools/bin/codesage-onboard"
                onboard.parent.mkdir(parents=True)
                onboard.write_text("#!/bin/bash\nexit 0\n")
                onboard.chmod(0o755)
                relative = "tools" if entry == "absolute" else entry
                tools = caller / relative
                tools.mkdir(parents=True, exist_ok=True)
                binary = tools / "codesage"
                binary.write_text('''#!/usr/bin/env python3
import json, os, sys
with open(os.environ["CALL_LOG"], "a") as stream:
    stream.write(json.dumps([os.getcwd(), sys.argv[1:]]) + "\\n")
''')
                binary.chmod(0o755)
                # A target-local executable must never replace the selected one.
                decoy = project / relative / "codesage"
                decoy.parent.mkdir(parents=True, exist_ok=True)
                decoy.write_text("#!/bin/bash\nexit 99\n")
                decoy.chmod(0o755)
                log = root / "calls.jsonl"
                path_entry = str(tools) if entry == "absolute" else entry
                result = subprocess.run(
                    ["bash", str(copied), "--full", "--no-semantic"],
                    cwd=caller, capture_output=True, text=True,
                    env={**os.environ, "HOME": str(home), "CALL_LOG": str(log),
                         "PATH": path_entry + os.pathsep + os.environ["PATH"]},
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                calls = [json.loads(line) for line in log.read_text().splitlines()]
                self.assertEqual(calls, [
                    [str(project), ["index", "--full", "--no-semantic"]],
                    [str(project), ["install-hooks"]],
                ])

    def test_refreshes_hooks_for_repositories_and_linked_worktrees_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            home = root / "home"
            projects = home / "projects"
            projects.mkdir(parents=True)
            ordinary = projects / "ordinary"
            linked = projects / "linked worktree"
            nongit = projects / "not a repo"
            subprocess.run(["git", "init", "-q", str(ordinary)], check=True)
            subprocess.run(["git", "-C", str(ordinary), "-c", "user.name=Fixture",
                            "-c", "user.email=fixture@example.invalid", "commit", "-q",
                            "--allow-empty", "-m", "fixture"], check=True)
            subprocess.run(["git", "-C", str(ordinary), "worktree", "add", "-q", "--detach", str(linked)], check=True)
            nongit.mkdir()
            for project in (ordinary, linked, nongit):
                (project / ".codesage").mkdir()
            # Copy only the production entrypoint; external tools are test doubles.
            copied = root / "checkout" / "scripts" / SCRIPT.name
            copied.parent.mkdir(parents=True)
            shutil.copyfile(SCRIPT, copied)
            onboard = root / "checkout/plugins/codesage-tools/bin/codesage-onboard"
            onboard.parent.mkdir(parents=True)
            onboard.write_text("#!/bin/bash\nexit 0\n")
            onboard.chmod(0o755)
            tools = root / "tools"
            tools.mkdir()
            binary = tools / "codesage"
            binary.write_text('''#!/usr/bin/env python3
import json, os, sys
with open(os.environ["CALL_LOG"], "a") as stream:
    stream.write(json.dumps([os.getcwd(), sys.argv[1:]]) + "\\n")
''')
            binary.chmod(0o755)
            log = root / "calls.jsonl"
            result = subprocess.run(
                ["bash", str(copied), "--no-index"], capture_output=True, text=True,
                env={**os.environ, "HOME": str(home), "CALL_LOG": str(log),
                     "PATH": str(tools) + os.pathsep + os.environ["PATH"]},
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(sorted(calls), sorted([
                [str(ordinary), ["install-hooks"]],
                [str(linked), ["install-hooks"]],
            ]))


if __name__ == "__main__":
    unittest.main()
