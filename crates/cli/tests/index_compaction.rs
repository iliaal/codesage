use std::path::Path;
use std::process::Command;

fn write(root: &Path, path: &str, source: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn index(root: &Path, compact: Option<&str>) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codesage"));
    command
        .current_dir(root)
        .env("CODESAGE_WATCH", "0")
        .env_remove("CODESAGE_INDEX_COMPACT")
        .args(["index", "--no-semantic"]);
    if let Some(value) = compact {
        command.env("CODESAGE_INDEX_COMPACT", value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn module(calls: usize) -> String {
    let body: String = (0..calls)
        .map(|n| format!("    helper_{n}(value);\n"))
        .collect();
    format!("pub fn run(value: u32) {{\n{body}}}\n")
}

fn record(state: &Path) -> (u64, u64) {
    let text = std::fs::read_to_string(state).unwrap();
    let fields: Vec<u64> = text
        .split_whitespace()
        .map(|field| field.parse().unwrap())
        .collect();
    assert_eq!(fields.len(), 2, "{text}");
    (fields[0], fields[1])
}

#[test]
fn index_compacts_once_then_only_after_enough_references_are_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let init = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .current_dir(root)
        .arg("init")
        .output()
        .unwrap();
    assert!(init.status.success());
    for name in ["a", "b", "c", "d", "e"] {
        write(root, &format!("src/{name}.rs"), &module(4));
    }
    let state = root.join(".codesage/compaction.state");

    // Disabled: no compaction and no record.
    assert!(!index(root, Some("0")).contains("Compact"));
    assert!(!state.exists());

    // No record yet: compact once and start counting from zero.
    let first = index(root, None);
    assert!(
        first.contains("Compacted:") && first.contains("no compaction recorded"),
        "{first}"
    );
    let (rewritten, rows) = record(&state);
    assert_eq!(rewritten, 0);
    assert!(rows >= 20, "{rows}");

    // Nothing rewritten since: left alone.
    assert!(!index(root, None).contains("Compact"));
    assert_eq!(record(&state), (0, rows));

    // One file loses a reference. Its remaining rows are rewritten, which is
    // a small share of the index, and the highest rowid going down must not
    // read as a replaced index.
    write(root, "src/e.rs", &module(3));
    assert!(!index(root, None).contains("Compact"));
    let (rewritten, recorded_rows) = record(&state);
    assert!(
        rewritten > 0 && rewritten < rows / 3,
        "{rewritten} of {rows}"
    );
    assert_eq!(recorded_rows, rows);

    // Rewriting most of the index crosses the threshold.
    for name in ["a", "b", "c"] {
        write(root, &format!("src/{name}.rs"), &module(5));
    }
    let again = index(root, None);
    assert!(
        again.contains("Compacted:") && again.contains("written since the last compaction"),
        "{again}"
    );
    assert_eq!(record(&state).0, 0);

    // The compacted index still answers structural queries.
    let found = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .current_dir(root)
        .args(["find-references", "helper_4", "--json"])
        .output()
        .unwrap();
    assert!(found.status.success());
    assert!(String::from_utf8_lossy(&found.stdout).contains("src/a.rs"));

    // A state path that cannot hold a record turns compaction off instead of
    // compacting on every pass.
    std::fs::remove_file(&state).unwrap();
    std::fs::create_dir(&state).unwrap();
    write(root, "src/d.rs", &module(6));
    assert!(!index(root, None).contains("Compact"));
    assert!(!index(root, None).contains("Compact"));
    assert!(state.is_dir());
}
