#!/usr/bin/env python3
"""Corpus query leakage regressions: python3 bench/test_generate_llm_corpus.py."""
from __future__ import annotations

import contextlib
import importlib.util
import io
from pathlib import Path
import unittest


spec = importlib.util.spec_from_file_location(
    "generate_llm_corpus", Path(__file__).with_name("generate-llm-corpus.py")
)
gen = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gen)


class CorpusQueryTests(unittest.TestCase):
    def test_assignment_constant_query_is_rejected_before_corpus_output(self):
        content = "MIN_LINES = 30\nregex = compile_pattern()\n"
        with contextlib.redirect_stderr(io.StringIO()):
            query = gen.validate_query_text(
                "filter source files using MIN_LINES", "src/selector.py", content
            )
        self.assertIsNone(query)

    def test_annotated_and_private_constants_are_rejected_case_insensitively(self):
        content = "_MAX_BYTES: int = 20000\n"
        self.assertIsNotNone(gen.query_rejection_reason(
            "bound source size using _max_bytes", "src/selector.py", content
        ))

    def test_constant_matching_respects_identifier_boundaries(self):
        content = "LIMIT = 30\n"
        self.assertIsNotNone(gen.query_rejection_reason(
            "bound source size using (LIMIT)", "src/selector.py", content
        ))
        self.assertIsNone(gen.query_rejection_reason(
            "find limiter handling source sizes", "src/selector.py", content
        ))
        self.assertIsNone(gen.query_rejection_reason(
            "find LIMIT_OVERRIDE handling source sizes", "src/selector.py", content
        ))

    def test_behavioral_vocabulary_is_not_a_symbol_ban(self):
        content = (
            "MIN_LINES = 30\n"
            "regex = compile_pattern()\n"
            "# tokenization uses backtracking and normalization\n"
            "description = 'regex tokenization backtracking normalization'\n"
            "# REGEX = descriptive example, not an assignment\n"
            "REGEX == expected\n"
        )
        query = "regex tokenization with backtracking and normalization"
        self.assertEqual(
            gen.validate_query_text(query, "src/selector.py", content), query
        )

    def test_declared_symbols_remain_rejected(self):
        self.assertIsNotNone(gen.query_rejection_reason(
            "find CandidateSelector filtering source files", "src/selector.py",
            "class CandidateSelector:\n    pass\n",
        ))


if __name__ == "__main__":
    unittest.main()
