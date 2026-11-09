import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "bench" / "extract-eval-cases.py"
SPEC = importlib.util.spec_from_file_location("extract_eval_content", SCRIPT)
extract = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(extract)

NON_OBJECTS = (None, False, True, 0, 1.5, "text", [], ["text"])
NON_STRINGS = (None, False, True, 0, 1.5, {}, {"text": "nested"}, [], ["text"])


class MessageContentTests(unittest.TestCase):
    def test_non_object_envelopes_have_no_content(self):
        for value in NON_OBJECTS:
            with self.subTest(value=value):
                row = {"message": value}
                self.assertEqual(extract.get_user_text(row), "")
                self.assertEqual(extract.extract_tool_file_paths(row), set())
                self.assertEqual(list(extract.tool_use_blocks(row)), [])
                self.assertFalse(extract.uses_codesage(row))

    def test_valid_content_forms_are_unchanged(self):
        self.assertEqual(extract.get_user_text({}), "")
        self.assertEqual(extract.get_user_text({"message": {}}), "")
        self.assertEqual(extract.get_user_text({"message": {"content": "query"}}), "query")
        row = {"message": {"content": [
            {"type": "text", "text": "where is"}, "the handler?",
            {"type": "tool_result", "content": {"file_path": "/project/handler.py"}},
            {"type": "tool_use", "name": "mcp__codesage__search", "input": {}},
        ]}}
        self.assertEqual(extract.get_user_text(row), "where is the handler?")
        self.assertEqual(extract.extract_tool_file_paths(row), {"/project/handler.py"})
        self.assertTrue(extract.uses_codesage(row))

    def test_non_string_text_blocks_do_not_hide_valid_siblings(self):
        for value in NON_STRINGS:
            with self.subTest(value=value):
                row = {"message": {"content": [
                    {"type": "text", "text": "where is"},
                    {"type": "text", "text": value},
                    "the request handler?",
                ]}}
                self.assertEqual(extract.get_user_text(row), "where is the request handler?")

    def test_non_content_values_remain_empty(self):
        for value in (None, False, True, 0, 1.5, {}):
            with self.subTest(value=value):
                row = {"message": {"content": value}}
                self.assertEqual(extract.get_user_text(row), "")
                self.assertEqual(extract.extract_tool_file_paths(row), set())
                self.assertFalse(extract.uses_codesage(row))


class ExtractionContentTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.sessions = self.root / "sessions"
        self.sessions.mkdir()
        self.session = self.sessions / "synthetic.jsonl"

    def file_result(self, name, **extra):
        return {"type": "user", "toolUseResult": {"filePath": str(self.root / name)}, **extra}

    def rows(self, malformed):
        return [
            {"type": "user", "message": {"content": "where is the request handler?"},
             "padding": "x" * 3500},
            {"type": "assistant", "message": malformed},
            self.file_result("handler.py", message=malformed),
            # Even a malformed user message still ends the previous case window.
            {"type": "user", "message": malformed},
            self.file_result("unrelated.py"),
            {"type": "user", "message": {"content": "where is authentication implemented?"}},
            {"type": "assistant", "message": malformed},
            {"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "mcp__codesage__search", "input": {}},
                {"type": "tool_result", "content": {"file_path": str(self.root / "auth.py")}},
            ]}},
        ]

    def write_rows(self, rows, separator="\n"):
        self.session.write_text(separator.join(json.dumps(row) for row in rows) + "\n")

    def test_malformed_content_preserves_boundaries_paths_and_contamination(self):
        for malformed in (*NON_OBJECTS, {"content": [{"type": "text", "text": None}]}):
            for separator in ("\n", "\n\n"):
                for include_codesage in (False, True):
                    with self.subTest(malformed=malformed, separator=separator,
                                      include_codesage=include_codesage):
                        self.write_rows(self.rows(malformed), separator)
                        stats = {}
                        cases = extract.extract_cases(
                            self.sessions, str(self.root), 1, 10,
                            include_codesage=include_codesage, stats=stats,
                        )
                        expected = [["handler.py"], ["auth.py"]] if include_codesage else [["handler.py"]]
                        self.assertEqual([case["files"] for case in cases], expected)
                        self.assertEqual([case["codesage_used"] for case in cases],
                                         [False, True] if include_codesage else [False])
                        self.assertEqual(stats, {"excluded_cases": 0 if include_codesage else 1,
                                                 "codesage_sessions": 1})

    def test_cli_keeps_valid_text_and_emits_clean_corpus(self):
        rows = self.rows(None)
        rows[0]["message"]["content"] = [
            {"type": "text", "text": "where is"},
            *({"type": "text", "text": value} for value in NON_STRINGS),
            "the request handler?",
        ]
        self.write_rows(rows)
        output = self.root / "corpus.yaml"
        result = subprocess.run(
            [sys.executable, str(SCRIPT), str(self.sessions), str(self.root), "--yaml", str(output)],
            capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 cases excluded", result.stderr)
        self.assertIn("1 sessions used codesage", result.stderr)
        corpus = output.read_text()
        self.assertIn('query: "where is the request handler?"', corpus)
        self.assertIn('      - "handler.py"', corpus)
        self.assertNotIn("unrelated.py", corpus)
        self.assertNotIn("auth.py", corpus)


if __name__ == "__main__":
    unittest.main()
