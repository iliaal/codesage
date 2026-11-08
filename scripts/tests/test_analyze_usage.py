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

    def test_note_deduplication_preserves_non_string_values(self):
        note = "A repeated note that is longer than thirty characters."
        other = [None, True, False, 42, 1.5, {"context": "検索"}, ["nested", {"value": 1}]]
        rows = [{"notes": [note, *other]} for _ in range(3)]
        expected = [{"_legend": {"N0": note}},
                    *[{"notes": ["N0", *other]} for _ in range(3)]]
        for nested in (False, True):
            with self.subTest(nested=nested):
                data = {"results": rows, "summary": "unchanged"} if nested else rows
                compressed = ({"results": expected, "summary": "unchanged"}
                              if nested else expected)
                self.assertEqual(usage.rule_dedupe_repeated_strings(json.dumps(data)),
                                 len(json.dumps(compressed).encode("utf-8")))

    def test_cli_scorecard_handles_mixed_note_values(self):
        note = "A repeated note that is longer than thirty characters."
        rows = [{"notes": [note, {"context": "検索"}, ["nested"]]} for _ in range(3)]
        text = json.dumps(rows)
        expected = [{"_legend": {"N0": note}},
                    *[{"notes": ["N0", {"context": "検索"}, ["nested"]]}
                      for _ in range(3)]]
        saved = len(text.encode("utf-8")) - len(json.dumps(expected).encode("utf-8"))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            project.mkdir()
            self.write_transcript(project / "session.jsonl", text)
            result = subprocess.run([sys.executable, str(SCRIPT), "--projects-root", str(root)],
                                    text=True, capture_output=True, check=True)
            self.assertIn("**Calls observed**: 1", result.stdout)
            self.assertIn(f"**Total output bytes**: {len(text.encode('utf-8'))} ",
                          result.stdout)
            self.assertIn(f"| `dedupe_repeated_strings` |  | {saved} |", result.stdout)

    def test_noop_array_rules_preserve_original_bytes(self):
        arrays = [
            ([], True), (["検索"], True), ([{"file": "検索.rs"}], True),
            ([{"file": f"d{n}/検索.rs", "line": n * 10} for n in range(3)], True),
            ([{"file": "検索.rs", "line": n * 10} for n in range(3)], False),
        ]
        for rows, directory_noop in arrays:
            for indent in (None, 8):
                text = json.dumps(rows, ensure_ascii=False, indent=indent)
                with self.subTest(rows=rows, indent=indent):
                    if directory_noop:
                        self.assertEqual(usage.rule_group_by_directory(text),
                                         len(text.encode("utf-8")))
                    self.assertEqual(usage.rule_collapse_adjacent_refs(text),
                                     len(text.encode("utf-8")))

    def test_noop_rules_do_not_promote_compression_in_cli_scorecard(self):
        rows = [{"file": f"d{n}/f.rs", "line": n * 10} for n in range(3)]
        text = json.dumps(rows, indent=8)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            project.mkdir()
            for n in range(20):
                self.write_transcript(project / f"session-{n}.jsonl", text)
            result = subprocess.run([sys.executable, str(SCRIPT), "--projects-root", str(root)],
                                    text=True, capture_output=True, check=True)
            for rule in ("group_by_directory", "collapse_adjacent_refs", "structured_best"):
                self.assertIn(f"| `{rule}` |  | 0 | 0.0% |", result.stdout)
            self.assertIn("**Shelf item 1.5.**", result.stdout)
            self.assertNotIn("**Promote", result.stdout)

    def malformed_reference_rows(self):
        for field in ("file", "file_path", "line"):
            values = (["unknown", "1.5", [1], {"line": 1}, float("nan"),
                       float("inf"), -float("inf")]
                      if field == "line" else [42, True, ["src/c.rs"], {"path": "src/c.rs"}])
            for value in values:
                path_field = "file_path" if field == "file_path" else "file"
                rows = [{path_field: "src/検索.rs", "line": n} for n in (1, 2, 3)]
                # A valid, compressible prefix must not hide the invalid row.
                rows.append({path_field: "src/検索.rs", "line": 4, field: value})
                yield field, value, rows

    def test_unusable_reference_keys_preserve_bytes_and_summary(self):
        for field, value, rows in self.malformed_reference_rows():
            for indent in (None, 8):
                with self.subTest(field=field, value=value, indent=indent):
                    text = json.dumps(rows, ensure_ascii=False, indent=indent)
                    size = len(text.encode("utf-8"))
                    self.assertEqual(usage.rule_collapse_adjacent_refs(text), size)
                    summary = usage.summarize([{"tool": "find_references",
                                               "output_text": text, "output_bytes": size}])
                    self.assertEqual(summary["totals"], {"calls": 1, "bytes": size, "errors": 0})
                    self.assertEqual(summary["per_tool"], {"find_references": [size]})
                    self.assertEqual(summary["compress_savings"]["collapse_adjacent_refs"], 0)

    def test_reference_key_coercions_and_path_fallbacks_still_compress(self):
        path = "src/検索.rs"
        expected = len(json.dumps([{"file": path, "lines": "1-3", "count": 3}]).encode())
        for paths in ({"file": path}, {"file_path": path},
                      {"file": "", "file_path": path},
                      {"file": path, "file_path": 42}):
            for lines in ((3, 1, 2), ("3", "1", "2"), (3.5, True, 2.5)):
                with self.subTest(paths=paths, lines=lines):
                    rows = [{**paths, "line": line} for line in lines]
                    self.assertEqual(usage.rule_collapse_adjacent_refs(json.dumps(rows)), expected)

    def test_cli_scorecard_handles_unusable_reference_keys(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            project.mkdir()
            total_bytes = 0
            cases = list(self.malformed_reference_rows())
            for number, (_, _, rows) in enumerate(cases):
                text = json.dumps(rows, ensure_ascii=False, indent=8)
                total_bytes += len(text.encode("utf-8"))
                self.write_transcript(project / f"session-{number}.jsonl", text)
            result = subprocess.run([sys.executable, str(SCRIPT), "--projects-root", str(root)],
                                    text=True, capture_output=True, check=True)
            self.assertIn(f"**Calls observed**: {len(cases)}", result.stdout)
            self.assertIn(f"**Total output bytes**: {total_bytes:,} ", result.stdout)
            self.assertIn("| `collapse_adjacent_refs` |  | 0 | 0.0% |", result.stdout)

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
