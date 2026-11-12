import importlib.machinery
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
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
    def run_checker(self, root):
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--root", str(root)],
            capture_output=True, text=True, check=False,
        )

    def test_invalid_json_shapes_have_controlled_cli_diagnostics(self):
        paths = (
            "plugins/codesage-tools/.codex-plugin/plugin.json",
            "plugins/codesage-tools/.claude-plugin/plugin.json",
            ".claude-plugin/marketplace.json",
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for path in paths:
                for value in (None, [], "version", 7, True):
                    with self.subTest(path=path, value=value):
                        make_repo(root)
                        write_json(root / path, value)
                        result = self.run_checker(root)
                        self.assertEqual(result.returncode, 1)
                        self.assertEqual(result.stdout, "")
                        self.assertEqual(
                            result.stderr,
                            f"check-plugin-versions: {root / path}: expected a JSON object\n",
                        )

    def test_invalid_marketplace_members_have_controlled_cli_diagnostics(self):
        cases = []
        for value in (None, [], "version", 7, True):
            cases.append(("metadata", value, "metadata: expected a JSON object"))
        for value in (None, {}, "plugins", 7, True):
            cases.append(("plugins", value, "plugins: expected a JSON array"))
        for value in (None, [], "plugin", 7, True):
            cases.append(("plugins", [{"name": "other"}, value],
                          "plugins[1]: expected a JSON object"))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / ".claude-plugin/marketplace.json"
            for key, value, message in cases:
                with self.subTest(key=key, value=value):
                    make_repo(root)
                    document = json.loads(path.read_text())
                    document[key] = value
                    write_json(path, document)
                    result = self.run_checker(root)
                    self.assertEqual(result.returncode, 1)
                    self.assertEqual(result.stdout, "")
                    self.assertEqual(result.stderr, f"check-plugin-versions: marketplace {message}\n")

    def test_optional_fields_and_unrelated_entries_keep_existing_results(self):
        checker = load_checker()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_repo(root)
            path = root / ".claude-plugin/marketplace.json"
            document = json.loads(path.read_text())
            document["plugins"].append({"name": "other", "version": "0.0.0"})
            write_json(path, document)
            self.assertEqual(checker.version_errors(root), [])
            self.assertEqual(self.run_checker(root).returncode, 0)
            del document["metadata"]
            del document["plugins"]
            write_json(path, document)
            self.assertEqual(checker.version_errors(root), [
                "Claude marketplace metadata version <missing>; expected 1.2.3",
                "Claude marketplace has 0 codesage-tools entries; expected 1",
            ])

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

    def test_stale_codex_base_version_is_reported_despite_cachebuster(self):
        checker = load_checker()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_repo(root, codex="0.4.0+codex.local-20260715-120000")

            errors = checker.version_errors(root)

        self.assertEqual(len(errors), 1)
        self.assertIn("Codex plugin version", errors[0])
        self.assertIn("expected 1.2.3", errors[0])

    def test_duplicate_marketplace_entries_are_reported(self):
        checker = load_checker()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            make_repo(root)
            write_json(
                root / ".claude-plugin/marketplace.json",
                {
                    "name": "codesage",
                    "metadata": {"version": "1.2.3"},
                    "plugins": [
                        {"name": "codesage-tools", "version": "1.2.3"},
                        {"name": "codesage-tools", "version": "1.2.3"},
                    ],
                },
            )

            errors = checker.version_errors(root)

        self.assertEqual(len(errors), 1)
        self.assertIn("2 codesage-tools entries", errors[0])


if __name__ == "__main__":
    unittest.main()
