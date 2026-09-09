use codesage_storage::Database;
use std::process::Command;

fn run(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// An unscored row and a scored row of similar score must not read alike:
/// history-derived terms were never measured for the former.
#[test]
fn risk_batch_and_diff_mark_unscored_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".codesage")).unwrap();
    let db = Database::open(&dir.path().join(".codesage/index.db")).unwrap();
    db.upsert_git_file("scored.rs", 0.0, 0, 1, None).unwrap();
    db.upsert_git_file("hot.rs", 10.0, 5, 20, None).unwrap();
    drop(db);

    let text = run(dir.path(), &["risk-batch", "--", "scored.rs", "fresh.rs"]);
    let line = |name: &str| {
        text.lines()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("{name} missing: {text}"))
            .to_string()
    };
    let scored = line("scored.rs");
    let fresh = line("fresh.rs");
    let (score, rest) = scored.split_at(7);
    assert!(score.trim().parse::<f64>().is_ok(), "{text}");
    assert_eq!(rest, "  scored.rs", "{text}");
    assert_eq!(fresh, "   0.00  fresh.rs  (unscored)", "{text}");

    let text = run(dir.path(), &["risk-diff", "--", "scored.rs", "fresh.rs"]);
    assert!(text.contains("  unscored (1):\n    - fresh.rs\n"), "{text}");
}
