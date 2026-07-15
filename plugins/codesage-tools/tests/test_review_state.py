import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
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


class IdentityTests(unittest.TestCase):
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

    def test_atomic_write_leaves_complete_json_without_temporary_file(self):
        review_state = load_review_state()
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "state.json"
            review_state.atomic_write_json(destination, {"value": 42})

            loaded = json.loads(destination.read_text())
            leftovers = list(Path(directory).glob(".state.json.*.tmp"))

        self.assertEqual(loaded, {"value": 42})
        self.assertEqual(leftovers, [])


class ProtocolContractTests(unittest.TestCase):
    def read(self, relative_path):
        return (PLUGIN_ROOT / relative_path).read_text()

    def test_review_command_routes_state_transitions_through_helper(self):
        command = self.read("commands/codesage-review.md")

        for subcommand in ("slim-priors", "feature-states", "combine", "validate", "merge"):
            self.assertIn(subcommand, command)
        self.assertIn("--focus all|product", command)
        self.assertIn("one `codesage-finding-verifier`", command)
        self.assertNotIn("stat -c %Y", command)

    def test_revalidate_never_marks_open_fixed_from_omission(self):
        command = self.read("commands/codesage-revalidate.md")

        self.assertIn("needs-confirmation", command)
        self.assertIn("--mode revalidate", command)
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


if __name__ == "__main__":
    unittest.main()
