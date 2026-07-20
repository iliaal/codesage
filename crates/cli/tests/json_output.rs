//! CLI `--json` output-shape regression tests.
//!
//! The MCP server wraps every list-shaped tool result in a `{"results": [...]}`
//! envelope (protocol `*Results` structs). The CLI used to emit bare arrays for
//! `find-symbol` / `find-references` / `search` / `similar`, so the same data
//! had two shapes depending on the entrypoint. These tests pin the envelope on
//! the CLI side. (`search` is exercised only via code symmetry — it needs a
//! model + embedder, which integration tests can't assume.)

use std::path::Path;
use std::process::Command;

fn init_project(dir: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .arg("init")
        .current_dir(dir)
        .output()
        .expect("spawn codesage init");
    assert!(
        out.status.success(),
        "codesage init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_results_envelope(args: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    init_project(dir.path());

    let out = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(args)
        .arg("--json")
        .current_dir(dir.path())
        .output()
        .expect("spawn codesage");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{args:?} failed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("{args:?} emitted invalid JSON ({e}):\n{stdout}"));
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("{args:?} must emit an object envelope, got:\n{stdout}"));
    assert!(
        obj.get("results").is_some_and(serde_json::Value::is_array),
        "{args:?} must emit {{\"results\": [...]}}, got:\n{stdout}"
    );
}

#[test]
fn find_symbol_json_uses_results_envelope() {
    assert_results_envelope(&["find-symbol", "no_such_symbol"]);
}

#[test]
fn find_symbol_rejects_unknown_kind() {
    let dir = tempfile::tempdir().unwrap();
    init_project(dir.path());

    let out = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(["find-symbol", "no_such_symbol", "--kind", "bogus"])
        .current_dir(dir.path())
        .output()
        .expect("spawn codesage");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "unknown kind must fail");
    assert!(
        stderr.contains("unknown symbol kind: bogus"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn find_references_json_uses_results_envelope() {
    assert_results_envelope(&["find-references", "no_such_symbol"]);
}

#[test]
fn status_json_covers_prose_fields() {
    let dir = tempfile::tempdir().unwrap();
    init_project(dir.path());

    let out = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .args(["status", "--json"])
        .current_dir(dir.path())
        .output()
        .expect("spawn codesage status");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "status --json failed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("status --json emitted invalid JSON ({e}):\n{stdout}"));
    for key in [
        "project_root",
        "database",
        "files",
        "symbols",
        "references",
        "chunks",
        "drift",
        "drift_summary",
        "semantic",
    ] {
        assert!(
            v.get(key).is_some(),
            "status --json must carry `{key}`, got:\n{stdout}"
        );
    }
    assert!(
        v["files"].is_u64(),
        "counts serialize as numbers:\n{stdout}"
    );
    assert!(
        v["drift"].get("kind").is_some(),
        "drift must be the structured DriftReport:\n{stdout}"
    );
    // Fresh `codesage init` in a non-git tempdir: no chunk table yet.
    assert_eq!(v["semantic"]["state"], "missing", "got:\n{stdout}");
    assert!(
        v["semantic"]["model"].is_string(),
        "semantic.model must name the configured model:\n{stdout}"
    );
}

#[test]
fn status_prose_output_is_unchanged_by_default() {
    let dir = tempfile::tempdir().unwrap();
    init_project(dir.path());

    let out = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .arg("status")
        .current_dir(dir.path())
        .output()
        .expect("spawn codesage status");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    for prefix in [
        "Project root: ",
        "Database: ",
        "Files:      ",
        "Symbols:    ",
        "References: ",
        "Chunks:     ",
        "Drift:      ",
        "Semantic:   ",
    ] {
        assert!(
            stdout.lines().any(|l| l.starts_with(prefix)),
            "prose status must keep the `{prefix}` line, got:\n{stdout}"
        );
    }
}

#[test]
fn similar_json_uses_results_envelope() {
    assert_results_envelope(&["similar", "no_such_symbol"]);
}
