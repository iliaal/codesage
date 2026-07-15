import importlib.machinery
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).parents[1] / "check-plugin-versions.py"


def load_checker():
    loader = importlib.machinery.SourceFileLoader("check_plugin_versions", str(SCRIPT))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value) + "\n")


def make_repo(root, workspace="1.2.3", codex="1.2.3", claude="1.2.3", marketplace="1.2.3"):
    (root / "Cargo.toml").write_text(
        f'[workspace.package]\nversion = "{workspace}"\n'
    )
    write_json(
        root / "plugins/codesage-tools/.codex-plugin/plugin.json",
        {"name": "codesage-tools", "version": codex},
    )
    write_json(
        root / "plugins/codesage-tools/.claude-plugin/plugin.json",
        {"name": "codesage-tools", "version": claude},
    )
    write_json(
        root / ".claude-plugin/marketplace.json",
        {
            "name": "codesage",
            "metadata": {"version": marketplace},
            "plugins": [
                {"name": "codesage-tools", "version": marketplace, "source": "./plugins/codesage-tools"}
            ],
        },
    )


class PluginVersionTests(unittest.TestCase):
    def test_matching_versions_accept_codex_cachebuster(self):
        checker = load_checker()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_repo(root, codex="1.2.3+codex.local-20260715-120000")

            errors = checker.version_errors(root)

        self.assertEqual(errors, [])

    def test_stale_claude_and_marketplace_versions_are_reported(self):
        checker = load_checker()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_repo(root, claude="0.4.0", marketplace="0.4.0")

            errors = checker.version_errors(root)

        self.assertEqual(len(errors), 3)
        self.assertTrue(all("expected 1.2.3" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
