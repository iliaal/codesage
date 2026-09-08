use codesage_storage::{Database, db::CoChangeWrite};
use std::process::Command;

#[test]
fn disabled_recurrence_legend_qualifies_default_ranking() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".codesage")).unwrap();
    let db = Database::open(&dir.path().join(".codesage/index.db")).unwrap();
    db.upsert_git_file("target.rs", 1.0, 0, 10, None).unwrap();
    for (file, weight, first) in [("burst.rs", 6.0, 4_000_000), ("recurring.rs", 4.0, 0)] {
        db.upsert_git_co_change_full(
            "target.rs",
            file,
            &CoChangeWrite {
                weight,
                count: 4,
                window_mask: 1,
                first_observed_at: Some(first),
                last_observed_at: Some(4_000_000),
            },
        )
        .unwrap();
    }
    drop(db);
    for disabled in ["0", "false"] {
        for command in ["coupling", "risk"] {
            let out = Command::new(env!("CARGO_BIN_EXE_codesage"))
                .current_dir(dir.path())
                .env("CODESAGE_COUPLING_RECURRENCE", disabled)
                .args([command, "target.rs"])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let text = String::from_utf8(out.stdout).unwrap();
            assert!(
                text.find("burst.rs").unwrap() < text.find("recurring.rs").unwrap(),
                "{text}"
            );
            assert!(
                text.contains("half weight by default"),
                "{command} {disabled}: {text}"
            );
            assert!(
                text.contains("CODESAGE_COUPLING_RECURRENCE=0 or false"),
                "{text}"
            );
        }
    }
}
