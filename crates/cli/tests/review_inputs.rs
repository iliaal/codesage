use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(root: &Path, path: &str, source: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn run(root: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .current_dir(root)
        .env("CODESAGE_WATCH", "0")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn json(root: &Path, args: &[&str]) -> Value {
    serde_json::from_str(&run(root, args)).unwrap()
}

#[test]
fn review_inputs_reports_equal_the_three_separate_commands() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-b", "main"]);
    write(
        root,
        "app/pricing.py",
        "from app import rates\n\n\ndef total(amount):\n    return amount * rates.current()\n",
    );
    write(root, "app/rates.py", "def current():\n    return 2\n");
    write(root, "app/__init__.py", "");
    write(
        root,
        "tests/test_pricing.py",
        "from app.pricing import total\n\n\ndef test_total():\n    assert total(2) == 4\n",
    );
    write(
        root,
        "src/lib.rs",
        "pub fn double(value: u32) -> u32 {\n    value * 2\n}\n",
    );
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "add pricing"]);
    // Three commits touching both files give them a co-change record, whose
    // probabilities are the reports' only single-precision numbers.
    for rate in 3..6 {
        write(
            root,
            "app/rates.py",
            &format!("def current():\n    return {rate}\n"),
        );
        write(
            root,
            "app/pricing.py",
            &format!(
                "from app import rates\n\n\ndef total(amount):\n    return amount * rates.current() + {rate}\n"
            ),
        );
        git(root, &["commit", "-am", "fix: correct the rate"]);
    }
    for bonus in 0..2 {
        write(
            root,
            "app/pricing.py",
            &format!(
                "from app import rates\n\n\ndef total(amount):\n    return amount * rates.current() - {bonus}\n"
            ),
        );
        git(root, &["commit", "-am", "adjust the total"]);
    }
    run(root, &["init"]);
    run(root, &["index", "--no-semantic"]);
    run(root, &["git-index"]);

    // An indexed Python importer, its target, a Rust file, and a path the index
    // does not hold: scored, structural-only, and unscored rows all appear.
    let files = ["app/pricing.py", "app/rates.py", "src/lib.rs", "missing.py"];
    let with = |command: &[&str]| -> Vec<String> {
        command
            .iter()
            .chain(["--"].iter())
            .chain(files.iter())
            .map(|arg| arg.to_string())
            .collect()
    };
    let args = |command: &[&str]| -> String {
        let owned = with(command);
        let borrowed: Vec<&str> = owned.iter().map(String::as_str).collect();
        run(root, &borrowed)
    };

    let combined: Value = serde_json::from_str(&args(&["review-inputs", "--json"])).unwrap();
    for (key, command) in [
        ("risk_diff", "risk-diff"),
        ("rehearsal", "rehearse"),
        ("risk_batch", "risk-batch"),
    ] {
        assert_eq!(
            combined[key]["text"].as_str().unwrap(),
            args(&[command]),
            "{command} text"
        );
        let report: Value = serde_json::from_str(&args(&[command, "--json"])).unwrap();
        assert_eq!(combined[key]["report"], report, "{command} report");
    }
    // The fixture must exercise real scoring, or equal empty reports prove nothing.
    let rows = combined["risk_batch"]["report"]["files"]
        .as_array()
        .unwrap();
    assert_eq!(rows.len(), files.len());
    assert!(rows.iter().any(|row| row["found"] == true), "{rows:?}");
    // A probability whose shortest single-precision spelling differs from its
    // double-precision value: printing it widened would not match `risk-batch`.
    assert!(
        rows.iter()
            .flat_map(|row| row["top_coupled"].as_array().into_iter().flatten())
            .filter_map(|coupled| coupled["confidence"].as_f64())
            .any(|p| f64::from(p as f32) != p),
        "{rows:?}"
    );
    assert_eq!(
        json(root, &["risk", "app/rates.py", "--json"])["file"],
        "app/rates.py"
    );

    let human = args(&["review-inputs"]);
    assert_eq!(
        human,
        format!(
            "{}\n{}\n{}",
            args(&["risk-diff"]),
            args(&["rehearse"]),
            args(&["risk-batch"])
        )
    );
}
