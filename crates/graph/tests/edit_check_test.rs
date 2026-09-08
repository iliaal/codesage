use std::path::Path;
use std::process::Command;

use codesage_graph::edit_check::{EditCheckReport, edit_check};

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn project(file: &str, source: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join(file), source).unwrap();
    git(dir.path(), &["add", file]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "baseline",
        ],
    );
    dir
}

fn check(file: &str, source: &str, replacement: &str) -> EditCheckReport {
    let dir = project(file, source);
    edit_check(dir.path(), file, "f", None, replacement).unwrap()
}

#[test]
fn rust_arity_and_visibility_proofs_exclude_unresolved_calls() {
    let report = check(
        "lib.rs",
        "mod api { pub fn f(a: i32) {} }\nfn caller() { let f = |_: i32| {}; self::api::f(1); f(1); }",
        "fn f(a: i32, b: i32) {}",
    );
    assert!(report.arity_changed && report.visibility_changed);
    assert_eq!(report.incompatible_callers.len(), 1);
    let caller = &report.incompatible_callers[0];
    assert_eq!(caller.expression, "self::api::f");
    assert!(caller.reason.contains("1 arguments"));
    assert!(caller.reason.contains("newly private"));
    assert!(report.counts_floor);
}

#[test]
fn rust_compiler_confirms_reported_arity_and_privacy_breaks() {
    let source = "mod api { pub fn f(a: i32) {} }\nfn caller() { self::api::f(1); }\n";
    let dir = project("lib.rs", source);
    let compile = || {
        Command::new("rustc")
            .current_dir(dir.path())
            .args(["--crate-type", "lib", "--emit", "metadata", "lib.rs"])
            .output()
            .unwrap()
    };
    let baseline = compile();
    assert!(
        baseline.status.success(),
        "{}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    for (replacement, diagnostic) in [("pub fn f() {}", "E0061"), ("fn f(a: i32) {}", "E0603")] {
        let report = edit_check(dir.path(), "lib.rs", "f", None, replacement).unwrap();
        assert_eq!(report.incompatible_callers.len(), 1);
        std::fs::write(
            dir.path().join("lib.rs"),
            source.replace("pub fn f(a: i32) {}", replacement),
        )
        .unwrap();
        let proposed = compile();
        assert!(!proposed.status.success());
        assert!(String::from_utf8_lossy(&proposed.stderr).contains(diagnostic));
    }
}

#[test]
fn same_module_privacy_and_unchanged_arguments_do_not_break_callers() {
    let report = check(
        "lib.rs",
        "pub fn f(a: i32) {}\nfn caller() { self::f(1); }",
        "fn f(a: i32) {}",
    );
    assert!(report.visibility_changed);
    assert!(!report.arity_changed);
    assert!(report.incompatible_callers.is_empty());
}

#[test]
fn imports_and_attributes_make_rust_resolution_unknown() {
    for prefix in ["use std::fmt;\n", "#[cfg(unix)]\n"] {
        let report = check(
            "lib.rs",
            &format!("{prefix}pub fn f(a: i32) {{}}\nfn caller() {{ self::f(1); }}"),
            "pub fn f() {}",
        );
        assert!(report.incompatible_callers.is_empty());
        assert!(
            report
                .unknown
                .iter()
                .any(|s| s.contains("alter name resolution"))
        );
    }
}

#[test]
fn nested_modules_resolve_super_and_reject_escaping_paths() {
    let report = check(
        "lib.rs",
        "pub fn f(a: i32) {}\nmod inner { fn caller() { super::f(1); super::super::f(1); } }",
        "pub fn f() {}",
    );
    assert_eq!(report.incompatible_callers.len(), 1);
    assert_eq!(report.incompatible_callers[0].expression, "super::f");
}

#[test]
fn cpp_overload_diff_keeps_unchanged_overloads_and_requires_selection() {
    let dir = project(
        "api.cpp",
        "int f(int x) { return x; }\nint f(int x, int y) { return x+y; }\n",
    );
    assert!(edit_check(dir.path(), "api.cpp", "f", None, "int f() { return 0; }").is_err());
    let report = edit_check(dir.path(), "api.cpp", "f", Some(1), "int f() { return 0; }").unwrap();
    assert_eq!(report.overloads_before.len(), 2);
    assert_eq!(report.overloads_after.len(), 2);
    assert_eq!(report.overloads_before[1], report.overloads_after[1]);
    assert!(report.overloads_changed);
    assert_eq!(report.before.arity.unwrap().minimum, 1);
    assert_eq!(report.after.arity.unwrap().minimum, 0);
    assert!(report.incompatible_callers.is_empty());
}

#[test]
fn parses_each_unambiguous_supported_language() {
    for (file, source, replacement, before, after) in [
        ("a.rs", "fn f(a: i32) {}", "fn f() {}", Some(1), Some(0)),
        (
            "a.py",
            "def f(a, b=1):\n    pass\n",
            "def f():\n    pass",
            Some(1),
            Some(0),
        ),
        (
            "a.c",
            "int f(int a) { return a; }",
            "int f(void) { return 0; }",
            Some(1),
            Some(0),
        ),
        (
            "a.cpp",
            "int f(int a) { return a; }",
            "int f() { return 0; }",
            Some(1),
            Some(0),
        ),
        (
            "a.java",
            "class A { public int f(int a) { return a; } }",
            "private int f() { return 0; }",
            Some(1),
            Some(0),
        ),
        (
            "a.php",
            "<?php function f($a) {}",
            "function f() {}",
            Some(1),
            Some(0),
        ),
        ("a.js", "function f(a) {}", "function f() {}", None, None),
        (
            "a.ts",
            "function f(a: number) {}",
            "function f() {}",
            Some(1),
            Some(0),
        ),
        (
            "a.go",
            "package main\nfunc f(a, b int) {}",
            "func f() {}",
            Some(2),
            Some(0),
        ),
    ] {
        let report = check(file, source, replacement);
        assert_eq!(report.before.arity.map(|a| a.minimum), before, "{file}");
        assert_eq!(report.after.arity.map(|a| a.minimum), after, "{file}");
        assert!(report.overloads_changed, "{file}");
    }
}

#[test]
fn receiver_and_keyword_only_parameters_have_unknown_call_arity() {
    for (file, source, replacement) in [
        (
            "a.rs",
            "struct A; impl A { fn f(&self, n: i32) {} }",
            "fn f(&self) {}",
        ),
        (
            "a.ts",
            "function f(this: Object, n: number) {}",
            "function f(this: Object) {}",
        ),
        (
            "a.py",
            "def f(*, n):\n    pass\n",
            "def f(*, n=1):\n    pass",
        ),
    ] {
        let report = check(file, source, replacement);
        assert!(report.before.arity.is_none(), "{file}: {report:?}");
        assert!(report.after.arity.is_none(), "{file}: {report:?}");
        assert!(report.incompatible_callers.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn fifo_and_symlink_worktrees_are_unreadable_without_blocking_head_analysis() {
    for fifo in [true, false] {
        let dir = project("lib.rs", "fn f(a: i32) {}\nfn caller() { self::f(1); }\n");
        std::fs::remove_file(dir.path().join("lib.rs")).unwrap();
        if fifo {
            assert!(
                Command::new("mkfifo")
                    .arg(dir.path().join("lib.rs"))
                    .status()
                    .unwrap()
                    .success()
            );
        } else {
            std::os::unix::fs::symlink("missing-destination", dir.path().join("lib.rs")).unwrap();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = edit_check(dir.path(), "lib.rs", "f", None, "fn f() {}");
            tx.send(result).unwrap();
        });
        let report = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("HEAD analysis must not block on a FIFO")
            .unwrap();
        assert!(!report.worktree_matches_head);
        assert_eq!(report.incompatible_callers.len(), 1);
    }
}

#[test]
fn cpp_prototypes_are_retained_and_identical_definitions_are_deduplicated() {
    let dir = project(
        "api.cpp",
        "int f(int x);\nint f(int x) { return x; }\nint f(double x) { return 0; }\n",
    );
    let report = edit_check(
        dir.path(),
        "api.cpp",
        "f",
        Some(3),
        "int f(double x, double y) { return 0; }",
    )
    .unwrap();
    assert_eq!(report.overloads_before.len(), 2);
    assert_eq!(report.overloads_after.len(), 2);
    assert_eq!(report.overloads_before[0].declaration, "int f(int x)");
    assert_eq!(report.overloads_before[0], report.overloads_after[0]);
    assert!(report.overloads_changed);
}

#[test]
fn cpp_pointer_and_reference_return_prototypes_are_in_the_snapshot() {
    let dir = project(
        "api.cpp",
        "char *f(int x);\nchar &f(char x);\nint f(double x) { return 0; }\n",
    );
    let report = edit_check(
        dir.path(),
        "api.cpp",
        "f",
        Some(3),
        "int f(double x, double y) { return 0; }",
    )
    .unwrap();
    assert_eq!(report.overloads_before.len(), 3, "{report:?}");
    assert_eq!(report.overloads_after.len(), 3);
    assert_eq!(
        report.overloads_before[0].arity.as_ref().unwrap().minimum,
        1
    );
    assert_eq!(report.overloads_before[0], report.overloads_after[0]);
    assert_eq!(report.overloads_before[1], report.overloads_after[1]);
}

#[test]
#[ignore = "requires a C++ compiler; run explicitly for the prototype runtime oracle"]
fn cpp_prototype_fixture_compiles() {
    let dir = project(
        "api.cpp",
        "int f(int x);\nint f(int x) { return x; }\nint f(double x) { return 0; }\n",
    );
    let output = Command::new("c++")
        .current_dir(dir.path())
        .args(["-fsyntax-only", "api.cpp"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn php_defaults_before_required_parameters_are_not_optional() {
    let report = check(
        "api.php",
        "<?php function f($a=1, $b) {}\n",
        "function f($a, $b) {}",
    );
    assert_eq!(report.before.arity.as_ref().unwrap().minimum, 2);
    assert_eq!(report.before.arity.as_ref().unwrap().maximum, None);
    assert_eq!(report.after.arity.as_ref().unwrap().minimum, 2);
    assert!(!report.arity_changed);
}

#[test]
#[ignore = "requires PHP; run explicitly for the Reflection and ArgumentCountError oracle"]
fn php_runtime_confirms_defaults_before_required_parameters() {
    let php = Command::new("php").args(["-r", "function f($a=1, $b) {} echo (new ReflectionFunction('f'))->getNumberOfRequiredParameters(); f(1,2,3); try { f(1); exit(3); } catch (ArgumentCountError $e) {}"])
        .output().unwrap();
    assert!(
        php.status.success(),
        "{}",
        String::from_utf8_lossy(&php.stderr)
    );
    assert_eq!(php.stdout, b"2");
}

#[test]
fn c_and_cpp_ellipsis_leave_maximum_unbounded() {
    for file in ["a.c", "a.cpp"] {
        let report = check(
            file,
            "int f(int n, ...) { return n; }",
            "int f(int n, int m, ...) { return n; }",
        );
        assert_eq!(report.before.arity.as_ref().unwrap().minimum, 1);
        assert_eq!(report.before.arity.as_ref().unwrap().maximum, None);
        assert_eq!(report.after.arity.as_ref().unwrap().minimum, 2);
        assert_eq!(report.after.arity.as_ref().unwrap().maximum, None);
    }
}

#[test]
fn java_static_modifier_is_not_a_visibility_change() {
    let report = check(
        "A.java",
        "class A { public static void f() {} }",
        "public void f() {}",
    );
    assert_eq!(report.before.visibility.as_deref(), Some("public"));
    assert_eq!(report.after.visibility.as_deref(), Some("public"));
    assert!(!report.visibility_changed);
    assert!(report.overloads_changed);
}

#[test]
fn reads_head_and_leaves_dirty_worktree_and_git_unchanged() {
    let dir = project(
        "lib.rs",
        "pub fn f(a: i32) {}\nfn caller() { self::f(1); }\n",
    );
    let dirty = b"not even parseable source";
    std::fs::write(dir.path().join("lib.rs"), dirty).unwrap();
    let head = git(dir.path(), &["rev-parse", "HEAD"]);
    let index = std::fs::read(dir.path().join(".git/index")).unwrap();
    let report = edit_check(dir.path(), "lib.rs", "f", None, "pub fn f() {}").unwrap();
    assert!(!report.worktree_matches_head);
    assert_eq!(report.head, String::from_utf8(head.clone()).unwrap().trim());
    assert_eq!(report.incompatible_callers.len(), 1);
    assert_eq!(std::fs::read(dir.path().join("lib.rs")).unwrap(), dirty);
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head);
    assert_eq!(std::fs::read(dir.path().join(".git/index")).unwrap(), index);
    assert!(!dir.path().join(".codesage").exists());
}

#[test]
fn rejects_malformed_extra_renamed_and_traversing_replacements() {
    let dir = project("lib.rs", "fn f() {}\n");
    for replacement in [
        "fn f( {",
        "fn f() {} fn g() {}",
        "fn g() {}",
        "fn f() {} use std::fmt;",
    ] {
        assert!(
            edit_check(dir.path(), "lib.rs", "f", None, replacement).is_err(),
            "{replacement}"
        );
    }
    for file in ["../lib.rs", "/lib.rs", ""] {
        assert!(edit_check(dir.path(), file, "f", None, "fn f() {}").is_err());
    }
}
