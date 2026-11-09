"""Failure counts must retain their input provenance through case exclusion."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).parents[2] / "bench" / "compare-runs.py"
SPEC = importlib.util.spec_from_file_location("comparison_failures", SCRIPT)
compare = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compare)


def record(number, *, hit=False, **extra):
    return {"id": str(number), "query": "find gold", "expected_files": ["gold"],
            "hits": ["gold"] if hit else [], "first_hit_rank": 1 if hit else None,
            **extra}


class ComparisonFailureTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.misses = [record(i) for i in range(7)]
        self.hits = [record(i, hit=True) for i in range(7)]

    def write(self, name, records, meta=None, complete=True):
        path = self.root / name
        value = records if meta is None else {"meta": meta, "records": records,
                                              "complete": complete}
        path.write_text(json.dumps(value))
        return path

    def arguments(self, baseline, candidate, *flags):
        return ["--baseline", *map(str, baseline), "--candidate", *map(str, candidate),
                "--min-n", "5", "--bootstrap", "200", *flags]

    def run_comparison(self, baseline, candidate, *flags):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            rc = compare.main(self.arguments(baseline, candidate, *flags))
        return rc, out.getvalue(), err.getvalue()

    def test_exclusion_cannot_cancel_another_inputs_unknown_failures(self):
        # Two identified failures from the second file cannot account for the
        # first file's unknown one, regardless of envelope shape or file order.
        for arm in ("baseline", "candidate"):
            rows = self.misses if arm == "baseline" else self.hits
            clean = self.write("clean.json", self.hits if arm == "baseline" else self.misses)
            ghost = self.write("ghost.json", rows[:5], {"search_failures": 1})
            errors = [{**r, "error": "timeout", "hits": [], "first_hit_rank": None}
                      for r in rows[5:]]
            for meta in (None, {}, {"search_failures": None}, {"search_failures": 0},
                         {"search_failures": 1}, {"search_failures": 2}):
                known = self.write("known.json", errors, meta)
                for inputs in ([ghost, known], [known, ghost]):
                    with self.subTest(arm=arm, meta=meta, inputs=inputs):
                        baseline, candidate = ((inputs, [clean]) if arm == "baseline"
                                               else ([clean], inputs))
                        merged, _ = compare.load_records(inputs, allow_partial=False)
                        self.assertEqual(merged["search_failures"], 3)
                        rc, out, err = self.run_comparison(
                            baseline, candidate, "--exclude-failed", "--gate", "--allow-mismatch")
                        self.assertEqual(rc, 2, (out, err))
                        counts = "baseline 1, candidate 0" if arm == "baseline" else "baseline 0, candidate 1"
                        self.assertIn(f"search failures recorded ({counts})", err)
                        self.assertNotIn("ACCEPT", out)
                        rc, out, err = self.run_comparison(
                            baseline, candidate, "--exclude-failed", "--allow-failures")
                        self.assertEqual(rc, 0, err)
                        cells = "1 | 0" if arm == "baseline" else "0 | 1"
                        self.assertIn(f"| search failures | {cells} | |", out)
                        self.assertIn(f"- {arm}: 1 search failure(s) without an identifiable case id", out)
                        self.assertIn("Excluded failed ids (both arms): 2 (5, 6)", out)
                        self.assertIn("Paired cases: 5 ", out)

    def test_counts_include_known_and_unknown_failures_without_exclusion(self):
        ghost = self.write("ghost.json", self.misses[:5], {"search_failures": 3})
        known = self.write("known.json", [record(5, error=""), record(6, error=False)])
        clean = self.write("clean.json", self.hits)
        rc, _, err = self.run_comparison([ghost, known], [clean])
        self.assertEqual(rc, 2)
        self.assertIn("baseline 5, candidate 0", err)
        rc, out, err = self.run_comparison([ghost, known], [clean], "--allow-failures")
        self.assertEqual(rc, 0, err)
        self.assertIn("| search failures | 5 | 0 | |", out)
        self.assertIn("- baseline: 5, 6", out)
        self.assertIn("- baseline: 3 search failure(s) without an identifiable case id", out)

    def test_fully_attributable_failures_still_clear(self):
        clean = self.write("clean.json", self.hits)
        for count in (None, 0, 1, 2):
            first = self.write("first.json", self.misses[:5], {"search_failures": 0})
            second = self.write("second.json", [record(5, error="timeout"), record(6, error="rc=1")],
                                {"search_failures": count})
            with self.subTest(count=count):
                rc, out, err = self.run_comparison([first, second], [clean], "--exclude-failed", "--gate")
                self.assertEqual(rc, 0, err)
                self.assertIn("\nACCEPT\n", out)
                self.assertIn("| search failures | 0 | 0 | |", out)
                self.assertNotIn("without an identifiable case id", out)

    def test_each_input_can_have_both_known_and_unknown_failures(self):
        first_rows = [record(i, error="timeout" if i == 0 else None) for i in range(7)]
        first = self.write("first.json", first_rows, {"search_failures": 3})
        second = self.write("second.json", [record(7, error="timeout"), record(8, error="rc=1")],
                            {"search_failures": 3})
        clean = self.write("clean.json", [record(i, hit=True) for i in range(9)])
        rc, out, err = self.run_comparison([first, second], [clean],
                                           "--exclude-failed", "--allow-failures")
        self.assertEqual(rc, 0, err)
        self.assertIn("| search failures | 3 | 0 | |", out)
        self.assertIn("- baseline: 3 search failure(s) without an identifiable case id", out)
        self.assertIn("Excluded failed ids (both arms): 3 (0, 7, 8)", out)
        self.assertIn("Paired cases: 6 ", out)

    def test_null_errors_and_clean_legacy_metadata_stay_unchanged(self):
        clean = self.write("clean.json", [{**r, "error": None} for r in self.hits])
        meta, records = compare.load_records([clean], allow_partial=False)
        self.assertEqual(meta, {})
        self.assertEqual(compare.failed_ids(records), [])
        rc, out, err = self.run_comparison([clean], [clean], "--exclude-failed")
        self.assertEqual(rc, 0, err)
        self.assertIn("(no meta: legacy list input)", out)
        self.assertIn("Excluded failed ids (both arms): none", out)

    def test_invalid_metadata_is_not_masked_by_record_errors(self):
        for count in (-1, 0.5, True, False, float("inf"), "many"):
            path = self.write("invalid.json", [record(0, error="timeout")], {"search_failures": count})
            with self.subTest(count=count), self.assertRaisesRegex(compare.Refused, "nonnegative integer"):
                compare.load_records([path], allow_partial=True)

    def test_partial_opt_in_does_not_clear_unknown_failures(self):
        partial = self.write("partial.json", self.misses[:5], {"search_failures": 1}, complete=False)
        known = self.write("known.json", [record(5, error="timeout"), record(6, error="timeout")])
        clean = self.write("clean.json", self.hits)
        rc, _, err = self.run_comparison([partial, known], [clean], "--exclude-failed")
        self.assertEqual(rc, 2)
        self.assertIn("complete: false", err)
        rc, _, err = self.run_comparison([partial, known], [clean], "--exclude-failed", "--allow-partial")
        self.assertEqual(rc, 2)
        self.assertIn("baseline 1, candidate 0", err)

    def test_cli_refuses_cross_input_cancellation(self):
        ghost = self.write("ghost.json", self.misses[:5], {"search_failures": 1})
        known = self.write("known.json", [record(5, error="timeout"), record(6, error="timeout")])
        clean = self.write("clean.json", self.hits)
        result = subprocess.run([sys.executable, str(SCRIPT), *self.arguments(
            [ghost, known], [clean], "--exclude-failed", "--gate")], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("baseline 1, candidate 0", result.stderr)
        self.assertNotIn("ACCEPT", result.stdout)


if __name__ == "__main__":
    unittest.main()
