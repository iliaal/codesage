import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "bench" / "extract-eval-cases.py"
SPEC = importlib.util.spec_from_file_location("extract_eval_session_rows", SCRIPT)
extract = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(extract)

NON_OBJECTS = (None, False, True, 0, 1.5, "text", [], [{"type": "user"}])


class SessionRowsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.sessions = self.root / "sessions"
        self.sessions.mkdir()
        self.session = self.sessions / "sample.jsonl"
        self.messages = [
            {},
            {"type": "progress", "data": "metadata"},
            {"type": "user", "message": {
                "content": "where is the request handler implemented?"},
                "padding": "x" * 3500},
            {"type": "user", "toolUseResult": {
                "filePath": str(self.root / "handler.py")}},
            {"type": "user", "message": {
                "content": "where is the authentication service implemented?"}},
            {"type": "assistant", "message": {"content": [{
                "type": "tool_use", "name": "mcp__codesage__search",
                "input": {"query": "authentication"}}]}},
            {"type": "user", "toolUseResult": {
                "filePath": str(self.root / "auth.py")}},
        ]

    def write_rows(self, rows):
        self.session.write_text(
            "\n".join(json.dumps(row) for row in rows) + "\n\n{broken\n",
            encoding="utf-8",
        )

    def assert_cases(self, include_codesage=False):
        stats = {}
        cases = extract.extract_cases(
            self.sessions, str(self.root), min_files=1, max_cases=10,
            include_codesage=include_codesage, stats=stats,
        )
        expected_files = [["handler.py"], ["auth.py"]] if include_codesage else [["handler.py"]]
        self.assertEqual([case["files"] for case in cases], expected_files)
        self.assertEqual([case["codesage_used"] for case in cases],
                         [False, True] if include_codesage else [False])
        self.assertEqual(stats, {"excluded_cases": 0 if include_codesage else 1,
                                 "codesage_sessions": 1})

    def test_object_rows_keep_case_boundaries_and_contamination(self):
        self.write_rows(self.messages)
        self.assertEqual(extract.parse_session(self.session), self.messages)
        self.assert_cases()
        self.assert_cases(include_codesage=True)

    def test_non_object_rows_are_skipped_without_losing_messages(self):
        for value in NON_OBJECTS:
            with self.subTest(value=value):
                rows = [value]
                for message in self.messages:
                    rows.extend([message, value])
                self.write_rows(rows)
                self.assertEqual(extract.parse_session(self.session), self.messages)
                self.assert_cases()
                self.assert_cases(include_codesage=True)

    def test_cli_writes_corpus_despite_non_object_rows(self):
        rows = []
        for message in self.messages:
            rows.extend([*NON_OBJECTS, message])
        self.write_rows(rows)
        output = self.root / "corpus.yaml"
        result = subprocess.run(
            [sys.executable, str(SCRIPT), str(self.sessions), str(self.root),
             "--yaml", str(output)], capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 cases excluded", result.stderr)
        self.assertIn('      - "handler.py"', output.read_text())
        self.assertNotIn("auth.py", output.read_text())


if __name__ == "__main__":
    unittest.main()
