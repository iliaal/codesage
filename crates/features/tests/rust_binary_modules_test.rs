use std::path::Path;

use codesage_features::map_features;
use codesage_storage::Database;
use tempfile::tempdir;

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
}

fn compile(root: &Path) {
    let output = std::process::Command::new("rustc")
        .arg(root.join("src/main.rs"))
        .arg("--out-dir")
        .arg(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn rust_path_literal_forms_select_actual_module_not_default_decoy() {
    for literal in [
        r#"r"actual.rs""#,
        r##"r#"actual.rs"#"##,
        r###"r##"actual.rs"##"###,
        r#""\x61ctual.rs""#,
        r#""\u{61}ctual.rs""#,
    ] {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "Cargo.toml", "[package]\nname = \"app\"\n");
        write(
            root,
            "src/main.rs",
            &format!("#[path = {literal}] mod worker; fn main() {{ worker::run(); }}"),
        );
        write(root, "src/actual.rs", "pub fn run() {}");
        write(root, "src/worker.rs", "compile_error!(\"wrong module\");");
        compile(root);
        let db = Database::open_in_memory().unwrap();
        map_features(root, &db, &[]).unwrap();
        assert_eq!(
            db.features_for_file("src/actual.rs").unwrap().len(),
            1,
            "{literal}"
        );
        assert!(
            db.features_for_file("src/worker.rs").unwrap().is_empty(),
            "{literal}"
        );
    }
}

#[test]
fn same_file_in_distinct_module_contexts_retains_both_descendants() {
    for declarations in [
        "mod worker; #[path = \"worker.rs\"] mod alias;",
        "#[path = \"worker.rs\"] mod alias; mod worker;",
    ] {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "Cargo.toml", "[package]\nname = \"app\"\n");
        write(
            root,
            "src/main.rs",
            &format!("{declarations} fn main() {{ worker::run(); alias::run(); }}"),
        );
        write(
            root,
            "src/worker.rs",
            "mod inner; pub fn run() { inner::run(); }",
        );
        write(root, "src/worker/inner.rs", "pub fn run() {}");
        write(root, "src/inner.rs", "pub fn run() {}");
        compile(root);
        let db = Database::open_in_memory().unwrap();
        map_features(root, &db, &[]).unwrap();
        for file in ["src/worker.rs", "src/worker/inner.rs", "src/inner.rs"] {
            assert_eq!(
                db.features_for_file(file).unwrap().len(),
                1,
                "{declarations}: {file}"
            );
        }
    }
}

#[test]
fn worker_change_selects_its_binary_and_shared_modules_select_both() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    );
    write(
        root,
        "src/main.rs",
        "mod worker; mod shared; fn main() { worker::run(); }",
    );
    write(root, "src/worker.rs", "pub fn run() {}");
    write(root, "src/shared.rs", "pub fn shared() {}");
    write(
        root,
        "src/bin/aux.rs",
        "#[path = \"../shared.rs\"] mod shared; mod auxiliary; fn main() {}",
    );
    write(root, "src/bin/auxiliary.rs", "pub fn aux() {}");
    write(root, "src/unrelated.rs", "pub fn unrelated() {}");
    let db = Database::open_in_memory().unwrap();
    map_features(root, &db, &[]).unwrap();
    let entries = |file: &str| {
        let mut paths: Vec<_> = db
            .features_for_file(file)
            .unwrap()
            .into_iter()
            .map(|feature| feature.entry_path)
            .collect();
        paths.sort();
        paths
    };
    assert_eq!(entries("src/worker.rs"), ["src/main.rs"]);
    assert_eq!(entries("src/shared.rs"), ["src/bin/aux.rs", "src/main.rs"]);
    assert!(!entries("src/bin/auxiliary.rs").contains(&"src/main.rs".to_string()));
    assert!(entries("src/unrelated.rs").is_empty());
}

#[test]
fn nested_inline_modules_paths_and_exclusions_are_respected() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "Cargo.toml", "[package]\nname = \"app\"\n");
    write(
        root,
        "src/main.rs",
        "mod outer { mod worker; } mod tree; mod excluded; /* mod decoy; */ fn main() {}",
    );
    write(root, "src/outer/worker.rs", "pub fn run() {}");
    write(
        root,
        "src/tree/mod.rs",
        "#[path = \"branch.rs\"] mod inner;",
    );
    write(root, "src/tree/branch.rs", "mod leaf; pub fn run() {}");
    write(root, "src/tree/leaf.rs", "pub fn run() {}");
    write(root, "src/excluded.rs", "mod deeper;");
    write(root, "src/excluded/deeper.rs", "pub fn run() {}");
    write(root, "src/decoy.rs", "pub fn run() {}");
    let db = Database::open_in_memory().unwrap();
    map_features(root, &db, &["src/excluded.rs".into()]).unwrap();
    for file in [
        "src/outer/worker.rs",
        "src/tree/mod.rs",
        "src/tree/branch.rs",
        "src/tree/leaf.rs",
    ] {
        assert_eq!(db.features_for_file(file).unwrap().len(), 1, "{file}");
    }
    for file in ["src/excluded.rs", "src/excluded/deeper.rs", "src/decoy.rs"] {
        assert!(db.features_for_file(file).unwrap().is_empty(), "{file}");
    }
}

#[test]
fn module_cycles_and_paths_outside_project_do_not_expand_ownership() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("project");
    write(&root, "Cargo.toml", "[package]\nname = \"app\"\n");
    write(
        &root,
        "src/main.rs",
        "#[path = \"../../outside.rs\"] mod outside; mod worker; fn main() {}",
    );
    write(&root, "src/worker.rs", "#[path = \"main.rs\"] mod cycle;");
    write(dir.path(), "outside.rs", "pub fn outside() {}");
    let db = Database::open_in_memory().unwrap();
    map_features(&root, &db, &[]).unwrap();
    let binary = db.features_for_file("src/worker.rs").unwrap();
    assert_eq!(binary.len(), 1);
    assert_eq!(binary[0].entry_path, "src/main.rs");
    assert!(db.features_for_file("../outside.rs").unwrap().is_empty());
}
