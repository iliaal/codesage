import importlib.util
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest


PLUGIN = Path(__file__).resolve().parents[1]
REPO = PLUGIN.parents[1]
SPEC = importlib.util.spec_from_file_location("guidance", PLUGIN / "bin/render-guidance.py")
GUIDANCE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUIDANCE)


class GuidanceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def installed_plugin(self):
        plugin = self.root / "source 'quoted' space/plugins/codesage-tools"
        shutil.copytree(PLUGIN, plugin)
        return plugin

    def invoke(self, *arguments, **kwargs):
        return subprocess.run(arguments, cwd=self.root, capture_output=True, text=True,
                              timeout=30, **kwargs)

    def onboard(self, plugin, project, *flags):
        result = self.invoke("bash", str(plugin / "bin/codesage-onboard"), str(project),
                             "--codesage-bin", "/bin/false", "--hint-only", *flags)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def test_generated_skill_and_registration_descriptions_match_shared_source(self):
        catalog = json.loads(GUIDANCE.CATALOG.read_text())
        self.assertEqual(GUIDANCE.SKILL.read_text(), GUIDANCE.render_skill(catalog))
        source = (REPO / "crates/cli/src/mcp/mod.rs").read_text()
        self.assertEqual(source, GUIDANCE.render_descriptions(source, catalog))
        result = self.invoke(sys.executable, str(PLUGIN / "bin/render-guidance.py"), "--check")
        self.assertEqual(result.returncode, 0, result.stderr)
        skill = GUIDANCE.SKILL.read_text()
        for name in catalog["tools"]:
            self.assertIn(f"`mcp__codesage__{name}`", skill)
        for recovery in catalog["recovery"].values():
            self.assertIn(recovery["meaning"], skill)
            for step in recovery["steps"]:
                self.assertIn(step, skill)

    def test_generation_check_detects_a_changed_route_in_an_installed_copy(self):
        plugin = self.installed_plugin()
        catalog_path = plugin / "skills/codesage-retrieval/references/help.json"
        catalog = json.loads(catalog_path.read_text())
        catalog["tools"]["help"]["description"] = "Run-unique changed route: fixture mutation."
        catalog_path.write_text(json.dumps(catalog))
        check = self.invoke(sys.executable, str(plugin / "bin/render-guidance.py"), "--check")
        self.assertEqual(check.returncode, 1)
        self.assertIn("generated guidance differs", check.stderr)
        write = self.invoke(sys.executable, str(plugin / "bin/render-guidance.py"), "--write")
        self.assertEqual(write.returncode, 0, write.stderr)
        self.assertIn("Run-unique changed route", (plugin / "skills/codesage-retrieval/SKILL.md").read_text())
        check = self.invoke(sys.executable, str(plugin / "bin/render-guidance.py"), "--check")
        self.assertEqual(check.returncode, 0, check.stderr)

    def test_installed_onboarding_quotes_project_data_and_refreshes_live_version(self):
        plugin = self.installed_plugin()
        project = self.root / "project 'single' \"double\"\n$(touch FORBIDDEN) `touch FORBIDDEN` \\ Ω\n"
        project.mkdir()
        self.onboard(plugin, project)
        path = project / ".claude/CLAUDE.md"
        original = path.read_text()
        self.assertLess(len(original.splitlines()), 15)
        values = {line.split(": ", 1)[0]: json.loads(line.split(": ", 1)[1])
                  for line in original.splitlines() if "(JSON string): " in line}
        self.assertEqual(values["Project argument (JSON string)"], str(project))
        self.assertEqual(values["Routing skill path (JSON string)"],
                         str(plugin / "skills/codesage-retrieval/SKILL.md"))
        self.assertIn(f"hint-version: {GUIDANCE.version(plugin)} -->", original)
        self.assertFalse((self.root / "FORBIDDEN").exists())
        self.assertFalse((project / "FORBIDDEN").exists())
        self.assertFalse((project / ".codesage").exists(), "hint-only must not initialize an index")
        path.write_text(original + "User note stays unless refresh is requested.\n")
        self.onboard(plugin, project)
        self.assertIn("User note stays", path.read_text())
        for host in (".codex-plugin", ".claude-plugin"):
            manifest = plugin / host / "plugin.json"
            metadata = json.loads(manifest.read_text())
            metadata["version"] = "9.8.7"
            manifest.write_text(json.dumps(metadata))
        self.onboard(plugin, project, "--refresh-hint")
        refreshed = path.read_text()
        self.assertIn("hint-version: 9.8.7 -->", refreshed)
        self.assertNotIn("User note stays", refreshed)
        self.assertLess(len(refreshed.splitlines()), 15)
        skill_paragraphs = set(re.split(r"\n\s*\n", GUIDANCE.SKILL.read_text()))
        hint_paragraphs = set(re.split(r"\n\s*\n", refreshed))
        self.assertFalse({paragraph for paragraph in skill_paragraphs & hint_paragraphs
                          if len(paragraph) > 30})

    def test_refresh_preserves_surrounding_ignored_agents_text(self):
        project = self.root / "repository with spaces"
        subprocess.run(["git", "init", "-q", str(project)], check=True)
        (project / ".gitignore").write_text("AGENTS.md\n")
        path = project / "AGENTS.md"
        path.write_text("Before\n<!-- codesage-onboard-hint-begin -->\nold\n"
                        "<!-- codesage-onboard-hint-end -->\nAfter\n")
        self.onboard(PLUGIN, project, "--refresh-hint")
        first = path.read_text()
        self.onboard(PLUGIN, project, "--refresh-hint")
        self.assertEqual(path.read_text(), first)
        self.assertTrue(first.startswith("Before\n"))
        self.assertTrue(first.endswith("After\n"))
        self.assertEqual(first.count("<!-- codesage-onboard-hint-begin -->"), 1)
        self.assertIn("Routing skill path (JSON string)", first)

    def test_prompt_compatibility_pointer_and_adapter_follow_installed_source(self):
        plugin = self.installed_plugin()
        result = self.invoke("bash", str(plugin / "bin/codesage-prompt-override"))
        self.assertEqual(result.returncode, 0, result.stderr)
        line = next(line for line in result.stdout.splitlines() if "(JSON string): " in line)
        self.assertEqual(json.loads(line.split(": ", 1)[1]),
                         str(plugin / "skills/codesage-retrieval/SKILL.md"))
        self.assertNotIn("| Question shape", result.stdout)
        destination = self.root / "installed adapters"
        installed = self.invoke(sys.executable, str(REPO / "scripts/install-codex-skills.py"),
                                "--source-root", str(plugin.parents[1]),
                                "--skills-dir", str(destination))
        self.assertEqual(installed.returncode, 0, installed.stderr)
        adapter = (destination / "codesage-prompt-override/SKILL.md").read_text()
        self.assertIn(str(plugin / "commands/codesage-prompt-override.md"), adapter)
        self.assertNotIn("Where is <Identifier>", adapter)

    def test_disagreeing_versions_fail_before_replacing_existing_hint(self):
        plugin = self.installed_plugin()
        manifest = plugin / ".codex-plugin/plugin.json"
        metadata = json.loads(manifest.read_text())
        metadata["version"] = "9.8.7"
        manifest.write_text(json.dumps(metadata))
        project = self.root / "project"
        (project / ".claude").mkdir(parents=True)
        hint = project / ".claude/CLAUDE.md"
        hint.write_text("Existing user content\n")
        result = self.invoke("bash", str(plugin / "bin/codesage-onboard"), str(project),
                             "--codesage-bin", "/bin/false", "--hint-only", "--refresh-hint")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("plugin version stamps disagree", result.stderr)
        self.assertEqual(hint.read_text(), "Existing user content\n")


if __name__ == "__main__":
    unittest.main()
