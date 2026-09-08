import importlib.util
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "install-codex-skills.py"
SPEC = importlib.util.spec_from_file_location("install_codex_skills", SCRIPT)
INSTALLER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(INSTALLER)


class AdapterInstallTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "source with spaces"
        self.plugin = self.source / "plugins" / "codesage-tools"
        self.skills = self.root / "skills"
        for group, names in (
            ("commands", [f"codesage-{name}" for name in INSTALLER.COMMANDS]),
            ("agents", ["codesage-feature-reviewer"]),
        ):
            (self.plugin / group).mkdir(parents=True)
            for name in names:
                (self.plugin / group / f"{name}.md").write_text(
                    f'---\nname: {name}\ndescription: "Review: {name}"\n---\n'
                    'Canonical body must not be copied.\n', encoding="utf-8")

    def test_replaces_only_entrypoints_and_links_live_sources(self):
        skill = self.skills / "codesage-revalidate"
        skill.mkdir(parents=True)
        (skill / "SKILL.md").write_text("stale automatic fix policy")
        (skill / "notes.txt").write_text("preserve")
        self.assertEqual(INSTALLER.install(self.source, self.skills), 11)
        first = {p: p.read_bytes() for p in self.skills.glob("*/SKILL.md")}
        self.assertEqual(len(first), 11)
        for path, data in first.items():
            group = "agents" if path.parent.name == "codesage-feature-reviewer" else "commands"
            canonical = self.plugin / group / f"{path.parent.name}.md"
            self.assertIn(f"(<{canonical}>)", data.decode())
            self.assertNotIn("Canonical body must not be copied", data.decode())
        canonical = self.plugin / "commands" / "codesage-revalidate.md"
        canonical.write_text(canonical.read_text() + "New policy is immediately available.\n")
        self.assertEqual(INSTALLER.install(self.source, self.skills), 11)
        self.assertEqual(first, {p: p.read_bytes() for p in self.skills.glob("*/SKILL.md")})
        self.assertEqual((skill / "notes.txt").read_text(), "preserve")

    def test_missing_source_preflights_before_any_write(self):
        (self.plugin / "agents" / "codesage-feature-reviewer.md").unlink()
        with self.assertRaises(FileNotFoundError):
            INSTALLER.install(self.source, self.skills)
        self.assertFalse(self.skills.exists())

    def test_symlink_skill_cannot_redirect_writes(self):
        outside = self.root / "outside"
        outside.mkdir()
        self.skills.mkdir()
        (self.skills / "codesage-triage").symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "symlink destination"):
            INSTALLER.install(self.source, self.skills)
        self.assertEqual(list(outside.iterdir()), [])
        self.assertEqual(list(self.skills.glob("*/SKILL.md")), [])


if __name__ == "__main__":
    unittest.main()
