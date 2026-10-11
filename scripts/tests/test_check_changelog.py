import contextlib
import importlib.machinery
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).parents[1] / "check-changelog.py"


def load_checker():
    loader = importlib.machinery.SourceFileLoader("check_changelog", str(SCRIPT))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def run_main(module, text):
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "CHANGELOG.md"
        path.write_text(text)
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            status = module.main(["check-changelog.py", str(path)])
    return status, err.getvalue()


class PreSectionContentTests(unittest.TestCase):
    def test_stray_bullet_before_first_section_fails(self):
        checker = load_checker()
        status, err = run_main(
            checker,
            "# Changelog\n"
            "\n"
            "## [Unreleased]\n"
            "\n"
            "- stray bullet outside any section.\n"
            "\n"
            "### Fixed\n"
            "\n"
            "- Real fix.\n"
            "\n"
            "## [1.0.0] - 2026-01-01\n"
            "\n"
            "### Added\n"
            "\n"
            "- Initial release.\n",
        )
        self.assertEqual(status, 1)
        self.assertIn("entry outside a section", err)

    def test_stray_prose_with_no_sections_fails(self):
        checker = load_checker()
        status, err = run_main(
            checker,
            "# Changelog\n"
            "\n"
            "## [Unreleased]\n"
            "\n"
            "Some prose that belongs under a heading.\n"
            "\n"
            "## [1.0.0] - 2026-01-01\n"
            "\n"
            "### Added\n"
            "\n"
            "- Initial release.\n",
        )
        self.assertEqual(status, 1)
        self.assertIn("entry outside a section", err)

    def test_leading_blank_lines_pass(self):
        checker = load_checker()
        status, err = run_main(
            checker,
            "# Changelog\n"
            "\n"
            "## [Unreleased]\n"
            "\n"
            "\n"
            "\n"
            "### Fixed\n"
            "\n"
            "- Real fix.\n"
            "\n"
            "## [1.0.0] - 2026-01-01\n"
            "\n"
            "### Added\n"
            "\n"
            "- Initial release.\n",
        )
        self.assertEqual(status, 0, err)

    def test_final_unreleased_block_with_link_refs_passes(self):
        # Fresh repo: [Unreleased] is the last `## ` block, so the extracted
        # body swallows the trailing link-reference definitions.
        checker = load_checker()
        status, err = run_main(
            checker,
            "# Changelog\n"
            "\n"
            "## [Unreleased]\n"
            "\n"
            "[Unreleased]: https://github.com/iliaal/codesage/compare/v0.1.0...HEAD\n",
        )
        self.assertEqual(status, 0, err)


class SectionEntryTests(unittest.TestCase):
    def test_non_bullet_content_inside_section_fails(self):
        for entry in ("Unattached prose.", "  An explanatory continuation.",
                      "1. Numbered entry.", "* Alternate bullet.", "#### Nested heading"):
            with self.subTest(entry=entry):
                status, err = run_main(load_checker(),
                    "## [Unreleased]\n\n### Fixed\n\n- Real fix.\n\n" + entry + "\n")
                self.assertEqual(status, 1)
                self.assertIn("entry outside a bullet", err)

    def test_wrapped_bullet_passes(self):
        for indent in ("", "  "):
            with self.subTest(indent=indent):
                status, err = run_main(load_checker(),
                    "## [Unreleased]\n\n### Fixed\n\n- Search returns\n"
                    + indent + "matching files.\n")
                self.assertEqual(status, 0, err)

    def test_wrapped_explanation_fails(self):
        status, err = run_main(load_checker(),
            "## [Unreleased]\n\n### Fixed\n\n- Search returns files,\n"
            "  so users can continue.\n")
        self.assertEqual(status, 1)
        self.assertIn("explanation", err)

    def test_wrapped_bullet_length_boundary(self):
        checker = load_checker()
        for length in (checker.BULLET_MAX_LEN - 1,
                       checker.BULLET_MAX_LEN, checker.BULLET_MAX_LEN + 1):
            with self.subTest(length=length):
                status, err = run_main(checker,
                    "## [Unreleased]\n\n### Fixed\n\n- " + "x" * 200
                    + "\n  " + "x" * (length - 201) + "\n")
                self.assertEqual(status, int(length > checker.BULLET_MAX_LEN), err)

    def test_empty_bullet_fails(self):
        for entry in ("-", "- ", "-   ", "  - "):
            with self.subTest(entry=entry):
                status, err = run_main(load_checker(),
                    "## [Unreleased]\n\n### Fixed\n\n" + entry + "\n")
                self.assertEqual(status, 1)
                self.assertIn("empty bullet", err)

    def test_link_references_after_last_section_pass(self):
        status, err = run_main(load_checker(),
            "## [Unreleased]\n\n### Fixed\n\n- Real fix.\n\n"
            "[Unreleased]: https://example.com/compare\n")
        self.assertEqual(status, 0, err)

    def test_link_reference_does_not_fill_empty_section(self):
        status, err = run_main(load_checker(),
            "## [Unreleased]\n\n### Fixed\n\n"
            "[Unreleased]: https://example.com/compare\n")
        self.assertEqual(status, 1)
        self.assertIn("has no `- ` bullets", err)

    def test_released_prose_is_not_linted(self):
        status, err = run_main(load_checker(),
            "## [Unreleased]\n\n### Fixed\n\n- Real fix.\n\n"
            "## [1.0.0]\n\n### Added\n\nHistorical prose.\n")
        self.assertEqual(status, 0, err)


if __name__ == "__main__":
    unittest.main()
