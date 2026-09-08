import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "sanity-check.sh"


class SanityRecipeTests(unittest.TestCase):
    def test_release_skill_has_a_repository_owned_workflow(self):
        root = SCRIPT.parent.parent
        skill = root / ".agents/skills/release/SKILL.md"
        workflow = skill.parent / "references/workflow.md"
        self.assertIn("(references/workflow.md)", skill.read_text())
        self.assertNotIn(".claude/commands/release.md", skill.read_text())
        self.assertTrue(workflow.is_file())
        self.assertIn("bash scripts/sanity-check.sh --cuda", workflow.read_text())
        self.assertIn("--include-approved-prose", workflow.read_text())

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        scripts = self.root / "scripts"
        scripts.mkdir()
        shutil.copyfile(SCRIPT, scripts / SCRIPT.name)
        for filename, label in (
            ("check-changelog.py", "changelog"),
            ("check-plugin-versions.py", "plugins"),
        ):
            (scripts / filename).write_text(
                "import os, sys\n"
                f"label = {label!r}\n"
                "with open(os.environ['RECIPE_LOG'], 'a') as log:\n"
                "    print(label, file=log)\n"
                "sys.exit(19 if os.environ.get('FAIL_GATE') == label else 0)\n"
            )
        (scripts / "regression-tests.sh").write_text(
            "printf 'regressions\\n' >>\"$RECIPE_LOG\"\n"
            '[[ "${FAIL_GATE:-}" != regressions ]]\n'
        )
        binary_dir = self.root / "bin"
        binary_dir.mkdir()
        for name in ("cargo", "shellcheck", "shfmt"):
            target = binary_dir / name
            target.write_text(
                "#!/bin/bash\n"
                'label="${0##*/} $*"\n'
                'printf "%s\\n" "$label" >>"$RECIPE_LOG"\n'
                '[[ "${FAIL_GATE:-}" != "$label" ]]\n'
            )
            target.chmod(0o755)
        self.log = self.root / "gates.log"
        self.env = {
            **os.environ,
            "PATH": f"{binary_dir}:{os.environ['PATH']}",
            "RECIPE_LOG": str(self.log),
        }

    def run_recipe(self, *args, failure=""):
        self.log.unlink(missing_ok=True)
        result = subprocess.run(
            ["bash", "scripts/sanity-check.sh", *args],
            cwd=self.root,
            env={**self.env, "FAIL_GATE": failure},
            text=True,
            capture_output=True,
        )
        lines = self.log.read_text().splitlines() if self.log.exists() else []
        return result, lines

    def test_release_recipe_includes_cuda_and_all_gates(self):
        result, lines = self.run_recipe("--cuda")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(lines[:5], [
            "changelog",
            "plugins",
            "cargo fmt --all -- --check",
            "cargo clippy --workspace --all-targets -- -D warnings",
            "cargo clippy --workspace --all-targets --features codesage/cuda -- -D warnings",
        ])
        self.assertTrue(lines[5].startswith("shellcheck "), lines)
        self.assertTrue(lines[6].startswith("shfmt -d "), lines)
        self.assertEqual(lines[7:], ["cargo test --workspace", "regressions"])

    def test_every_gate_failure_stops_before_later_gates(self):
        result, baseline = self.run_recipe("--cuda")
        self.assertEqual(result.returncode, 0)
        for position, gate in enumerate(baseline):
            with self.subTest(gate=gate):
                result, lines = self.run_recipe("--cuda", failure=gate)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(lines, baseline[:position + 1])
                self.assertNotIn("sanity checks passed", result.stdout)

    def test_cuda_is_opt_in_and_fast_skips_only_test_suites(self):
        result, lines = self.run_recipe("--fast")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(lines[:2], ["changelog", "plugins"])
        self.assertFalse(any("codesage/cuda" in line for line in lines))
        self.assertFalse(any(line.startswith("cargo test") for line in lines))
        self.assertNotIn("regressions", lines)
        self.assertTrue(any(line.startswith("shellcheck ") for line in lines))
        self.assertTrue(any(line.startswith("shfmt -d ") for line in lines))


if __name__ == "__main__":
    unittest.main()
