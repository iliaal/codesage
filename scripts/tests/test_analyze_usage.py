"""Synthetic UTF-8 byte-accounting regressions for the usage scorecard."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).parents[2] / "bench" / "analyze-codesage-usage.py"
SPEC = importlib.util.spec_from_file_location("analyze_usage", SCRIPT)
usage = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(usage)


class UsageBytesTests(unittest.TestCase):
    def write_transcript(self, path, content):
        messages = [
            {"message": {"content": [{"type": "tool_use", "name": "mcp__codesage__search",
                                      "id": "call-1", "input": {"query": "検索"}}]}},
            {"message": {"content": [{"type": "tool_result", "tool_use_id": "call-1",
                                      "content": content}]}},
        ]
        path.write_text("\n".join(json.dumps(m, ensure_ascii=False) for m in messages),
                        encoding="utf-8")

    def test_extract_counts_utf8_bytes_for_both_result_shapes(self):
        text = "café 検索 🔎"
        for content in (text, [{"type": "text", "text": "café"},
                               {"type": "text", "text": "検索 🔎"}]):
            with self.subTest(content=content), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "session.jsonl"
                self.write_transcript(path, content)
                calls = usage.extract_calls(path)
                expected = text if isinstance(content, str) else "café\n検索 🔎"
                self.assertEqual(len(calls), 1)
                self.assertEqual(calls[0]["output_text"], expected)
                self.assertEqual(calls[0]["output_bytes"], len(expected.encode("utf-8")))
                self.assertEqual(calls[0]["input_bytes"],
                                 len(json.dumps({"query": "検索"}).encode("utf-8")))

    def test_unchanged_rules_return_utf8_size(self):
        for text in ("ASCII", "café 検索 🔎", '{"message": "検索"}', '"🔎"'):
            for rule in (usage.rule_group_by_directory, usage.rule_collapse_adjacent_refs,
                         usage.rule_dedupe_repeated_strings, usage.rule_middle_truncate):
                with self.subTest(text=text, rule=rule.__name__):
                    self.assertEqual(rule(text), len(text.encode("utf-8")))

    def test_truncation_threshold_is_bytes(self):
        self.assertEqual(usage.rule_middle_truncate("🔎" * 1024), 4096)
        self.assertEqual(usage.rule_middle_truncate("🔎" * 1025),
                         2048 + len("<... 2052 bytes elided ...>"))
        self.assertEqual(usage.rule_middle_truncate("a" * 4097),
                         2048 + len("<... 2049 bytes elided ...>"))

    def test_unmodified_unicode_response_has_no_structured_savings(self):
        text = '{"message": "検索 🔎"}'
        summary = usage.summarize([{"tool": "search", "output_text": text,
                                   "output_bytes": len(text.encode("utf-8"))}])
        self.assertEqual(summary["compress_savings"]["structured_best"], 0)
        self.assertEqual(summary["totals"]["bytes"], len(text.encode("utf-8")))

    def test_transformed_json_keeps_existing_serialization(self):
        files = [{"file": f"検索/{n}.rs"} for n in range(4)]
        expected = [{"directory": "検索", "count": 4,
                     "top_files": [f"検索/{n}.rs" for n in range(3)]}]
        self.assertEqual(usage.rule_group_by_directory(json.dumps(files, ensure_ascii=False)),
                         len(json.dumps(expected).encode("utf-8")))
        refs = [{"file": "検索.rs", "line": n} for n in (1, 2, 3)]
        expected = [{"file": "検索.rs", "lines": "1-3", "count": 3}]
        self.assertEqual(usage.rule_collapse_adjacent_refs(json.dumps(refs, ensure_ascii=False)),
                         len(json.dumps(expected).encode("utf-8")))
        note = "検索" * 20
        rows = [{"notes": [note]} for _ in range(3)]
        expected = [{"_legend": {"N0": note}}, *[{"notes": ["N0"]} for _ in range(3)]]
        self.assertEqual(usage.rule_dedupe_repeated_strings(json.dumps(rows, ensure_ascii=False)),
                         len(json.dumps(expected).encode("utf-8")))

    def test_invalid_surrogate_does_not_abort_analysis(self):
        text = "bad: " + chr(0xD800)
        self.assertEqual(usage.rule_middle_truncate(text), 6)

    def test_cli_reports_utf8_total(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            project.mkdir()
            self.write_transcript(project / "session.jsonl", "検索🔎")
            result = subprocess.run([sys.executable, str(SCRIPT), "--projects-root", str(root)],
                                    text=True, capture_output=True, check=True)
            self.assertIn("**Total output bytes**: 10 ", result.stdout)
            self.assertIn("| `search` | 1 | 10 | 10 | 10 | 10 | 10 |", result.stdout)


if __name__ == "__main__":
    unittest.main()
