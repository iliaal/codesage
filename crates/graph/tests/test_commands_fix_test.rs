use std::path::Path;
use std::process::Command;

use codesage_features::map_features;
use codesage_graph::{
    ReachabilityOptions, assess_risk, full_index, recommend_tests,
    recommend_tests_with_reachability,
};
use codesage_protocol::FeatureFileRole;
use codesage_storage::Database;

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

#[test]
fn test_commands_phpt_discovery_and_risk_use_real_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "ext/demo/demo.c", "int demo(void) { return 1; }\n");
    write(root, "ext/demo/demo.h", "int demo(void);\n");
    write(
        root,
        "ext/demo/tests/works.phpt",
        "--TEST--\nworks\n--FILE--\n<?php echo 1; ?>\n--EXPECT--\n1\n",
    );
    write(root, "ext/empty/empty.c", "int empty(void) { return 0; }\n");
    std::fs::create_dir(root.join(".codesage")).unwrap();
    let db = Database::open(&root.join(".codesage/index.db")).unwrap();
    full_index(root, &db, &[], false).unwrap();
    assert!(
        db.file_id_for_path("ext/demo/tests/works.phpt")
            .unwrap()
            .is_none()
    );
    for source in ["ext/demo/demo.c", "ext/demo/demo.h"] {
        let recs = recommend_tests(&db, &[source.into()]).unwrap();
        assert_eq!(recs.primary, ["ext/demo/tests/works.phpt"]);
        assert!(!assess_risk(&db, source).unwrap().test_gap);
    }
    assert!(assess_risk(&db, "ext/empty/empty.c").unwrap().test_gap);
    let opts = ReachabilityOptions {
        project_root: Some(root.to_path_buf()),
        ..Default::default()
    };
    let recs = recommend_tests_with_reachability(&db, &["ext/demo/demo.c".into()], &opts).unwrap();
    assert!(
        recs.commands
            .iter()
            .any(|c| c.command == "php run-tests.php ext/demo/tests"),
        "{:?}",
        recs.commands
    );
}

#[test]
fn test_commands_directory_binary_runs_root_and_nested_tests() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"binary-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(
        root,
        "src/lib.rs",
        "#[cfg(test)]\nmod tests {\n#[test]\nfn library_marker() {}\n}\n",
    );
    write(
        root,
        "src/bin/flat.rs",
        "fn main() {}\n#[cfg(test)]\nmod tests {\n#[test]\nfn flat_marker() {}\n}\n",
    );
    write(
        root,
        "src/bin/tool/main.rs",
        "mod nested;\nfn main() {}\n#[cfg(test)]\nmod tests {\n#[test]\nfn root_marker() {}\n}\n",
    );
    write(
        root,
        "src/bin/tool/nested.rs",
        "#[cfg(test)]\nmod tests {\n#[test]\nfn nested_marker() {}\n}\n",
    );
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    let opts = ReachabilityOptions {
        project_root: Some(root.to_path_buf()),
        ..Default::default()
    };
    for (source, marker) in [
        ("src/bin/tool/main.rs", "test tests::root_marker ... ok"),
        (
            "src/bin/tool/nested.rs",
            "test nested::tests::nested_marker ... ok",
        ),
        ("src/bin/flat.rs", "test tests::flat_marker ... ok"),
        ("src/lib.rs", "test tests::library_marker ... ok"),
    ] {
        let recs = recommend_tests_with_reachability(&db, &[source.into()], &opts).unwrap();
        let command = recs
            .commands
            .iter()
            .find(|c| c.source == "inline")
            .expect("inline command");
        let output = Command::new("sh")
            .args(["-c", &command.command])
            .current_dir(root)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env("CARGO_TERM_COLOR", "never")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}: {}",
            command.command,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(marker),
            "{} did not run {marker}: {}",
            command.command,
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn test_commands_go_package_tests_persist_as_tests() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "go.mod", "module example.com/demo\n");
    write(root, "pkg/demo/demo.go", "package demo\nfunc Demo() {}\n");
    write(
        root,
        "pkg/demo/demo_test.go",
        "package demo\nimport \"testing\"\nfunc TestDemo(t *testing.T) { Demo() }\n",
    );
    let db = Database::open_in_memory().unwrap();
    map_features(root, &db, &[]).unwrap();
    let features = db.features_for_file("pkg/demo/demo.go").unwrap();
    let feature = features
        .iter()
        .find(|f| f.entry_path == "pkg/demo/demo.go")
        .expect("Go package feature");
    let stored = db.load_feature(&feature.feature_id).unwrap().unwrap();
    let roles: Vec<_> = stored
        .files
        .iter()
        .filter(|f| f.path == "pkg/demo/demo_test.go")
        .map(|f| f.role)
        .collect();
    assert_eq!(roles, [FeatureFileRole::Test]);
}

#[test]
fn test_commands_rust_workspace_members_do_not_inherit_root_tests() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"root-package\"\nversion = \"0.1.0\"\n[workspace]\nmembers = [\"crates/covered\", \"crates/empty\"]\n",
    );
    write(root, "src/lib.rs", "pub fn root() {}\n");
    write(root, "tests/root_suite.rs", "#[test]\nfn root_suite() {}\n");
    for member in ["covered", "empty"] {
        write(
            root,
            &format!("crates/{member}/Cargo.toml"),
            &format!("[package]\nname = \"{member}\"\nversion = \"0.1.0\"\n"),
        );
        write(
            root,
            &format!("crates/{member}/src/lib.rs"),
            "pub fn member() {}\n",
        );
    }
    write(
        root,
        "crates/covered/tests/member_suite.rs",
        "#[test]\nfn member_suite() {}\n",
    );
    let db = Database::open_in_memory().unwrap();
    map_features(root, &db, &[]).unwrap();
    for (entry, expected) in [
        ("src/lib.rs", vec!["tests/root_suite.rs"]),
        (
            "crates/covered/src/lib.rs",
            vec!["crates/covered/tests/member_suite.rs"],
        ),
        ("crates/empty/src/lib.rs", vec![]),
    ] {
        let features = db.features_for_file(entry).unwrap();
        let feature = features
            .iter()
            .find(|f| f.entry_path == entry)
            .expect("crate feature");
        let tests: Vec<_> = feature
            .files
            .iter()
            .filter(|f| f.role == FeatureFileRole::Test)
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(tests, expected, "{entry}");
    }
}
