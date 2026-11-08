"""Bounded terminality look-ahead must not copy the remaining transcript."""

import importlib.util
from pathlib import Path
import unittest


SCRIPT = Path(__file__).parents[2] / "bench" / "analyze-codesage-quality.py"
SPEC = importlib.util.spec_from_file_location("quality_window", SCRIPT)
quality = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(quality)


def call(tool):
    return {"kind": "tool_use", "tool": tool}


class QualityWindowTests(unittest.TestCase):
    def test_window_preserves_boundaries_and_skipped_events(self):
        first = call("Bash")
        second = call("Grep")
        chained = call("mcp__codesage__search")
        question = {"kind": "user", "text": "Where is the next function?"}
        notification = {"kind": "user", "text": "<task-notification>done</task-notification>"}
        skipped = [{"kind": "tool_result"}, notification,
                   *(call(tool) for tool in sorted(quality.NON_SLOT_TOOLS))]
        cases = [
            ([], 3, []),
            (skipped, 3, []),
            ([first, second], 1, [first]),
            ([first, second], 3, [first, second]),
            ([*skipped, first, *skipped, second], 2, [first, second]),
            ([question, first], 3, []),
            ([first, question, second], 3, [first]),
            ([first, chained, second], 3, [first, chained]),
        ]
        for tail, window, expected in cases:
            for start in (0, 5):
                with self.subTest(tail=tail, window=window, start=start):
                    events = [chained] * (start + 1) + tail
                    before = list(events)
                    actual = quality._window_calls(events, start, window)
                    self.assertEqual(actual, expected)
                    self.assertTrue(all(a is b for a, b in zip(actual, expected)))
                    self.assertEqual(events, before)
        self.assertEqual(quality._window_calls([], 0, 3), [])

    def test_window_reads_only_the_needed_indices(self):
        class IndexedEvents(list):
            def __getitem__(self, index):
                if isinstance(index, slice):
                    raise AssertionError("look-ahead must not copy a transcript tail")
                reads.append(index)
                return super().__getitem__(index)

            def __iter__(self):
                raise AssertionError("look-ahead must not scan from the transcript start")

        reads = []
        followup = call("Grep")
        events = IndexedEvents([followup] * 10000)
        self.assertEqual(quality._window_calls(events, 5000, 3), [followup] * 3)
        self.assertEqual(reads, [5001, 5002, 5003])


if __name__ == "__main__":
    unittest.main()
