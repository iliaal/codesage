use codesage_parser::discover::discover_files_with_excludes;
use codesage_protocol::Language;

#[test]
fn discovers_only_supported_languages() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("controller.php"), "<?php\nclass Foo {}\n").unwrap();
    std::fs::write(root.join("main.py"), "def main(): pass\n").unwrap();
    std::fs::write(
        root.join("util.c"),
        "int add(int a, int b) { return a + b; }\n",
    )
    .unwrap();
    std::fs::write(root.join("header.h"), "#pragma once\nint add(int, int);\n").unwrap();
    std::fs::write(root.join("readme.txt"), "ignore me\n").unwrap();
    std::fs::write(root.join("Makefile"), "all:\n\techo hi\n").unwrap();

    let files = discover_files_with_excludes(root, &[]).unwrap();

    assert_eq!(files.len(), 4);

    let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(names.contains(&"controller.php"));
    assert!(names.contains(&"main.py"));
    assert!(names.contains(&"util.c"));
    assert!(names.contains(&"header.h"));

    assert_eq!(
        files
            .iter()
            .find(|f| f.path == "controller.php")
            .unwrap()
            .language,
        Language::Php
    );
    assert_eq!(
        files.iter().find(|f| f.path == "main.py").unwrap().language,
        Language::Python
    );
    assert_eq!(
        files.iter().find(|f| f.path == "util.c").unwrap().language,
        Language::C
    );
    assert_eq!(
        files
            .iter()
            .find(|f| f.path == "header.h")
            .unwrap()
            .language,
        Language::C
    );

    for f in &files {
        assert!(!f.content_hash.is_empty());
        assert_eq!(f.content_hash.len(), 64); // SHA-256 hex
    }
}

#[test]
fn respects_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(root.join(".gitignore"), "vendor/\n").unwrap();
    std::fs::create_dir(root.join("vendor")).unwrap();
    std::fs::write(root.join("vendor/dep.php"), "<?php\n").unwrap();
    std::fs::write(root.join("app.php"), "<?php\nclass App {}\n").unwrap();

    let files = discover_files_with_excludes(root, &[]).unwrap();

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "app.php");
}

#[test]
fn hash_changes_on_content_change() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("test.py"), "v1\n").unwrap();
    let files1 = discover_files_with_excludes(root, &[]).unwrap();
    let hash1 = files1[0].content_hash.clone();

    std::fs::write(root.join("test.py"), "v2\n").unwrap();
    let files2 = discover_files_with_excludes(root, &[]).unwrap();
    let hash2 = files2[0].content_hash.clone();

    assert_ne!(hash1, hash2);
}

#[test]
fn sorted_by_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("z.py"), "pass\n").unwrap();
    std::fs::write(root.join("a.py"), "pass\n").unwrap();
    std::fs::write(root.join("m.c"), "int x;\n").unwrap();

    let files = discover_files_with_excludes(root, &[]).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["a.py", "m.c", "z.py"]);
}
