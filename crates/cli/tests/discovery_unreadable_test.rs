#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn run(root: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_codesage"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn has_symbol(root: &Path, name: &str) -> bool {
    let output = run(root, &["find-symbol", name, "--json"]);
    let symbols: serde_json::Value = serde_json::from_str(&output).unwrap();
    !symbols["results"].as_array().unwrap().is_empty()
}

#[test]
fn discovery_read_failure_preserves_rows_and_reports_failure() {
    for full in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let source = root.join("api.c");
        fs::write(&source, "int durable(void) { return 1; }\n").unwrap();
        fs::write(root.join("deleted.c"), "int deleted(void) { return 2; }\n").unwrap();
        fs::write(
            root.join("excluded.c"),
            "int excluded(void) { return 3; }\n",
        )
        .unwrap();
        run(root, &["init"]);
        run(root, &["index", "--no-semantic", "--no-features"]);
        assert!(has_symbol(root, "durable"));
        assert!(has_symbol(root, "deleted"));
        assert!(has_symbol(root, "excluded"));

        fs::set_permissions(&source, fs::Permissions::from_mode(0o0)).unwrap();
        if fs::read(&source).is_ok() {
            fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
            eprintln!("unreadability requires a user without permission bypass");
            continue;
        }
        fs::remove_file(root.join("deleted.c")).unwrap();
        fs::write(
            root.join(".codesage/config.toml"),
            "[index]\nexclude_patterns = [\"excluded.c\"]\n",
        )
        .unwrap();
        let mut args = vec!["index", "--no-semantic", "--no-features"];
        if full {
            args.push("--full");
        }
        let output = run(root, &args);
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(has_symbol(root, "durable"), "{output}");
        assert!(output.contains("1 failed, 2 removed"), "{output}");
        assert!(
            output.contains("failed (retried next pass): api.c"),
            "{output}"
        );
        assert!(!has_symbol(root, "deleted"));
        assert!(!has_symbol(root, "excluded"));
        fs::write(&source, "int recovered(void) { return 4; }\n").unwrap();
        run(root, &["index", "--no-semantic", "--no-features"]);
        assert!(!has_symbol(root, "durable"));
        assert!(has_symbol(root, "recovered"));
    }
}
