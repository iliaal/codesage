use codesage_parser::discover::{
    MAX_INDEXABLE_FILE_BYTES, WatchFilter, discover_files_with_excludes,
};
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
fn h_files_default_to_c_when_no_cpp_present() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("util.c"), "int x;\n").unwrap();
    std::fs::write(root.join("util.h"), "int x;\n").unwrap();

    let files = discover_files_with_excludes(root, &[]).unwrap();
    let h = files.iter().find(|f| f.path == "util.h").unwrap();
    assert_eq!(h.language, Language::C);
}

#[test]
fn h_files_route_to_cpp_when_project_has_cpp() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("util.h"), "int x;\n").unwrap();
    std::fs::write(root.join("main.cpp"), "int main(){}\n").unwrap();

    let files = discover_files_with_excludes(root, &[]).unwrap();
    let h = files.iter().find(|f| f.path == "util.h").unwrap();
    assert_eq!(h.language, Language::Cpp);
    let cpp = files.iter().find(|f| f.path == "main.cpp").unwrap();
    assert_eq!(cpp.language, Language::Cpp);
}

#[test]
fn c_files_stay_c_even_in_cpp_project() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("legacy.c"), "int x;\n").unwrap();
    std::fs::write(root.join("main.cpp"), "int main(){}\n").unwrap();

    let files = discover_files_with_excludes(root, &[]).unwrap();
    let c = files.iter().find(|f| f.path == "legacy.c").unwrap();
    assert_eq!(c.language, Language::C);
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
fn watch_filter_respects_nested_gitignore_like_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src/generated")).unwrap();
    std::fs::write(root.join("src/.gitignore"), "generated/\n").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(root.join("src/generated/out.rs"), "fn generated() {}\n").unwrap();

    let files = discover_files_with_excludes(root, &[]).unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["src/main.rs"]);

    let filter = WatchFilter::new(root, &[]).unwrap();
    assert!(!filter.is_ignored(&root.join("src/main.rs"), false));
    assert!(filter.is_ignored(&root.join("src/generated/out.rs"), false));
}

#[test]
fn watch_filter_anchors_nested_patterns_and_preserves_directory_pruning() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    let cases = [
        ("app.py", true),
        ("gen/root.rs", true),
        ("sub/app.py", false),
        ("sub/gen/generated.rs", false),
        ("sub/deep/gen/kept.rs", true),
        ("sub/deep/hidden.py", false),
        ("sub/deep/keep.py", true),
        ("sub/blocked/reopened.rs", false),
        ("sibling/app.py", true),
        ("sub/.visible/kept.rs", true),
        ("sub/.hidden/hidden.rs", false),
    ];
    for (path, _) in cases {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "fn item() {}\n").unwrap();
    }
    std::fs::write(
        root.join("sub/.gitignore"),
        "/gen\n*.py\nblocked/\n!.visible/\n",
    )
    .unwrap();
    std::fs::write(root.join("sub/deep/.gitignore"), "!keep.py\n").unwrap();
    std::fs::write(root.join("sub/blocked/.gitignore"), "!reopened.rs\n").unwrap();
    let files = discover_files_with_excludes(root, &[]).unwrap();
    let filter = WatchFilter::new(root, &[]).unwrap();
    for (path, admitted) in cases {
        let walked = files.iter().any(|file| file.path == path);
        assert_eq!(walked, admitted, "walker: {path}");
        assert_eq!(
            !filter.is_ignored(&root.join(path), false),
            walked,
            "{path}"
        );
    }
    assert!(!filter.is_ignored(&root.join("gen"), true));
    assert!(filter.is_ignored(&root.join("sub/gen"), true));
    assert!(filter.is_ignored(&root.join("sub/blocked"), true));
}

#[test]
fn watch_filter_matches_ignore_and_global_excludes() {
    const CHILD: &str = "CODESAGE_TEST_WATCH_IGNORE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let global = home.path().join("global-ignore");
        std::fs::write(&global, "global.rs\nrestored.rs\n").unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            format!("[core]\nexcludesFile = {}\n", global.display()),
        )
        .unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "watch_filter_matches_ignore_and_global_excludes"])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".git/info")).unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join(".git/info/exclude"), "local.rs\nrestored.rs\n").unwrap();
    std::fs::write(root.join(".gitignore"), "!restored.rs\n!priority.rs\n").unwrap();
    std::fs::write(root.join(".ignore"), "priority.rs\n").unwrap();
    std::fs::write(root.join("sub/.ignore"), "/ignored.rs\n").unwrap();
    let cases = [
        ("global.rs", false),
        ("local.rs", false),
        ("restored.rs", true),
        ("priority.rs", false),
        ("ignored.rs", true),
        ("sub/ignored.rs", false),
        ("sub/kept.rs", true),
    ];
    for (path, _) in cases {
        std::fs::write(root.join(path), "fn item() {}\n").unwrap();
    }
    let files = discover_files_with_excludes(root, &[]).unwrap();
    let filter = WatchFilter::new(root, &[]).unwrap();
    for (path, admitted) in cases {
        let walked = files.iter().any(|file| file.path == path);
        assert_eq!(walked, admitted, "walker: {path}");
        assert_eq!(
            !filter.is_ignored(&root.join(path), false),
            walked,
            "{path}"
        );
    }
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

#[test]
fn oversized_file_is_skipped_after_read_cap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let path = root.join("huge.py");
    let bytes = vec![b'x'; (MAX_INDEXABLE_FILE_BYTES as usize) + 1];
    std::fs::write(&path, &bytes).unwrap();

    let files = discover_files_with_excludes(root, &[]).unwrap();
    assert!(
        files.is_empty(),
        "file larger than MAX_INDEXABLE_FILE_BYTES must be skipped: {:?}",
        files
    );
}

#[cfg(unix)]
#[test]
fn unreadable_file_is_skipped_not_fatal() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("ok.py"), "def good(): pass\n").unwrap();
    let bad = root.join("bad.py");
    std::fs::write(&bad, "def bad(): pass\n").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();

    // Root bypasses permission bits: if the file is still readable we can't
    // exercise the skip path, so skip rather than assert a false pass.
    if std::fs::read(&bad).is_ok() {
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644)).unwrap();
        eprintln!("skipping: cannot make a file unreadable (running as root?)");
        return;
    }

    let files = discover_files_with_excludes(root, &[])
        .expect("an unreadable file must be skipped, not abort the whole index");
    let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(
        names.contains(&"ok.py"),
        "readable file must be indexed: {names:?}"
    );
    assert!(
        !names.contains(&"bad.py"),
        "unreadable file must be skipped: {names:?}"
    );

    // Restore perms so the tempdir can be cleaned up.
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644)).unwrap();
}

#[cfg(unix)]
#[test]
fn excluded_unreadable_directory_is_pruned_not_fatal() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("ok.py"), "def good(): pass\n").unwrap();
    let vendor = root.join("vendor");
    std::fs::create_dir(&vendor).unwrap();
    std::fs::write(vendor.join("dep.py"), "def dep(): pass\n").unwrap();
    std::fs::set_permissions(&vendor, std::fs::Permissions::from_mode(0o000)).unwrap();

    if std::fs::read_dir(&vendor).is_ok() {
        std::fs::set_permissions(&vendor, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipping: cannot make a directory unreadable (running as root?)");
        return;
    }

    let excludes = vec!["**/vendor/**".to_string()];
    let files = discover_files_with_excludes(root, &excludes)
        .expect("an excluded unreadable directory must be pruned before walk errors");
    let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, vec!["ok.py"]);

    std::fs::set_permissions(&vendor, std::fs::Permissions::from_mode(0o755)).unwrap();
}
