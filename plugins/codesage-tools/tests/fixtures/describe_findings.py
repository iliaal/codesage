import importlib.machinery
import importlib.util
from pathlib import Path
import sys


def save_findings(project):
    script = Path(__file__).parents[2] / "bin" / "codesage-review-state"
    loader = importlib.machinery.SourceFileLoader("describe_review_state", str(script))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    state = importlib.util.module_from_spec(spec)
    loader.exec_module(state)
    feature = {
        "feature_id": "feat_1111111111111111",
        "title": "Fixture",
        "kind": "library",
        "entry_path": "src/lib.rs",
        "files": [
            {"path": "src/lib.rs", "role": "entry"},
            {"path": "src/helper.rs", "role": "owned"},
        ],
    }
    findings = [
        {
            "finding_id": finding_id,
            "file": file,
            "line": line,
            "severity": severity,
            "category": "bug",
            "title": title,
            "summary": "Saved review summary stays out of the describe card.",
            "evidence": ["pub fn leaf() { target(); }"],
            "suggested_fix": "Inspect the recorded review before editing.",
        }
        for finding_id, file, line, severity, title in [
            ("fnd_11111111", "src/helper.rs", 3, "low", "Helper low finding"),
            ("fnd_22222222", "src/lib.rs", 1, "high", "Library finding"),
            ("fnd_33333333", "src/helper.rs", 2, "high", "Helper high finding"),
            ("fnd_44444444", "src/helper.rs", 2, "high", "Fixed finding"),
            ("fnd_55555555", "src/helper.rs", 2, "high", "Transferred finding"),
        ]
    ]
    validated = {
        "feature_id": feature["feature_id"],
        "findings": findings,
        "new_finding_ids": [finding["finding_id"] for finding in findings],
        "recurring_finding_ids": [],
    }
    verdicts = {
        "verdicts": [
            {"finding_id": finding["finding_id"], "verdict": "confirmed"}
            for finding in findings
        ]
    }
    document, _ = state.merge_document(
        project, feature, {}, validated, verdicts, "describe-fixture",
        "2026-10-03T00:00:00Z", "review",
    )
    document, _ = state.merge_document(
        project, feature, document,
        {
            "feature_id": feature["feature_id"],
            "findings": [],
            "superseded_priors": {
                "fnd_55555555": [{
                    "feature_id": "feat_2222222222222222",
                    "finding_id": "fnd_66666666",
                }],
            },
        },
        {}, "describe-transfer-fixture", "2026-10-03T00:01:00Z", "review",
    )
    destination = project / ".codesage" / "findings" / f"{feature['feature_id']}.json"
    state.atomic_write_json(destination, document)
    state.triage(project, "fnd_44444444", "fixed")


if __name__ == "__main__":
    save_findings(Path(sys.argv[1]))
