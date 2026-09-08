import copy
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).parents[1] / "bin" / "codesage-review-state"
PLUGIN_ROOT = Path(__file__).parents[1]


def load_review_state():
    loader = importlib.machinery.SourceFileLoader("codesage_review_state", str(SCRIPT))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


class SlimPriorTests(unittest.TestCase):
    def test_slim_priors_exclude_fixed_and_history_heavy_fields(self):
        review_state = load_review_state()
        history = [{"action": "not-seen", "note": "x" * 500} for _ in range(20)]
        document = {
            "findings": [
                {
                    "finding_id": "fnd_11111111",
                    "file": "src/lib.rs",
                    "line": 12,
                    "severity": "medium",
                    "category": "bug",
                    "title": "Open defect",
                    "status": "open",
                    "summary": "Concrete summary",
                    "evidence": ["let value = broken();"],
                    "suggested_fix": "x" * 1000,
                    "history": history,
                },
                {
                    "finding_id": "fnd_22222222",
                    "file": "src/lib.rs",
                    "line": 30,
                    "severity": "low",
                    "category": "bug",
                    "title": "Resolved defect",
                    "status": "fixed",
                    "summary": "No longer relevant",
                    "history": history,
                },
            ]
        }

        slim = review_state.slim_priors(document)

        self.assertEqual([finding["finding_id"] for finding in slim], ["fnd_11111111"])
        self.assertEqual(
            set(slim[0]),
            {
                "finding_id",
                "file",
                "line",
                "severity",
                "category",
                "title",
                "status",
                "summary",
            },
        )
        self.assertLess(len(json.dumps(slim)), len(json.dumps(document)) // 10)


class FeatureStateTests(unittest.TestCase):
    def test_feature_state_tracks_slice_content_not_review_state_mtime(self):
        review_state = load_review_state()
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text("pub fn value() -> u8 { 1 }\n")
            findings = project / "findings.json"
            findings.write_text("{}\n")
            feature = {
                "feature_id": "feat_1111111111111111",
                "files": [{"path": "src/lib.rs", "role": "entry"}],
            }

            initial = review_state.feature_state(project, feature)
            os.utime(findings, (2_000_000_000, 2_000_000_000))
            after_triage_touch = review_state.feature_state(project, feature)
            source.write_text("pub fn value() -> u8 { 2 }\n")
            after_code_change = review_state.feature_state(project, feature)

        self.assertEqual(initial, after_triage_touch)
        self.assertNotEqual(initial, after_code_change)

    def test_feature_states_hash_shared_files_once_per_batch(self):
        review_state = load_review_state()
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "shared.rs"
            source.parent.mkdir()
            source.write_text("pub fn shared() {}\n")
            features = [
                {
                    "feature_id": "feat_1111111111111111",
                    "files": [{"path": "src/shared.rs", "role": "owned"}],
                },
                {
                    "feature_id": "feat_2222222222222222",
                    "files": [{"path": "src/shared.rs", "role": "context"}],
                },
            ]
            original_read_bytes = Path.read_bytes
            reads = []

            def counted_read_bytes(path):
                reads.append(path)
                return original_read_bytes(path)

            with mock.patch.object(Path, "read_bytes", counted_read_bytes):
                states = review_state.feature_states(project, features)

        self.assertEqual(len(states), 2)
        self.assertEqual(reads, [source])


    def test_feature_state_binds_to_review_params(self):
        review_state = load_review_state()
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text("fn value() {}\n")
            feature = {
                "feature_id": "feat_1111111111111111",
                "files": [{"path": "src/lib.rs", "role": "entry"}],
            }

            default_state = review_state.feature_state(project, feature)
            narrow = review_state.feature_state(
                project, feature, params="categories=bug;severity=medium"
            )
            wide = review_state.feature_state(
                project, feature, params="categories=bug,security;severity=low"
            )
            narrow_again = review_state.feature_state(
                project, feature, params="categories=bug;severity=medium"
            )

        self.assertNotEqual(default_state, narrow)
        self.assertNotEqual(narrow, wide)
        self.assertEqual(narrow, narrow_again)


class ReviewPlanTests(unittest.TestCase):
    def test_plan_drops_deleted_files_when_project_supplied(self):
        review_state = load_review_state()
        feature = {
            "feature_id": "feat_1111111111111111",
            "entry_path": "src/main.rs",
            "files": [
                {"path": "src/main.rs", "role": "entry"},
                {"path": "src/real.rs", "role": "owned"},
                {"path": "src/deleted.rs", "role": "owned"},
            ],
            "trust_boundaries": [],
        }
        risk = {"files": [{"file": "src/real.rs", "score": 0.2}]}
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            (project / "src").mkdir()
            (project / "src" / "main.rs").write_text("fn main() {}\n")
            (project / "src" / "real.rs").write_text("fn real() {}\n")

            unfiltered = review_state.plan_feature_review(feature, risk, [])
            filtered = review_state.plan_feature_review(
                feature, risk, [], project=project
            )

        self.assertIn("src/deleted.rs", unfiltered["must_read"])
        self.assertEqual(filtered["must_read"], ["src/main.rs", "src/real.rs"])

    def test_plan_includes_changed_context_and_test_files(self):
        review_state = load_review_state()
        feature = {
            "feature_id": "feat_1111111111111111",
            "entry_path": "src/main.rs",
            "files": [
                {"path": "src/main.rs", "role": "entry"},
                {"path": "src/owned.rs", "role": "owned"},
                {"path": "tests/helper.rs", "role": "context"},
            ],
            "trust_boundaries": [],
        }
        risk = {"files": [{"file": "src/owned.rs", "score": 0.3}]}

        plan = review_state.plan_feature_review(feature, risk, ["tests/helper.rs"])

        self.assertEqual(
            plan["must_read"], ["src/main.rs", "tests/helper.rs", "src/owned.rs"]
        )
        self.assertEqual(plan["eligible_file_count"], 3)

    def test_plan_expands_to_ten_for_trust_heavy_slice(self):
        review_state = load_review_state()
        feature = {
            "feature_id": "feat_1111111111111111",
            "entry_path": "src/main.rs",
            "files": [
                {"path": "src/main.rs", "role": "entry"},
                {"path": "src/owned.rs", "role": "owned"},
            ],
            "trust_boundaries": ["filesystem", "network", "secrets"],
        }
        risk = {"files": [{"file": "src/owned.rs", "score": 0.1}]}

        expanded = review_state.plan_feature_review(feature, risk, [])
        feature["trust_boundaries"] = ["filesystem", "network"]
        control = review_state.plan_feature_review(feature, risk, [])

        self.assertEqual(expanded["read_limit"], 10)
        self.assertIn("trust-boundaries", expanded["expansion_reasons"])
        self.assertEqual(control["read_limit"], 5)

    def test_plan_expands_to_ten_for_large_owned_set(self):
        review_state = load_review_state()

        def feature_with_owned(count):
            return {
                "feature_id": "feat_1111111111111111",
                "entry_path": "src/main.rs",
                "files": [{"path": "src/main.rs", "role": "entry"}]
                + [
                    {"path": f"src/owned_{index}.rs", "role": "owned"}
                    for index in range(count)
                ],
                "trust_boundaries": [],
            }

        expanded = review_state.plan_feature_review(feature_with_owned(8), [], [])
        control = review_state.plan_feature_review(feature_with_owned(7), [], [])

        self.assertEqual(expanded["read_limit"], 10)
        self.assertIn("large-owned-set", expanded["expansion_reasons"])
        self.assertEqual(control["read_limit"], 5)

    def test_plan_expands_to_ten_for_broadly_changed_slice(self):
        review_state = load_review_state()
        feature = {
            "feature_id": "feat_1111111111111111",
            "entry_path": "src/main.rs",
            "files": [{"path": "src/main.rs", "role": "entry"}]
            + [
                {"path": f"src/owned_{index}.rs", "role": "owned"}
                for index in range(6)
            ],
            "trust_boundaries": [],
        }
        changed = [f"src/owned_{index}.rs" for index in range(5)]

        expanded = review_state.plan_feature_review(feature, [], changed)
        control = review_state.plan_feature_review(feature, [], changed[:4])

        self.assertEqual(expanded["read_limit"], 10)
        self.assertIn("many-changed-files", expanded["expansion_reasons"])
        self.assertEqual(control["read_limit"], 5)

    def test_plan_prioritizes_entry_then_changed_then_risk_and_ignores_context(self):
        review_state = load_review_state()
        feature = {
            "feature_id": "feat_1111111111111111",
            "entry_path": "src/main.rs",
            "files": [
                {"path": "src/context.rs", "role": "context"},
                {"path": "src/low.rs", "role": "owned"},
                {"path": "src/main.rs", "role": "entry"},
                {"path": "src/high.rs", "role": "owned"},
                {"path": "src/mid.rs", "role": "owned"},
                {"path": "src/changed.rs", "role": "owned"},
            ],
            "trust_boundaries": ["filesystem"],
        }
        risk = {
            "files": [
                {"file": "src/main.rs", "score": 0.1},
                {"file": "src/changed.rs", "score": 0.05},
                {"file": "src/high.rs", "score": 0.49},
                {"file": "src/mid.rs", "score": 0.3},
                {"file": "src/low.rs", "score": 0.0},
            ]
        }

        plan = review_state.plan_feature_review(feature, risk, ["src/changed.rs"])

        self.assertEqual(
            plan["must_read"],
            [
                "src/main.rs",
                "src/changed.rs",
                "src/high.rs",
                "src/mid.rs",
                "src/low.rs",
            ],
        )
        self.assertEqual(plan["read_limit"], 5)
        self.assertEqual(plan["eligible_file_count"], 5)

    def test_plan_expands_to_ten_for_high_risk_slice(self):
        review_state = load_review_state()
        files = [{"path": "src/main.rs", "role": "entry"}] + [
            {"path": f"src/owned_{index}.rs", "role": "owned"}
            for index in range(1, 12)
        ]
        feature = {
            "feature_id": "feat_1111111111111111",
            "entry_path": "src/main.rs",
            "files": files,
            "trust_boundaries": [],
        }
        risk = {
            "files": [
                {"file": item["path"], "score": 0.8 if index == 0 else index / 100}
                for index, item in enumerate(files)
            ]
        }

        plan = review_state.plan_feature_review(feature, risk, [])

        self.assertEqual(plan["read_limit"], 10)
        self.assertEqual(len(plan["must_read"]), 10)
        self.assertEqual(plan["must_read"][0], "src/main.rs")
        self.assertIn("high-risk", plan["expansion_reasons"])


class EvidenceValidationTests(unittest.TestCase):
    def setUp(self):
        self.review_state = load_review_state()
        self.tempdir = tempfile.TemporaryDirectory()
        self.project = Path(self.tempdir.name)
        source = self.project / "src" / "handler.rs"
        source.parent.mkdir()
        source.write_text(
            "fn handle(input: &str) {\n"
            "    let normalized = input.trim().to_ascii_lowercase();\n"
            "    execute_without_validation(&normalized);\n"
            "}\n"
            "const REPEATED_DIAGNOSTIC_MESSAGE_FOR_OPERATORS: &str = \"same\";\n"
            "const REPEATED_DIAGNOSTIC_MESSAGE_FOR_OPERATORS: &str = \"same\";\n"
        )

    def tearDown(self):
        self.tempdir.cleanup()

    def finding(self, line, evidence, title="Unvalidated input reaches executor"):
        return {
            "file": "src/handler.rs",
            "line": line,
            "severity": "medium",
            "category": "security",
            "title": title,
            "summary": "The normalized input reaches the executor without validation.",
            "evidence": evidence,
            "suggested_fix": "Validate before execution.",
        }

    def test_two_consecutive_nearby_evidence_lines_pass(self):
        response = {
            "feature_id": "feat_1111111111111111",
            "findings": [
                self.finding(
                    2,
                    [
                        "    let normalized = input.trim().to_ascii_lowercase();",
                        "    execute_without_validation(&normalized);",
                    ],
                )
            ],
        }

        validated = self.review_state.validate_response(
            self.project, response, {"findings": []}
        )

        self.assertEqual(len(validated["findings"]), 1)
        self.assertEqual(validated["findings"][0]["line"], 2)
        self.assertEqual(validated["evidence_rejected"], [])

    def test_short_two_line_code_evidence_passes_but_one_short_line_does_not(self):
        source = self.project / "src" / "short.rs"
        source.write_text("fn short() {\n    let x = 1;\n    let y = 2;\n}\n")
        finding = self.finding(2, ["let x = 1;", "let y = 2;"])
        finding["file"] = "src/short.rs"
        short_pair = {
            "feature_id": "feat_1111111111111111",
            "findings": [finding],
        }
        short_single = copy.deepcopy(short_pair)
        short_single["findings"][0]["evidence"] = ["let x = 1;"]

        pair_validated = self.review_state.validate_response(
            self.project, short_pair, {"findings": []}
        )
        single_validated = self.review_state.validate_response(
            self.project, short_single, {"findings": []}
        )

        self.assertEqual(len(pair_validated["findings"]), 1)
        self.assertEqual(pair_validated["findings"][0]["line"], 2)
        self.assertEqual(single_validated["findings"], [])

    def test_two_line_evidence_must_fit_entirely_inside_citation_window(self):
        source = self.project / "src" / "window.rs"
        lines = [f"let filler_{index} = {index};" for index in range(1, 41)]
        source.write_text("\n".join(lines) + "\n")

        inside = self.review_state.evidence_match(
            source,
            20,
            {"evidence": [lines[33], lines[34]]},
        )
        outside = self.review_state.evidence_match(
            source,
            20,
            {"evidence": [lines[34], lines[35]]},
        )

        self.assertEqual(inside, 34)
        self.assertIsNone(outside)

    def test_brace_only_and_repeated_generic_evidence_are_rejected(self):
        response = {
            "feature_id": "feat_1111111111111111",
            "findings": [
                self.finding(4, ["}"], "Brace locus"),
                self.finding(
                    5,
                    ["const REPEATED_DIAGNOSTIC_MESSAGE_FOR_OPERATORS: &str = \"same\";"],
                    "Repeated generic locus",
                ),
            ],
        }

        validated = self.review_state.validate_response(
            self.project, response, {"findings": []}
        )

        self.assertEqual(validated["findings"], [])
        self.assertEqual(len(validated["evidence_rejected"]), 2)

    def test_two_line_block_matches_across_blank_source_line(self):
        source = self.project / "src" / "spaced.py"
        source.write_text("def foo():\n\n    return bar\n")
        finding = self.finding(1, ["def foo():", "    return bar"])
        finding["file"] = "src/spaced.py"
        response = {"feature_id": "feat_1111111111111111", "findings": [finding]}

        validated = self.review_state.validate_response(
            self.project, response, {"findings": []}
        )

        self.assertEqual(len(validated["findings"]), 1)
        self.assertEqual(validated["findings"][0]["line"], 1)
        self.assertEqual(validated["evidence_rejected"], [])

    def test_combine_rejects_a_lens_response_reporting_an_error(self):
        with self.assertRaises(ValueError) as caught:
            self.review_state.combine_responses(
                [
                    {"feature_id": "feat_1111111111111111", "findings": []},
                    {"feature_id": "feat_1111111111111111", "error": "lens aborted"},
                ]
            )

        self.assertIn("lens aborted", str(caught.exception))

    def test_malformed_finding_is_rejected_without_crashing_validation(self):
        response = {
            "feature_id": "feat_1111111111111111",
            "findings": ["not-an-object"],
        }

        validated = self.review_state.validate_response(
            self.project, response, {"findings": []}
        )

        self.assertEqual(validated["findings"], [])
        self.assertEqual(
            validated["evidence_rejected"][0]["reason"], "finding must be an object"
        )

    def test_duplicate_lens_candidates_are_collapsed_before_id_assignment(self):
        finding = self.finding(
            2,
            [
                "    let normalized = input.trim().to_ascii_lowercase();",
                "    execute_without_validation(&normalized);",
            ],
        )
        response = {
            "feature_id": "feat_1111111111111111",
            "findings": [finding, dict(finding)],
        }

        validated = self.review_state.validate_response(
            self.project, response, {"findings": []}
        )

        self.assertEqual(len(validated["findings"]), 1)
        self.assertEqual(
            validated["evidence_rejected"][0]["reason"],
            "duplicate candidate in response",
        )

    def test_combine_responses_rejects_mixed_features(self):
        with self.assertRaisesRegex(ValueError, "different feature IDs"):
            self.review_state.combine_responses(
                [
                    {"feature_id": "feat_1111111111111111", "findings": []},
                    {"feature_id": "feat_2222222222222222", "findings": []},
                ]
            )

    def test_combine_responses_rejects_a_non_object_first_response(self):
        with self.assertRaisesRegex(ValueError, "review response must be an object"):
            self.review_state.combine_responses(["not-an-object"])

    def test_combine_responses_unions_reviewed_files_in_response_order(self):
        combined = self.review_state.combine_responses(
            [
                {
                    "feature_id": "feat_1111111111111111",
                    "reviewed_files": ["src/handler.rs", "src/worker.rs"],
                    "findings": [],
                },
                {
                    "feature_id": "feat_1111111111111111",
                    "reviewed_files": ["src/worker.rs", "tests/handler.rs"],
                    "findings": [],
                },
            ]
        )

        self.assertEqual(
            combined["reviewed_files"],
            ["src/handler.rs", "src/worker.rs", "tests/handler.rs"],
        )

    def test_validation_requires_every_planned_file_to_be_declared(self):
        response = {
            "feature_id": "feat_1111111111111111",
            "reviewed_files": [],
            "findings": [],
        }

        with self.assertRaisesRegex(ValueError, "missing required reviewed files"):
            self.review_state.validate_response(
                self.project,
                response,
                {"findings": []},
                {"must_read": ["src/handler.rs"]},
            )

    def test_validation_accepts_and_preserves_complete_reviewed_files(self):
        response = {
            "feature_id": "feat_1111111111111111",
            "reviewed_files": ["src/handler.rs", "src/handler.rs"],
            "findings": [],
        }
        plan = {
            "feature_id": "feat_1111111111111111",
            "must_read": ["src/handler.rs"],
        }

        validated = self.review_state.validate_response(
            self.project, response, {"findings": []}, plan
        )

        self.assertEqual(validated["reviewed_files"], ["src/handler.rs"])

    def test_validation_rejects_a_plan_for_another_feature(self):
        response = {
            "feature_id": "feat_1111111111111111",
            "reviewed_files": ["src/handler.rs"],
            "findings": [],
        }

        with self.assertRaisesRegex(ValueError, "plan feature_id does not match"):
            self.review_state.validate_response(
                self.project,
                response,
                {"findings": []},
                {
                    "feature_id": "feat_2222222222222222",
                    "must_read": ["src/handler.rs"],
                },
            )


class IdentityTests(unittest.TestCase):
    def test_nearby_similar_title_reuses_prior_id_despite_new_evidence(self):
        review_state = load_review_state()
        title = "Operation lacks validation"
        replacement = "fn first() { dangerous_rewritten_operation_with_untrusted_value(); }"
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text(
                "let a = 1;\nlet b = 2;\nlet c = 3;\nlet d = 4;\n" + replacement + "\n"
            )
            existing = {
                "findings": [
                    {
                        "finding_id": "fnd_aaaaaaaa",
                        "file": "src/lib.rs",
                        "line": 1,
                        "severity": "medium",
                        "category": "bug",
                        "title": title,
                        "summary": "First summary",
                        "evidence": [
                            "fn first() { dangerous_first_operation_with_untrusted_value(); }"
                        ],
                        "status": "open",
                    }
                ]
            }
            response = {
                "feature_id": "feat_1111111111111111",
                "findings": [
                    {
                        "file": "src/lib.rs",
                        "line": 5,
                        "severity": "medium",
                        "category": "bug",
                        "title": title,
                        "summary": "Rewritten but same defect",
                        "evidence": [replacement],
                        "suggested_fix": "Add the missing validation.",
                    }
                ],
            }

            validated = review_state.validate_response(project, response, existing)

        self.assertEqual(validated["findings"][0]["finding_id"], "fnd_aaaaaaaa")
        self.assertEqual(validated["recurring_finding_ids"], ["fnd_aaaaaaaa"])
        self.assertEqual(validated["new_finding_ids"], [])

    def test_returned_prior_id_is_rejected_for_an_unrelated_finding(self):
        review_state = load_review_state()
        original = "fn first() { dangerous_first_operation_with_untrusted_value(); }"
        replacement = "fn second() { dangerous_second_operation_without_bounds_check(); }"
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text(original + "\n" + replacement + "\n")
            existing = {
                "findings": [
                    {
                        "finding_id": "fnd_aaaaaaaa",
                        "file": "src/lib.rs",
                        "line": 1,
                        "severity": "medium",
                        "category": "bug",
                        "title": "First operation trusts input",
                        "summary": "First summary",
                        "evidence": [original],
                        "status": "open",
                    }
                ]
            }
            response = {
                "feature_id": "feat_1111111111111111",
                "findings": [
                    {
                        "finding_id": "fnd_aaaaaaaa",
                        "file": "src/lib.rs",
                        "line": 2,
                        "severity": "medium",
                        "category": "bug",
                        "title": "Second operation lacks bounds check",
                        "summary": "Second summary",
                        "evidence": [replacement],
                        "suggested_fix": "Add the missing bound.",
                    }
                ],
            }

            validated = review_state.validate_response(project, response, existing)

        self.assertNotEqual(validated["findings"][0]["finding_id"], "fnd_aaaaaaaa")
        self.assertEqual(validated["recurring_finding_ids"], [])
        self.assertEqual(len(validated["new_finding_ids"]), 1)

    def test_returned_prior_id_and_same_title_do_not_override_changed_identity(self):
        review_state = load_review_state()
        title = "Operation lacks validation"
        original = "fn first() { dangerous_first_operation_with_untrusted_value(); }"
        replacement = "fn moved() { unrelated_operation_without_bounds_check(); }"
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text(original + "\n" + "\n" * 29 + replacement + "\n")
            existing = {
                "findings": [
                    {
                        "finding_id": "fnd_aaaaaaaa",
                        "file": "src/lib.rs",
                        "line": 1,
                        "severity": "medium",
                        "category": "bug",
                        "title": title,
                        "summary": "First summary",
                        "evidence": [original],
                        "status": "open",
                    }
                ]
            }
            response = {
                "feature_id": "feat_1111111111111111",
                "findings": [
                    {
                        "finding_id": "fnd_aaaaaaaa",
                        "file": "src/lib.rs",
                        "line": 31,
                        "severity": "medium",
                        "category": "bug",
                        "title": title,
                        "summary": "Different summary",
                        "evidence": [replacement],
                        "suggested_fix": "Add the missing bound.",
                    }
                ],
            }

            validated = review_state.validate_response(project, response, existing)

        self.assertNotEqual(validated["findings"][0]["finding_id"], "fnd_aaaaaaaa")

    def test_nearby_same_category_findings_do_not_collapse_by_line(self):
        review_state = load_review_state()
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text(
                "fn first() { dangerous_first_operation_with_untrusted_value(); }\n"
                "fn second() { dangerous_second_operation_without_bounds_check(); }\n"
            )
            existing = {
                "findings": [
                    {
                        "finding_id": "fnd_aaaaaaaa",
                        "file": "src/lib.rs",
                        "line": 1,
                        "severity": "medium",
                        "category": "bug",
                        "title": "First operation trusts input",
                        "summary": "First summary",
                        "evidence": [
                            "fn first() { dangerous_first_operation_with_untrusted_value(); }"
                        ],
                        "status": "open",
                    },
                    {
                        "finding_id": "fnd_bbbbbbbb",
                        "file": "src/lib.rs",
                        "line": 2,
                        "severity": "medium",
                        "category": "bug",
                        "title": "Second operation lacks bounds check",
                        "summary": "Second summary",
                        "evidence": [
                            "fn second() { dangerous_second_operation_without_bounds_check(); }"
                        ],
                        "status": "open",
                    },
                ]
            }
            response = {
                "feature_id": "feat_1111111111111111",
                "findings": [
                    {
                        "file": "src/lib.rs",
                        "line": 2,
                        "severity": "medium",
                        "category": "bug",
                        "title": "Second operation lacks bounds check",
                        "summary": "Updated second summary",
                        "evidence": [
                            "fn second() { dangerous_second_operation_without_bounds_check(); }"
                        ],
                        "suggested_fix": "Add the missing bound.",
                    }
                ],
            }

            validated = review_state.validate_response(project, response, existing)

        self.assertEqual(validated["recurring_finding_ids"], ["fnd_bbbbbbbb"])
        self.assertEqual(validated["findings"][0]["finding_id"], "fnd_bbbbbbbb")

    def test_identical_evidence_keeps_identity_after_large_line_move(self):
        review_state = load_review_state()
        evidence = "fn moved() { perform_sensitive_operation_without_guard(); }"
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text("\n" * 30 + evidence + "\n")
            existing = {
                "findings": [
                    {
                        "finding_id": "fnd_cccccccc",
                        "file": "src/lib.rs",
                        "line": 2,
                        "severity": "medium",
                        "category": "security",
                        "title": "Sensitive operation lacks guard",
                        "summary": "Original summary",
                        "evidence": [evidence],
                        "status": "open",
                    }
                ]
            }
            response = {
                "feature_id": "feat_1111111111111111",
                "findings": [
                    {
                        "file": "src/lib.rs",
                        "line": 31,
                        "severity": "medium",
                        "category": "security",
                        "title": "Sensitive operation lacks guard",
                        "summary": "Moved summary",
                        "evidence": [evidence],
                        "suggested_fix": "Add a guard.",
                    }
                ],
            }

            validated = review_state.validate_response(project, response, existing)

        self.assertEqual(validated["findings"][0]["finding_id"], "fnd_cccccccc")


class MergeTests(unittest.TestCase):
    def finding(self, finding_id, status):
        return {
            "finding_id": finding_id,
            "file": "src/lib.rs",
            "line": 1,
            "severity": "medium",
            "category": "bug",
            "title": f"Finding {finding_id}",
            "summary": "Summary",
            "evidence": ["fn value() { operation_with_a_substantive_evidence_line(); }"],
            "suggested_fix": "Fix it.",
            "status": status,
            "first_seen_at": "2026-06-01T00:00:00Z",
            "last_seen_at": "2026-06-01T00:00:00Z",
            "history": [{"action": "review", "at": "2026-06-01T00:00:00Z"}],
        }

    def test_missing_findings_keep_status_and_only_open_gets_one_pending_event(self):
        review_state = load_review_state()
        existing = {
            "findings": [
                self.finding("fnd_11111111", "open"),
                self.finding("fnd_22222222", "fixed"),
                self.finding("fnd_33333333", "false-positive"),
                self.finding("fnd_44444444", "wont-fix"),
            ]
        }
        validated = {
            "feature_id": "feat_1111111111111111",
            "findings": [],
            "new_finding_ids": [],
            "recurring_finding_ids": [],
            "evidence_rejected": [],
        }
        feature = {
            "feature_id": "feat_1111111111111111",
            "title": "Library",
            "kind": "library",
            "entry_path": "src/lib.rs",
            "files": [{"path": "src/lib.rs", "role": "entry"}],
        }
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text("fn value() {}\n")
            first, _ = review_state.merge_document(
                project,
                feature,
                existing,
                validated,
                {"verdicts": []},
                "run_1",
                "2026-07-15T00:00:00Z",
                "review",
            )
            second, _ = review_state.merge_document(
                project,
                feature,
                first,
                validated,
                {"verdicts": []},
                "run_2",
                "2026-07-15T01:00:00Z",
                "review",
            )

        by_id = {finding["finding_id"]: finding for finding in second["findings"]}
        self.assertEqual(by_id["fnd_11111111"]["status"], "open")
        self.assertEqual(
            [event["action"] for event in by_id["fnd_11111111"]["history"]].count(
                "not-seen"
            ),
            1,
        )
        for finding_id in ("fnd_22222222", "fnd_33333333", "fnd_44444444"):
            self.assertEqual(len(by_id[finding_id]["history"]), 1)

    def test_merge_persists_feature_metadata_and_drops_refuted_new_finding(self):
        review_state = load_review_state()
        new_finding = self.finding("fnd_55555555", "open")
        new_finding.pop("status")
        new_finding.pop("first_seen_at")
        new_finding.pop("last_seen_at")
        new_finding.pop("history")
        feature = {
            "feature_id": "feat_1111111111111111",
            "title": "Library",
            "kind": "library",
            "entry_path": "src/lib.rs",
            "files": [
                {"path": "src/lib.rs", "role": "entry"},
                {"path": "src/worker.rs", "role": "owned"},
            ],
        }
        validated = {
            "feature_id": feature["feature_id"],
            "findings": [new_finding],
            "new_finding_ids": ["fnd_55555555"],
            "recurring_finding_ids": [],
            "evidence_rejected": [],
        }
        verdicts = {
            "verdicts": [
                {
                    "finding_id": "fnd_55555555",
                    "verdict": "refuted",
                    "refutation": "A caller guard proves the path unreachable.",
                }
            ]
        }
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            (project / "src").mkdir()
            (project / "src" / "lib.rs").write_text("fn value() {}\n")
            (project / "src" / "worker.rs").write_text("fn work() {}\n")

            merged, summary = review_state.merge_document(
                project,
                feature,
                {"findings": []},
                validated,
                verdicts,
                "run_1",
                "2026-07-15T00:00:00Z",
                "review",
            )

        self.assertEqual(merged["findings"], [])
        self.assertEqual(merged["entry_path"], "src/lib.rs")
        self.assertEqual(merged["kind"], "library")
        self.assertEqual(merged["title"], "Library")
        self.assertEqual(len(merged["feature_files"]), 2)
        self.assertEqual(summary["refuted"][0]["finding_id"], "fnd_55555555")

    def test_revalidation_reopens_fixed_only_when_evidence_validated_it_as_present(self):
        review_state = load_review_state()
        prior = self.finding("fnd_66666666", "fixed")
        validated_finding = {
            field: prior[field]
            for field in (
                "finding_id",
                "file",
                "line",
                "severity",
                "category",
                "title",
                "summary",
                "evidence",
                "suggested_fix",
            )
        }
        feature = {
            "feature_id": "feat_1111111111111111",
            "title": "Library",
            "kind": "library",
            "entry_path": "src/lib.rs",
            "files": [{"path": "src/lib.rs", "role": "entry"}],
        }
        validated = {
            "feature_id": feature["feature_id"],
            "findings": [validated_finding],
            "new_finding_ids": [],
            "recurring_finding_ids": ["fnd_66666666"],
            "evidence_rejected": [],
        }
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text("fn value() {}\n")

            merged, _ = review_state.merge_document(
                project,
                feature,
                {"findings": [prior]},
                validated,
                {"verdicts": []},
                "rev_1",
                "2026-07-15T00:00:00Z",
                "revalidate",
                {"fnd_66666666"},
            )

        finding = merged["findings"][0]
        self.assertEqual(finding["status"], "open")
        self.assertEqual(finding["history"][-1]["from_status"], "fixed")
        self.assertEqual(finding["history"][-1]["to_status"], "open")

    def test_revalidation_does_not_mark_unselected_open_finding_not_seen(self):
        review_state = load_review_state()
        selected = self.finding("fnd_77777777", "open")
        unselected = self.finding("fnd_88888888", "open")
        feature = {
            "feature_id": "feat_1111111111111111",
            "title": "Library",
            "kind": "library",
            "entry_path": "src/lib.rs",
            "files": [{"path": "src/lib.rs", "role": "entry"}],
        }
        validated = {
            "feature_id": feature["feature_id"],
            "findings": [],
            "new_finding_ids": [],
            "recurring_finding_ids": [],
            "evidence_rejected": [],
        }
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text("fn value() {}\n")

            merged, summary = review_state.merge_document(
                project,
                feature,
                {"findings": [selected, unselected]},
                validated,
                {"verdicts": []},
                "rev_1",
                "2026-07-15T00:00:00Z",
                "revalidate",
                {"fnd_77777777"},
            )

        by_id = {finding["finding_id"]: finding for finding in merged["findings"]}
        self.assertEqual(by_id["fnd_77777777"]["history"][-1]["action"], "not-seen")
        self.assertEqual(len(by_id["fnd_88888888"]["history"]), 1)
        self.assertEqual(summary["not_seen"], ["fnd_77777777"])

    def test_atomic_write_refuses_a_symlinked_findings_dir(self):
        # `.codesage/findings` ships with the repo, so it can be a directory
        # symlink; mkdir/temp/replace would then land outside the project.
        review_state = load_review_state()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            outside = root / "outside"
            outside.mkdir()
            codesage = root / "project" / ".codesage"
            codesage.mkdir(parents=True)
            (codesage / "findings").symlink_to(outside, target_is_directory=True)

            destination = codesage / "findings" / "feat.json"
            with self.assertRaises(OSError):
                review_state.atomic_write_json(destination, {"value": 42})

            self.assertEqual(list(outside.iterdir()), [])

    def test_load_json_refuses_a_symlinked_source(self):
        # The real target of concern is a character device such as /dev/zero,
        # but this points at an ordinary file on purpose: reverting the guard
        # must fail this test deterministically rather than hang the suite.
        review_state = load_review_state()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            outside = root / "outside.json"
            outside.write_text(json.dumps({"value": "victim"}))
            codesage = root / ".codesage"
            (codesage / "findings").mkdir(parents=True)
            source = codesage / "findings" / "feat.json"
            source.symlink_to(outside)

            with self.assertRaises(OSError):
                review_state.load_json(source)

    def test_load_json_refuses_an_oversized_source(self):
        review_state = load_review_state()
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "big.json"
            source.write_bytes(b"x" * (review_state.MAX_STATE_BYTES + 1))

            with self.assertRaises(OSError):
                review_state.load_json(source)

    def test_load_json_reads_an_ordinary_document(self):
        review_state = load_review_state()
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "ok.json"
            source.write_text(json.dumps({"value": 42}))

            self.assertEqual(review_state.load_json(source), {"value": 42})

    def test_atomic_write_leaves_complete_json_without_temporary_file(self):
        review_state = load_review_state()
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "state.json"
            review_state.atomic_write_json(destination, {"value": 42})

            loaded = json.loads(destination.read_text())
            leftovers = list(Path(directory).glob(".state.json.*.tmp"))

        self.assertEqual(loaded, {"value": 42})
        self.assertEqual(leftovers, [])


    def test_review_mode_reopens_fixed_prior_on_evidence_match(self):
        review_state = load_review_state()
        prior = self.finding("fnd_99999999", "fixed")
        validated_finding = {
            field: prior[field] for field in review_state.FINDING_FIELDS
        }
        feature = {
            "feature_id": "feat_1111111111111111",
            "title": "Library",
            "kind": "library",
            "entry_path": "src/lib.rs",
            "files": [{"path": "src/lib.rs", "role": "entry"}],
        }
        validated = {
            "feature_id": feature["feature_id"],
            "findings": [validated_finding],
            "new_finding_ids": [],
            "recurring_finding_ids": ["fnd_99999999"],
            "evidence_rejected": [],
        }
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text("fn value() {}\n")

            merged, summary = review_state.merge_document(
                project,
                feature,
                {"findings": [prior]},
                validated,
                {"verdicts": []},
                "run_1",
                "2026-07-15T00:00:00Z",
                "review",
            )

        finding = merged["findings"][0]
        self.assertEqual(finding["status"], "open")
        self.assertEqual(finding["history"][-1]["action"], "review")
        self.assertEqual(finding["history"][-1]["result"], "reopened")
        self.assertEqual(finding["history"][-1]["from_status"], "fixed")
        self.assertEqual(finding["history"][-1]["to_status"], "open")
        self.assertEqual(summary["reopened"], ["fnd_99999999"])

    def test_revalidation_ignores_rediscovered_untargeted_prior(self):
        review_state = load_review_state()
        targeted = self.finding("fnd_aaaaaaa1", "open")
        untargeted = self.finding("fnd_bbbbbbb2", "wont-fix")
        feature = {
            "feature_id": "feat_1111111111111111",
            "title": "Library",
            "kind": "library",
            "entry_path": "src/lib.rs",
            "files": [{"path": "src/lib.rs", "role": "entry"}],
        }
        validated = {
            "feature_id": feature["feature_id"],
            "findings": [
                {field: targeted[field] for field in review_state.FINDING_FIELDS},
                {field: untargeted[field] for field in review_state.FINDING_FIELDS},
            ],
            "new_finding_ids": [],
            "recurring_finding_ids": ["fnd_aaaaaaa1", "fnd_bbbbbbb2"],
            "evidence_rejected": [],
        }
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text("fn value() {}\n")

            merged, summary = review_state.merge_document(
                project,
                feature,
                {"findings": [targeted, untargeted]},
                validated,
                {"verdicts": []},
                "rev_1",
                "2026-07-15T00:00:00Z",
                "revalidate",
                {"fnd_aaaaaaa1"},
            )

        by_id = {finding["finding_id"]: finding for finding in merged["findings"]}
        self.assertEqual(summary["out_of_scope"], ["fnd_bbbbbbb2"])
        self.assertEqual(by_id["fnd_bbbbbbb2"]["status"], "wont-fix")
        self.assertEqual(len(by_id["fnd_bbbbbbb2"]["history"]), 1)
        self.assertEqual(by_id["fnd_aaaaaaa1"]["history"][-1]["result"], "still-present")

    def test_reemitted_false_positive_keeps_status_through_revalidation(self):
        review_state = load_review_state()
        prior = self.finding("fnd_ccccccc3", "false-positive")
        feature = {
            "feature_id": "feat_1111111111111111",
            "title": "Library",
            "kind": "library",
            "entry_path": "src/lib.rs",
            "files": [{"path": "src/lib.rs", "role": "entry"}],
        }
        validated = {
            "feature_id": feature["feature_id"],
            "findings": [
                {field: prior[field] for field in review_state.FINDING_FIELDS}
            ],
            "new_finding_ids": [],
            "recurring_finding_ids": ["fnd_ccccccc3"],
            "evidence_rejected": [],
        }
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text("fn value() {}\n")

            merged, summary = review_state.merge_document(
                project,
                feature,
                {"findings": [prior]},
                validated,
                {"verdicts": []},
                "rev_1",
                "2026-07-15T00:00:00Z",
                "revalidate",
                {"fnd_ccccccc3"},
            )

        finding = merged["findings"][0]
        self.assertEqual(finding["status"], "false-positive")
        self.assertEqual(finding["history"][-1]["result"], "still-present")
        self.assertNotIn("from_status", finding["history"][-1])
        self.assertEqual(summary["reopened"], [])

    def test_review_merge_stores_plan_state_and_trust_boundaries(self):
        review_state = load_review_state()
        feature = {
            "feature_id": "feat_1111111111111111",
            "title": "Library",
            "kind": "library",
            "entry_path": "src/lib.rs",
            "files": [{"path": "src/lib.rs", "role": "entry"}],
            "trust_boundaries": ["filesystem", "network"],
        }
        validated = {
            "feature_id": feature["feature_id"],
            "findings": [],
            "new_finding_ids": [],
            "recurring_finding_ids": [],
            "evidence_rejected": [],
        }
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text("fn value() {}\n")

            merged, _ = review_state.merge_document(
                project,
                feature,
                {"findings": []},
                validated,
                {"verdicts": []},
                "run_1",
                "2026-07-15T00:00:00Z",
                "review",
                None,
                reviewed_state="sha256:plan-time-state",
            )

        self.assertEqual(merged["reviewed_state"], "sha256:plan-time-state")
        self.assertEqual(merged["trust_boundaries"], ["filesystem", "network"])

    def test_revalidation_preserves_freshness_state(self):
        review_state = load_review_state()
        prior = self.finding("fnd_ddddddd4", "open")
        feature = {
            "feature_id": "feat_1111111111111111",
            "title": "Library",
            "kind": "library",
            "entry_path": "src/lib.rs",
            "files": [{"path": "src/lib.rs", "role": "entry"}],
        }
        existing = {
            "findings": [prior],
            "reviewed_state": "sha256:before-revalidation",
            "reviewed_at_sha": "abc123def456",
        }
        validated = {
            "feature_id": feature["feature_id"],
            "findings": [],
            "new_finding_ids": [],
            "recurring_finding_ids": [],
            "evidence_rejected": [],
        }
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            source = project / "src" / "lib.rs"
            source.parent.mkdir()
            source.write_text("fn value() {}\n")

            merged, _ = review_state.merge_document(
                project,
                feature,
                existing,
                validated,
                {"verdicts": []},
                "rev_1",
                "2026-07-15T00:00:00Z",
                "revalidate",
                {"fnd_ddddddd4"},
            )

        self.assertEqual(merged["reviewed_state"], "sha256:before-revalidation")
        self.assertEqual(merged["reviewed_at_sha"], "abc123def456")

    def test_atomic_write_failure_preserves_destination_and_cleans_temp(self):
        review_state = load_review_state()
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "state.json"
            review_state.atomic_write_json(destination, {"value": 1})

            with mock.patch("json.dump", side_effect=RuntimeError("disk exploded")):
                with self.assertRaises(RuntimeError):
                    review_state.atomic_write_json(destination, {"value": 2})

            loaded = json.loads(destination.read_text())
            leftovers = list(Path(directory).glob(".state.json.*.tmp"))

        self.assertEqual(loaded, {"value": 1})
        self.assertEqual(leftovers, [])


class CliContractTests(unittest.TestCase):
    def test_plan_feature_cli_writes_the_same_plan_it_prints(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            feature_path = root / "feature.json"
            risk_path = root / "risk.json"
            changed_path = root / "changed.json"
            output_path = root / "plan.json"
            feature_path.write_text(
                json.dumps(
                    {
                        "feature_id": "feat_1111111111111111",
                        "entry_path": "src/main.rs",
                        "files": [
                            {"path": "src/main.rs", "role": "entry"},
                            {"path": "src/high.rs", "role": "owned"},
                        ],
                    }
                )
            )
            risk_path.write_text(
                json.dumps(
                    {
                        "files": [
                            {"file": "src/main.rs", "score": 0.1},
                            {"file": "src/high.rs", "score": 0.6},
                        ]
                    }
                )
            )
            changed_path.write_text("[]\n")

            completed = subprocess.run(
                [
                    str(SCRIPT),
                    "plan-feature",
                    "--feature",
                    str(feature_path),
                    "--risk",
                    str(risk_path),
                    "--changed",
                    str(changed_path),
                    "--output",
                    str(output_path),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            written = json.loads(output_path.read_text())

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(json.loads(completed.stdout), written)


    def test_validate_cli_rejects_missing_coverage_with_nonzero_exit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            (project / "src").mkdir(parents=True)
            (project / "src" / "handler.rs").write_text("fn handle() {}\n")
            (project / "src" / "other.rs").write_text("fn other() {}\n")
            plan_path = root / "plan.json"
            plan_path.write_text(
                json.dumps(
                    {
                        "feature_id": "feat_1111111111111111",
                        "must_read": ["src/handler.rs", "src/other.rs"],
                    }
                )
            )
            response_path = root / "response.json"
            response_path.write_text(
                json.dumps(
                    {
                        "feature_id": "feat_1111111111111111",
                        "reviewed_files": ["src/handler.rs"],
                        "findings": [],
                    }
                )
            )

            completed = subprocess.run(
                [
                    str(SCRIPT),
                    "validate",
                    "--project",
                    str(project),
                    "--response",
                    str(response_path),
                    "--must-read",
                    str(plan_path),
                ],
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertEqual(completed.returncode, 1, completed.stdout)
        self.assertIn("codesage-review-state:", completed.stderr)
        self.assertIn("missing required reviewed files", completed.stderr)
        self.assertIn("src/other.rs", completed.stderr)

    def test_validate_cli_accepts_complete_coverage_through_must_read_flag(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            (project / "src").mkdir(parents=True)
            (project / "src" / "handler.rs").write_text("fn handle() {}\n")
            (project / "src" / "other.rs").write_text("fn other() {}\n")
            plan_path = root / "plan.json"
            plan_path.write_text(
                json.dumps(
                    {
                        "feature_id": "feat_1111111111111111",
                        "must_read": ["src/handler.rs", "src/other.rs"],
                    }
                )
            )
            response_path = root / "response.json"
            response_path.write_text(
                json.dumps(
                    {
                        "feature_id": "feat_1111111111111111",
                        "reviewed_files": ["src/handler.rs", "src/other.rs"],
                        "findings": [],
                    }
                )
            )
            output_path = root / "validated.json"

            completed = subprocess.run(
                [
                    str(SCRIPT),
                    "validate",
                    "--project",
                    str(project),
                    "--response",
                    str(response_path),
                    "--must-read",
                    str(plan_path),
                    "--output",
                    str(output_path),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            written = json.loads(output_path.read_text())

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            written["reviewed_files"], ["src/handler.rs", "src/other.rs"]
        )

    def test_merge_cli_downgrades_snapshot_failure_to_warning(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project = root / "project"
            (project / "src").mkdir(parents=True)
            (project / "src" / "lib.rs").write_text("fn value() {}\n")
            findings_dir = project / ".codesage" / "findings"
            findings_dir.mkdir(parents=True)
            (findings_dir / "history").write_text("not a directory\n")
            feature_path = root / "feature.json"
            feature_path.write_text(
                json.dumps(
                    {
                        "feature_id": "feat_1111111111111111",
                        "title": "Library",
                        "kind": "library",
                        "entry_path": "src/lib.rs",
                        "files": [{"path": "src/lib.rs", "role": "entry"}],
                    }
                )
            )
            validated_path = root / "validated.json"
            validated_path.write_text(
                json.dumps(
                    {
                        "feature_id": "feat_1111111111111111",
                        "findings": [],
                        "new_finding_ids": [],
                        "recurring_finding_ids": [],
                        "evidence_rejected": [],
                    }
                )
            )

            completed = subprocess.run(
                [
                    str(SCRIPT),
                    "merge",
                    "--project",
                    str(project),
                    "--feature",
                    str(feature_path),
                    "--validated",
                    str(validated_path),
                    "--run-id",
                    "run_1",
                    "--mode",
                    "review",
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            merged_exists = (findings_dir / "feat_1111111111111111.json").is_file()

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(merged_exists)
        self.assertIn("snapshot write failed", completed.stderr)
        self.assertIn("snapshot_error", completed.stdout)


class ProtocolContractTests(unittest.TestCase):
    def read(self, relative_path):
        return (PLUGIN_ROOT / relative_path).read_text()

    def test_review_command_routes_state_transitions_through_helper(self):
        command = self.read("commands/codesage-review.md")

        for subcommand in (
            "slim-priors",
            "feature-states",
            "plan-feature",
            "combine",
            "validate",
            "merge",
        ):
            self.assertIn(subcommand, command)
        self.assertIn("--focus all|product", command)
        self.assertIn("one `codesage-finding-verifier`", command)
        self.assertIn("--must-read", command)
        self.assertIn("one focused `codesage-feature-reviewer`", command)
        self.assertNotIn("stat -c %Y", command)

    def test_revalidate_never_marks_open_fixed_from_omission(self):
        command = self.read("commands/codesage-revalidate.md")

        self.assertIn("needs-confirmation", command)
        self.assertIn("--mode revalidate", command)
        self.assertIn("plan-feature", command)
        self.assertIn("--must-read", command)
        self.assertIn("not proof of a fix", command)
        self.assertNotIn("flip to `fixed` automatically", command)

    def test_reviewer_uses_risk_batch_and_has_no_write_capable_shell(self):
        reviewer = self.read("agents/codesage-feature-reviewer.md")
        frontmatter = reviewer.split("---", 2)[1]

        self.assertIn("mcp__codesage__assess_risk_batch", frontmatter)
        self.assertNotIn("Bash", frontmatter)
        self.assertNotIn("mcp__codesage__find_coupling", frontmatter)
        self.assertIn("at most 800 lines", reviewer)
        self.assertIn("no semantic chunks", reviewer)
        self.assertIn("`must_read`", reviewer)
        self.assertIn('"reviewed_files"', reviewer)

    def test_verifier_batches_findings_without_shell_access(self):
        verifier = self.read("agents/codesage-finding-verifier.md")
        frontmatter = verifier.split("---", 2)[1]

        self.assertNotIn("Bash", frontmatter)
        self.assertIn('"verdicts"', verifier)
        self.assertIn("up to ten new findings", verifier)

    def test_triage_and_report_use_consistent_persisted_contracts(self):
        triage = self.read("commands/codesage-triage.md")
        report = self.read("commands/codesage-report.md")

        self.assertIn("Later reviews suppress it", triage)
        self.assertIn("Read `title`, `kind`, `entry_path`, and `feature_files`", report)
        self.assertIn("join records by `feature_id`", report)


class AcknowledgementTests(unittest.TestCase):
    def setUp(self):
        self.state = load_review_state()
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.project = Path(self.temporary.name)
        self.source = self.project / "old.rs"
        self.source.write_text("let pending_items = collect_without_limit();\n")
        self.feature = {"feature_id": "feat_123", "files": [{"path": "old.rs"}]}
        self.candidate = {
            "file": "old.rs", "line": 1, "severity": "medium", "category": "perf",
            "title": "Unbounded pending work", "summary": "Pending work exceeds the limit.",
            "evidence": ["let pending_items = collect_without_limit();"],
            "suggested_fix": "Bound pending work.",
            "magnitude": {"metric": "pending-items", "value": 10},
        }
        self.document, _ = self.review({}, [self.candidate])
        self.finding_id = self.document["findings"][0]["finding_id"]
        self.findings_path = self.state.default_findings_path(self.project, "feat_123")
        self.state.atomic_write_json(self.findings_path, self.document)
        self.state.triage(self.project, self.finding_id, "wont-fix",
                          {"metric": "pending-items", "value": 10})
        self.document = self.state.load_json(self.findings_path)

    def review(self, existing, candidates, mode="review", targets=None):
        validated = self.state.validate_response(
            self.project, {"feature_id": "feat_123", "findings": candidates}, existing)
        return self.state.merge_document(self.project, self.feature, existing, validated,
                                         {}, "run", "2026-09-07", mode, targets)

    def test_numeric_boundary_suppresses_equal_and_lower_reopens_higher(self):
        for value in (0, 9, 10, 11):
            with self.subTest(value=value):
                candidate = copy.deepcopy(self.candidate)
                candidate["magnitude"]["value"] = value
                document, summary = self.review(self.document, [candidate])
                finding = document["findings"][0]
                self.assertEqual(finding["status"], "open" if value > 10 else "wont-fix")
                self.assertEqual(summary["suppressed"], [] if value > 10 else [self.finding_id])
                if value > 10:
                    self.assertEqual(summary["ack_sweep"][0]["reason"], "magnitude-increased")
                    self.assertEqual(summary["reopened"], [self.finding_id])

    def test_incomparable_measurements_reopen_with_visible_reason(self):
        for magnitude, reason in ((None, "magnitude-missing"),
                                  ({"metric": "bytes", "value": 1}, "metric-changed")):
            candidate = copy.deepcopy(self.candidate)
            candidate.pop("magnitude")
            if magnitude is not None:
                candidate["magnitude"] = magnitude
            document, summary = self.review(self.document, [candidate])
            self.assertEqual(document["findings"][0]["status"], "open")
            self.assertEqual(summary["ack_sweep"][0]["reason"], reason)

    def test_unique_content_rename_survives_but_copy_or_rewrite_does_not(self):
        subprocess.run(["git", "init", "-q", str(self.project)], check=True)
        renamed = self.project / "new.rs"
        self.source.rename(renamed)
        candidate = {**self.candidate, "file": "new.rs"}
        document, summary = self.review(self.document, [candidate])
        self.assertEqual(len(document["findings"]), 1)
        self.assertEqual(document["findings"][0]["file"], "new.rs")
        self.assertEqual(summary["suppressed"], [self.finding_id])
        duplicate = self.project / "duplicate.rs"
        duplicate.write_bytes(renamed.read_bytes())
        document, summary = self.review(self.document, [candidate])
        self.assertEqual(len(document["findings"]), 2)
        self.assertEqual(summary["suppressed"], [])
        self.assertEqual(document["findings"][-1]["status"], "open")
        duplicate.unlink()
        renamed.write_text(renamed.read_text() + "fn unrelated_rewrite() {}\n")
        document, summary = self.review(self.document, [candidate])
        self.assertEqual(len(document["findings"]), 2)
        self.assertEqual(summary["suppressed"], [])

    def test_foreign_ack_sweep_is_persisted_and_target_scoped(self):
        document, summary = self.review(self.document, [])
        self.assertEqual(summary["ack_sweep"], [{"finding_id": self.finding_id,
                         "kind": "foreign", "reason": "not-emitted-by-current-review"}])
        self.assertEqual(document["ack_sweep"], summary["ack_sweep"])
        self.assertEqual(document["findings"][0]["status"], "wont-fix")
        _, summary = self.review(self.document, [], "revalidate", [])
        self.assertEqual(summary["ack_sweep"], [])

    def test_legacy_triage_keeps_suppression_without_numeric_assumptions(self):
        self.document["findings"][0].pop("acknowledgement")
        candidate = copy.deepcopy(self.candidate)
        candidate["magnitude"]["value"] = 100
        document, summary = self.review(self.document, [candidate])
        self.assertEqual(document["findings"][0]["status"], "wont-fix")
        self.assertEqual(summary["reopened"], [])

    def test_invalid_magnitudes_are_rejected(self):
        for value in (True, -1, float("nan"), float("inf"), "10"):
            candidate = copy.deepcopy(self.candidate)
            candidate["magnitude"]["value"] = value
            validated = self.state.validate_response(self.project,
                {"feature_id": "feat_123", "findings": [candidate]}, {})
            self.assertEqual(validated["findings"], [])
            self.assertIn("finite and nonnegative", validated["evidence_rejected"][0]["reason"])

    def test_triage_cli_requires_matching_measurement_and_preserves_history(self):
        command = [str(SCRIPT), "triage", "--project", str(self.project),
                   "--finding", self.finding_id, "--status", "false-positive"]
        failed = subprocess.run(command + ["--magnitude", "10"], capture_output=True, text=True)
        self.assertEqual(failed.returncode, 1)
        self.assertIn("supplied together", failed.stderr)
        success = subprocess.run(command + ["--magnitude", "12", "--metric", "pending-items"],
                                 capture_output=True, text=True)
        self.assertEqual(success.returncode, 0, success.stderr)
        finding = json.loads(success.stdout)["finding"]
        self.assertEqual(finding["acknowledgement"]["value"], 12)
        self.assertEqual(finding["history"][-1]["from_status"], "wont-fix")
        self.source.write_text(self.source.read_text() + "fn changed() {}\n")
        failed = subprocess.run(command + ["--magnitude", "12", "--metric", "pending-items"],
                                capture_output=True, text=True)
        self.assertEqual(failed.returncode, 1)
        self.assertIn("source changed since review", failed.stderr)

    def test_slim_priors_keep_ack_measurement_for_reviewer(self):
        slim = self.state.slim_priors(self.document)
        self.assertEqual(slim[0]["magnitude"], self.candidate["magnitude"])
        self.assertEqual(slim[0]["acknowledgement"]["value"], 10)

    def test_entrypoint_renames_transfer_between_feature_documents(self):
        subprocess.run(["git", "init", "-q", str(self.project)], check=True)
        old_id = self.finding_id
        for number in (1, 2):
            new_name = f"new{number}.rs"
            self.source.rename(self.project / new_name)
            self.source = self.project / new_name
            feature = {"feature_id": f"feat_new{number}", "files": [{"path": new_name}]}
            response = {"feature_id": feature["feature_id"],
                        "findings": [{**self.candidate, "file": new_name}]}
            validated = self.state.validate_response(self.project, response, {})
            document, summary = self.state.merge_document(self.project, feature, {}, validated,
                {}, "rename", "2026-09-07", "review")
            self.assertEqual(len(document["findings"]), 1)
            finding = document["findings"][0]
            self.assertEqual(finding["status"], "wont-fix")
            self.assertEqual(finding["ack_transferred_from"]["finding_id"], old_id)
            self.assertNotEqual(finding["finding_id"], old_id)
            self.assertEqual(summary["suppressed"], [finding["finding_id"]])
            old_id = finding["finding_id"]
            self.state.atomic_write_json(
                self.state.default_findings_path(self.project, feature["feature_id"]), document)
        sweep = subprocess.run([str(SCRIPT), "sweep-acks", "--project", str(self.project)],
                               capture_output=True, text=True)
        self.assertEqual(sweep.returncode, 0, sweep.stderr)
        diagnostics = json.loads(sweep.stdout)["ack_sweep"]
        self.assertEqual({entry["feature_id"] for entry in diagnostics}, {"feat_123", "feat_new1"})
        self.assertTrue(all(entry["kind"] == "transferred" for entry in diagnostics))
        self.assertTrue(all(entry["reason"] == "superseded-by-destination" for entry in diagnostics))

    def test_cross_feature_hash_ambiguity_does_not_transfer(self):
        subprocess.run(["git", "init", "-q", str(self.project)], check=True)
        duplicate_document = copy.deepcopy(self.document)
        duplicate_document["feature_id"] = "feat_duplicate"
        self.state.atomic_write_json(
            self.state.default_findings_path(self.project, "feat_duplicate"), duplicate_document)
        self.source.rename(self.project / "new.rs")
        response = {"feature_id": "feat_new", "findings": [{**self.candidate, "file": "new.rs"}]}
        validated = self.state.validate_response(self.project, response, {})
        self.assertEqual(validated["imported_priors"], [])
        self.assertEqual(len(validated["new_finding_ids"]), 1)

    def test_duplicate_lenses_cannot_hide_a_larger_or_incomparable_measurement(self):
        for magnitude in ({"metric": "pending-items", "value": 11},
                          {"metric": "bytes", "value": 1}, None):
            candidate = copy.deepcopy(self.candidate)
            candidate.pop("magnitude")
            if magnitude is not None:
                candidate["magnitude"] = magnitude
            for candidates in ([self.candidate, candidate], [candidate, self.candidate]):
                document, summary = self.review(self.document, candidates)
                self.assertEqual(document["findings"][0]["status"], "open")
                self.assertEqual(summary["suppressed"], [])

    def cli_review(self, name, feature_id):
        feature = self.project / "feature-input.json"
        response = self.project / "response-input.json"
        validated = self.project / "validated-input.json"
        feature.write_text(json.dumps({"feature_id": feature_id, "files": [{"path": name}]}))
        response.write_text(json.dumps({"feature_id": feature_id,
            "findings": [{**self.candidate, "file": name}]}))
        result = subprocess.run([str(SCRIPT), "validate", "--project", str(self.project),
            "--response", str(response), "--output", str(validated)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        result = subprocess.run([str(SCRIPT), "merge", "--project", str(self.project),
            "--feature", str(feature), "--validated", str(validated), "--run-id", "cli-test"],
            capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        summary = json.loads(result.stdout)
        document = self.state.load_json(self.state.default_findings_path(self.project, feature_id))
        current = [finding for finding in document["findings"] if not finding.get("ack_transferred_to")]
        self.assertEqual(len(current), 1)
        return current[0], summary

    def test_cli_revocation_survives_rename_back_and_longer_chains(self):
        subprocess.run(["git", "init", "-q", str(self.project)], check=True)
        for name, feature_id in (("b.rs", "feat_b"), ("c.rs", "feat_c")):
            self.source.rename(self.project / name)
            self.source = self.project / name
            current, summary = self.cli_review(name, feature_id)
            self.assertEqual(summary["suppressed"], [current["finding_id"]])
        result = subprocess.run([str(SCRIPT), "triage", "--project", str(self.project),
            "--finding", current["finding_id"], "--status", "open"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        for name, feature_id in (("old.rs", "feat_123"), ("b.rs", "feat_b"), ("old.rs", "feat_123")):
            self.source.rename(self.project / name)
            self.source = self.project / name
            current, summary = self.cli_review(name, feature_id)
            self.assertEqual(current["status"], "open")
            self.assertNotIn("acknowledgement", current)
            self.assertEqual(summary["suppressed"], [])

    def test_cli_return_rename_uses_latest_ack_and_refuses_old_triage(self):
        subprocess.run(["git", "init", "-q", str(self.project)], check=True)
        self.source.rename(self.project / "b.rs")
        self.source = self.project / "b.rs"
        current, _ = self.cli_review("b.rs", "feat_b")
        result = subprocess.run([str(SCRIPT), "triage", "--project", str(self.project),
            "--finding", current["finding_id"], "--status", "wont-fix", "--metric", "pending-items",
            "--magnitude", "12"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.source.rename(self.project / "old.rs")
        current, summary = self.cli_review("old.rs", "feat_123")
        self.assertEqual(current["acknowledgement"]["value"], 12)
        self.assertEqual(summary["suppressed"], [current["finding_id"]])
        result = subprocess.run([str(SCRIPT), "triage", "--project", str(self.project),
            "--finding", self.finding_id, "--status", "open"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("transferred", result.stderr)

    def test_cli_revocation_survives_immediate_rename_back(self):
        subprocess.run(["git", "init", "-q", str(self.project)], check=True)
        self.source.rename(self.project / "b.rs")
        current, _ = self.cli_review("b.rs", "feat_b")
        result = subprocess.run([str(SCRIPT), "triage", "--project", str(self.project),
            "--finding", current["finding_id"], "--status", "open"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        (self.project / "b.rs").rename(self.project / "old.rs")
        current, summary = self.cli_review("old.rs", "feat_123")
        self.assertEqual(current["status"], "open")
        self.assertNotIn("acknowledgement", current)
        self.assertEqual(summary["suppressed"], [])

    def test_revalidation_contract_keeps_numeric_prior_fields(self):
        text = (PLUGIN_ROOT / "commands" / "codesage-revalidate.md").read_text()
        projection = next(line for line in text.splitlines() if line.startswith("3. Project only"))
        self.assertIn("`magnitude`", projection)
        self.assertIn("`acknowledgement` unchanged", projection)


class InventoryReportTests(unittest.TestCase):
    def setUp(self):
        self.state = load_review_state()

    def test_inventory_rejects_under_cap_and_nested_truncation(self):
        feature = {"feature_id": "feat_1", "files": [{"path": "a.rs", "role": "owned"}]}
        for value in ({"results": [feature], "_meta": {"truncated": True, "total_results": 69}},
                      {"results": [{**feature, "_meta": {"truncated": True}}]},
                      {"results": [{"feature_id": "feat_1"}]}):
            with self.assertRaises(ValueError):
                self.state.complete_inventory(value)

    def test_inventory_cli_requests_unlimited_and_retains_all_nested_files(self):
        feature = {"feature_id": "feat_1", "files": [{"path": f"a{i}.rs", "role": "owned"} for i in range(800)]}
        result = subprocess.CompletedProcess([], 0, json.dumps({"results": [feature]}), "")
        with mock.patch.object(self.state.subprocess, "run", return_value=result) as run:
            value = self.state.collect_inventory("/project")
        self.assertEqual(len(value["results"][0]["files"]), 800)
        self.assertEqual(run.call_args.args[0], ["codesage", "features-list", "--json", "--limit", "0"])
        self.assertEqual(run.call_args.kwargs["cwd"], "/project")

    def test_inventory_preserves_unknown_feature_and_database_error_diagnostics(self):
        for diagnostic in ("no feature with id `feat_old` in this project", "unable to open database file"):
            error = subprocess.CalledProcessError(1, ["codesage"], stderr=diagnostic)
            with mock.patch.object(self.state.subprocess, "run", side_effect=error):
                with self.assertRaises(ValueError) as raised:
                    self.state.collect_inventory("/project", "feat_old")
            self.assertIn(diagnostic, str(raised.exception))
            self.assertIn("exit 1", str(raised.exception))

    def test_risk_coverage_rejects_missing_duplicate_invalid_and_truncated(self):
        row = {"file": "a.rs", "score": 0.2}
        for response in ([row], [row, row], [{**row, "score": float("nan")}],
                         {"files": [row], "_meta": {"truncated": True}}):
            with self.assertRaises(ValueError):
                self.state.complete_risk(["a.rs", "b.rs"], response)
        self.assertEqual(self.state.complete_risk(["a.rs"], [row]), {"files": [row]})

    def test_strict_plan_refuses_missing_entry_score(self):
        feature = {"feature_id": "feat_1", "entry_path": "a.rs", "files": [{"path": "a.rs", "role": "entry"}]}
        with self.assertRaisesRegex(ValueError, "missing risk"):
            self.state.plan_feature_review(feature, [], [], strict_risk=True)

    def report_fixture(self, project):
        folder = project / ".codesage" / "findings"
        folder.mkdir(parents=True)
        document = {"feature_id": "feat_a", "reviewed_at": "2026-01-02T00:00:00Z",
                    "trust_boundaries": ["network"], "ack_sweep": [{"finding_id": "fnd_old", "kind": "foreign", "reason": "not-emitted"}],
                    "findings": [{"finding_id": "fnd_a", "file": "a.rs", "line": 2,
                                  "severity": "high", "category": "bug", "status": "open",
                                  "title": "A | title", "summary": "Summary", "evidence": ["``` odd fence"],
                                  "suggested_fix": "Fix it", "acknowledgement": {"metric": "count", "value": 4}}]}
        (folder / "feat_a.json").write_text(json.dumps(document))
        return folder, document

    def test_report_cli_stdout_output_parity_and_no_clock_or_model(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            self.report_fixture(project)
            command = [str(SCRIPT), "report", directory]
            first = subprocess.run(command, capture_output=True, check=True).stdout
            second = subprocess.run(command, capture_output=True, check=True).stdout
            output = project / "report.md"
            written = subprocess.run([*command, "-o", str(output)], capture_output=True, check=True)
            self.assertEqual(first, second)
            self.assertEqual(first, output.read_bytes())
            self.assertIn(b"Findings: 1", written.stdout)
            self.assertIn(b"2026-01-02T00:00:00Z", first)
            self.assertIn(b"metadata unavailable", first)
            self.assertIn(b"current measurement unavailable", first)
            self.assertIn(b"persisted last review scope", first)
            self.assertIn(b"Live source acknowledgement diagnostics", first)
            self.assertIn(b"````\n``` odd fence\n````", first)

    def test_report_filters_leave_diagnostics_and_transfers_out_of_totals(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            folder, original = self.report_fixture(project)
            destination = {"feature_id": "feat_b", "findings": [{**original["findings"][0], "finding_id": "fnd_b",
                           "ack_transferred_from": {"feature_id": "feat_a", "finding_id": "fnd_a"}}]}
            (folder / "feat_b.json").write_text(json.dumps(destination))
            args = self.state.build_parser().parse_args(["report", directory, "--severity", "low", "--feature", "feat_a"])
            report, count, features = self.state.render_report(args)
            self.assertEqual((count, features), (0, 0))
            self.assertIn("Total findings in project: 1", report)
            self.assertIn("Transfer audit", report)
            self.assertIn("not-emitted", report)
            self.assertNotIn("Live source acknowledgement diagnostics", report)

    def test_report_refuses_symlink_findings_and_output(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            folder, _ = self.report_fixture(project)
            target = project / "outside.json"
            target.write_text("{}")
            (folder / "planted.json").symlink_to(target)
            args = self.state.build_parser().parse_args(["report", directory])
            with self.assertRaises(OSError):
                self.state.render_report(args)
            output = project / "output.md"
            output.symlink_to(target)
            with self.assertRaises(OSError):
                self.state.write_report(output, "overwrite")
            self.assertEqual(target.read_text(), "{}")

    def test_report_legacy_metadata_uses_complete_cli_once_or_frozen_inventory(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            self.report_fixture(project)
            inventory = {"results": [{"feature_id": "feat_a", "title": "Real feature", "kind": "library",
                                      "entry_path": "actual.rs", "files": []}]}
            args = self.state.build_parser().parse_args(["report", directory])
            with mock.patch.object(self.state, "collect_inventory", return_value=inventory) as collect:
                report, _, _ = self.state.render_report(args)
            collect.assert_called_once_with(project)
            self.assertIn("Real feature", report)
            self.assertIn("actual.rs", report)
            frozen = project / "inventory.json"
            frozen.write_text(json.dumps(inventory))
            args.features = str(frozen)
            with mock.patch.object(self.state, "collect_inventory", side_effect=AssertionError("live lookup")):
                repeated, _, _ = self.state.render_report(args)
            self.assertEqual(report, repeated)
            frozen.write_text(json.dumps({**inventory, "_meta": {"truncated": True}}))
            with self.assertRaisesRegex(ValueError, "truncated"):
                self.state.render_report(args)

    def test_report_density_filters_and_boundary_counts(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            folder, original = self.report_fixture(project)
            original.update({"title": "Feature", "kind": "library", "entry_path": "a.rs", "feature_files": []})
            original["findings"] = [{**original["findings"][0], "finding_id": f"fnd_{index}", "severity": "medium"} for index in range(9)]
            (folder / "feat_a.json").write_text(json.dumps(original))
            args = self.state.build_parser().parse_args(["report", directory, "--status", "open", "--severity", "medium", "--category", "bug"])
            report, count, features = self.state.render_report(args)
            self.assertEqual((count, features), (9, 1))
            self.assertIn("| Finding | File:line |", report)
            self.assertIn("network 1", report)
            self.assertIn("accepted count = 4", report)
            args.category = "security"
            report, count, _ = self.state.render_report(args)
            self.assertEqual(count, 0)
            self.assertIn("Total findings in project: 9", report)
            args.category = "not-a-category"
            with self.assertRaises(ValueError):
                self.state.render_report(args)

    def test_report_rejects_explicit_bad_inventory_without_legacy_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            folder, original = self.report_fixture(project)
            original.update({"title": "Feature", "kind": "library", "entry_path": "a.rs", "feature_files": []})
            (folder / "feat_a.json").write_text(json.dumps(original))
            inventory = project / "inventory.json"
            args = self.state.build_parser().parse_args(["report", directory, "--features", str(inventory)])
            for value in ("malformed json", json.dumps({"results": [], "_meta": {"truncated": True}})):
                inventory.write_text(value)
                with self.assertRaises(ValueError):
                    self.state.render_report(args)

    def test_report_rejects_invalid_findings_even_when_filters_exclude_them(self):
        with tempfile.TemporaryDirectory() as directory:
            project = Path(directory)
            folder, original = self.report_fixture(project)
            args = self.state.build_parser().parse_args(["report", directory, "--feature", "feat_other"])
            for field, value in (("status", "opne"), ("severity", "critical"), ("category", "typo"),
                                 ("line", "2"), ("file", None), ("title", None), ("evidence", [42])):
                invalid = json.loads(json.dumps(original))
                invalid["findings"][0][field] = value
                (folder / "feat_a.json").write_text(json.dumps(invalid))
                with self.subTest(field=field), self.assertRaises(ValueError):
                    self.state.render_report(args)


if __name__ == "__main__":
    unittest.main()
