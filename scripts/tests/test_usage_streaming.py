"""Preserve usage scorecards while consuming raw responses incrementally."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).parents[2] / "bench" / "analyze-codesage-usage.py"
SPEC = importlib.util.spec_from_file_location("usage_streaming", SCRIPT)
usage = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(usage)


def message(*blocks):
    return json.dumps({"message": {"content": list(blocks)}}) + "\n"


def call(call_id, tool="search"):
    return {"type": "tool_use", "name": "mcp__codesage__" + tool,
            "id": call_id, "input": {"query": "検索"}}


def result(call_id, content):
    return {"type": "tool_result", "tool_use_id": call_id, "content": content}


class UsageStreamingTests(unittest.TestCase):
    def test_extraction_preserves_pairing_order_and_list_contract(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "session.jsonl"
            path.write_text(
                message(result("unknown", "ignored"))
                + message(call("a"), call("b", "find_references"), call("unmatched"))
                + '{"tool_result": invalid}\n'
                + message(result("b", [{"text": "café"}, {"text": "検索"}]),
                          result("a", "Error: missing index"))
                + message(result("a", "duplicate result ignored")), encoding="utf-8")
            rows = usage.extract_calls(path)
            self.assertIsInstance(rows, list)
            self.assertEqual(rows, [
                {"tool": tool, "input_bytes": len(json.dumps({"query": "検索"}).encode()),
                 "output_bytes": len(text.encode()), "output_text": text,
                 "transcript": str(path)}
                for tool, text in (("find_references", "café\n検索"),
                                   ("search", "Error: missing index"))
            ])

    def test_missing_transcript_still_returns_empty_list(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(usage.extract_calls(Path(directory) / "missing.jsonl"), [])

    def test_one_pass_summary_preserves_all_rules_and_rendering(self):
        note = "A repeated note that is longer than thirty characters."
        texts = [
            "MCP error: unavailable", "検索 🔎" * 1000,
            json.dumps([{"file": f"src/{n}.rs"} for n in range(5)]),
            json.dumps([{"file": "src/a.rs", "line": n} for n in (1, 2, 3)]),
            json.dumps([{"notes": [note]} for _ in range(4)]),
        ]
        rows = [{"tool": f"tool-{n % 2}", "output_text": text,
                 "output_bytes": len(text.encode())} for n, text in enumerate(texts)]
        expected = usage.summarize(rows)
        actual = usage.summarize(row for row in rows)
        self.assertEqual(actual, expected)
        self.assertEqual(actual["totals"]["calls"], 5)
        self.assertEqual(actual["totals"]["errors"], 1)
        for saved in actual["compress_savings"].values():
            self.assertGreater(saved, 0)
        kwargs = {"window_days": 7, "transcripts": 2, "now": "fixed"}
        self.assertEqual(usage.render(actual, **kwargs), usage.render(expected, **kwargs))

    def test_iterator_yields_before_reading_next_line_and_closes(self):
        class NoReadAhead(io.StringIO):
            def __next__(self):
                if self.tell() == len(self.getvalue()):
                    raise AssertionError("read past the completed call")
                return super().__next__()

        stream = NoReadAhead(message(call("a")) + message(result("a", "first")))
        with patch.object(Path, "open", return_value=stream):
            calls = usage.iter_calls(Path("synthetic.jsonl"))
            try:
                self.assertEqual(next(calls)["output_text"], "first")
                self.assertFalse(stream.closed)
            finally:
                calls.close()
        self.assertTrue(stream.closed)

    def test_iterator_closes_after_exhaustion(self):
        stream = io.StringIO(message(call("a")) + message(result("a", "first")))
        with patch.object(Path, "open", return_value=stream):
            self.assertEqual(len(list(usage.iter_calls(Path("synthetic.jsonl")))), 1)
        self.assertTrue(stream.closed)

    def test_main_scores_each_call_before_requesting_another(self):
        consumed = []

        class TrackedCall(dict):
            def __getitem__(self, key):
                if key == "output_bytes":
                    consumed.append(self["output_text"])
                return super().__getitem__(key)

        def calls(path):
            for n in range(2):
                text = f"{path}-{n}"
                yield TrackedCall(tool="search", output_text=text, output_bytes=len(text))
                self.assertIn(text, consumed, "raw responses were collected before scoring")

        output = io.StringIO()
        with patch.object(usage, "iter_transcripts", return_value=["one", "two"]), \
                patch.object(usage, "iter_calls", side_effect=calls), \
                patch.object(usage.sys, "argv", [str(SCRIPT)]), \
                contextlib.redirect_stdout(output):
            self.assertEqual(usage.main(), 0)
        self.assertEqual(consumed, ["one-0", "one-1", "two-0", "two-1"])
        self.assertIn("**Calls observed**: 4", output.getvalue())
        self.assertIn("**Transcripts scanned**: 2", output.getvalue())


if __name__ == "__main__":
    unittest.main()
