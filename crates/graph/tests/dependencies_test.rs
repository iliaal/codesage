use codesage_graph::{find_references, full_index, list_dependencies, list_dependencies_batch};
use codesage_protocol::{FindReferencesRequest, ReferenceKind};
use codesage_storage::Database;

fn indexed(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (path, body) in files {
        let full = root.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, body).unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    (dir, db)
}

#[test]
fn rust_use_crate_import_appears_in_imported_by() {
    let (_dir, db) = indexed(&[
        (
            "src/lib.rs",
            "pub mod util;\nuse crate::util::helper;\n\npub fn run() -> u32 {\n    helper()\n}\n",
        ),
        ("src/util.rs", "pub fn helper() -> u32 {\n    1\n}\n"),
    ]);

    let deps = list_dependencies(&db, "src/util.rs").unwrap();
    assert!(deps.found);
    assert_eq!(
        deps.imported_by,
        vec!["src/lib.rs".to_string()],
        "use crate::util::helper must resolve back to src/util.rs"
    );
}

#[test]
fn rust_binary_crate_import_uses_the_binary_directory() {
    let (_dir, db) = indexed(&[
        (
            "src/bin/tool.rs",
            "use crate::foo::helper; fn main() { helper(); }",
        ),
        ("src/bin/foo.rs", "pub fn helper() {}"),
        ("src/foo.rs", "pub fn helper() {}"),
    ]);
    assert!(
        list_dependencies(&db, "src/foo.rs")
            .unwrap()
            .imported_by
            .is_empty()
    );
    assert_eq!(
        list_dependencies(&db, "src/bin/foo.rs")
            .unwrap()
            .imported_by,
        vec!["src/bin/tool.rs"]
    );
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "helper".into(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    assert_eq!(refs.results.len(), 1);
    assert_eq!(
        refs.results[0].to.as_deref(),
        Some("sym:src/bin/foo.rs#helper")
    );
}

#[test]
fn rust_integration_test_module_is_a_sibling_import() {
    let (_dir, db) = indexed(&[
        (
            "tests/t.rs",
            "mod support; #[test] fn check() { support::helper(); }",
        ),
        ("tests/support.rs", "pub fn helper() {}"),
        ("src/support.rs", "pub fn helper() {}"),
    ]);
    assert!(
        list_dependencies(&db, "src/support.rs")
            .unwrap()
            .imported_by
            .is_empty()
    );
    assert_eq!(
        list_dependencies(&db, "tests/support.rs")
            .unwrap()
            .imported_by,
        vec!["tests/t.rs"]
    );
}

#[test]
fn rust_flat_targets_admit_crate_visible_shared_modules() {
    for (entry, module) in [
        ("tests/t.rs", "tests/support/mod.rs"),
        ("src/bin/tool.rs", "src/bin/support/mod.rs"),
    ] {
        let (_dir, db) = indexed(&[
            (entry, "mod support; use support::f; fn run() { f(); }"),
            (module, "pub(crate) fn f() {}"),
            ("src/support.rs", "pub(crate) fn f() {}"),
        ]);
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "f".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        assert_eq!(refs.results.len(), 1);
        assert_eq!(
            refs.results[0].to,
            Some(format!("sym:{module}#f")),
            "{entry}"
        );
    }
}

#[test]
fn rust_flat_included_modules_keep_child_self_and_super_context() {
    for root in ["tests", "src/bin"] {
        let entry = format!("{root}/t.rs");
        let support = format!("{root}/support.rs");
        let child = format!("{root}/support/child.rs");
        let decoy = format!("{root}/child.rs");
        let (_dir, db) = indexed(&[
            (
                &entry,
                "mod support; fn parent() {} fn main() { support::run(); }",
            ),
            (
                &support,
                "mod child; use super::parent; use self::child::*; pub fn run() { parent(); value(); }",
            ),
            (&child, "pub fn value() {}"),
            (&decoy, "pub fn value() {}"),
        ]);
        let child_deps = list_dependencies(&db, &child).unwrap();
        assert_eq!(child_deps.imported_by, vec![support.clone()]);
        assert!(
            child_deps
                .note
                .unwrap()
                .contains("standalone Cargo targets")
        );
        assert!(
            list_dependencies(&db, &decoy)
                .unwrap()
                .imported_by
                .is_empty()
        );
        for (name, file) in [("parent", &entry), ("value", &child)] {
            let refs = find_references(
                &db,
                &FindReferencesRequest {
                    symbol_name: name.into(),
                    kind: Some(ReferenceKind::Call),
                },
            )
            .unwrap();
            assert_eq!(refs.results.len(), 1);
            assert_eq!(refs.results[0].to, Some(format!("sym:{file}#{name}")));
        }
    }
}

#[test]
fn rust_shared_modules_do_not_choose_one_of_several_parent_targets() {
    let (_dir, db) = indexed(&[
        ("tests/a.rs", "mod support; pub fn parent() {}"),
        ("tests/b.rs", "mod support; pub fn parent() {}"),
        (
            "tests/support.rs",
            "mod child; use super::*; pub fn run() { parent(); }",
        ),
        ("tests/support/child.rs", "pub fn child() {}"),
    ]);
    assert_eq!(
        list_dependencies(&db, "tests/support/child.rs")
            .unwrap()
            .imported_by,
        vec!["tests/support.rs"]
    );
    for root in ["tests/a.rs", "tests/b.rs"] {
        assert!(list_dependencies(&db, root).unwrap().imported_by.is_empty());
    }
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "parent".into(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    assert_eq!(refs.results.len(), 1);
    assert_eq!(refs.results[0].to, None);
}

#[test]
fn rust_included_flat_modules_do_not_expose_private_items_to_their_parent() {
    let (_dir, db) = indexed(&[
        (
            "tests/t.rs",
            "mod support; use support::hidden; fn run() { hidden(); }",
        ),
        ("tests/support.rs", "fn hidden() {}"),
    ]);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "hidden".into(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    assert_eq!(refs.results.len(), 1);
    assert_eq!(refs.results[0].to, None);
}

#[test]
fn rust_conventional_target_subdirectories_keep_their_own_modules() {
    for target in [
        "src/bin/tool",
        "tests/check",
        "benches/bench",
        "examples/demo",
    ] {
        let entry = format!("crates/app/{target}/main.rs");
        let helper = format!("crates/app/{target}/helper.rs");
        let nested = format!("crates/app/{target}/helper/nested.rs");
        let (_dir, db) = indexed(&[
            (&entry, "mod helper; use crate::helper::*; fn main() {}"),
            (&helper, "mod nested; pub fn help() {}"),
            (&nested, "pub fn nested() {}"),
            ("crates/app/src/helper.rs", "pub fn help() {}"),
        ]);
        assert_eq!(
            list_dependencies(&db, &helper).unwrap().imported_by,
            vec![entry]
        );
        assert_eq!(
            list_dependencies(&db, &nested).unwrap().imported_by,
            vec![helper]
        );
        assert!(
            list_dependencies(&db, "crates/app/src/helper.rs")
                .unwrap()
                .imported_by
                .is_empty()
        );
    }
}

#[test]
fn rust_inline_module_declarations_resolve_only_external_children() {
    let (_dir, db) = indexed(&[
        ("tests/t.rs", "mod outer { mod support; }"),
        ("tests/outer/support.rs", "pub fn helper() {}"),
        ("tests/support.rs", "pub fn helper() {}"),
        ("tests/outer.rs", "pub fn helper() {}"),
    ]);
    assert_eq!(
        list_dependencies(&db, "tests/outer/support.rs")
            .unwrap()
            .imported_by,
        vec!["tests/t.rs"]
    );
    for unrelated in ["tests/support.rs", "tests/outer.rs"] {
        assert!(
            list_dependencies(&db, unrelated)
                .unwrap()
                .imported_by
                .is_empty(),
            "{unrelated}"
        );
    }
}

#[test]
fn rust_whole_module_use_appears_in_imported_by() {
    let (_dir, db) = indexed(&[
        (
            "src/lib.rs",
            "pub mod util;\nuse crate::util;\n\npub fn run() -> u32 {\n    util::helper()\n}\n",
        ),
        ("src/util.rs", "pub fn helper() -> u32 {\n    1\n}\n"),
    ]);

    let deps = list_dependencies(&db, "src/util.rs").unwrap();
    assert_eq!(deps.imported_by, vec!["src/lib.rs".to_string()]);
}

#[test]
fn rust_workspace_crate_use_appears_in_imported_by() {
    let (_dir, db) = indexed(&[
        (
            "crates/app/src/lib.rs",
            "pub mod util;\nuse crate::util::helper;\n\npub fn run() -> u32 {\n    helper()\n}\n",
        ),
        (
            "crates/app/src/util.rs",
            "pub fn helper() -> u32 {\n    1\n}\n",
        ),
    ]);

    let deps = list_dependencies(&db, "crates/app/src/util.rs").unwrap();
    assert_eq!(
        deps.imported_by,
        vec!["crates/app/src/lib.rs".to_string()],
        "crate:: resolves relative to the importer's own src root in a workspace"
    );
}

#[test]
fn sibling_crates_with_same_named_module_resolve_to_their_own_crate() {
    let (_dir, db) = indexed(&[
        (
            "crates/a/src/lib.rs",
            "pub mod util;\nuse crate::util::helper;\n\npub fn run_a() -> u32 {\n    helper()\n}\n",
        ),
        (
            "crates/a/src/util.rs",
            "pub fn helper() -> u32 {\n    1\n}\n",
        ),
        (
            "crates/b/src/lib.rs",
            "pub mod util;\nuse crate::util::helper;\n\npub fn run_b() -> u32 {\n    helper()\n}\n",
        ),
        (
            "crates/b/src/util.rs",
            "pub fn helper() -> u32 {\n    2\n}\n",
        ),
    ]);

    let a = list_dependencies(&db, "crates/a/src/util.rs").unwrap();
    assert_eq!(
        a.imported_by,
        vec!["crates/a/src/lib.rs".to_string()],
        "crate:: in crates/a must not reach across to crates/b"
    );
    let b = list_dependencies(&db, "crates/b/src/util.rs").unwrap();
    assert_eq!(b.imported_by, vec!["crates/b/src/lib.rs".to_string()]);
}

#[test]
fn js_relative_specifier_appears_in_imported_by() {
    let (_dir, db) = indexed(&[
        (
            "main.js",
            "import { greet } from './util.js';\n\nexport function run() {\n  return greet('x');\n}\n",
        ),
        ("util.js", "export function greet(n) {\n  return n;\n}\n"),
    ]);

    let deps = list_dependencies(&db, "util.js").unwrap();
    assert_eq!(
        deps.imported_by,
        vec!["main.js".to_string()],
        "'./util.js' must resolve back to util.js"
    );
}

#[test]
fn js_specifier_from_subdirectory_resolves_lexically() {
    let (_dir, db) = indexed(&[
        (
            "lib/main.js",
            "import { greet } from '../util/helpers.js';\n\nexport function run() {\n  return greet('x');\n}\n",
        ),
        (
            "util/helpers.js",
            "export function greet(n) {\n  return n;\n}\n",
        ),
    ]);

    let deps = list_dependencies(&db, "util/helpers.js").unwrap();
    assert_eq!(deps.imported_by, vec!["lib/main.js".to_string()]);
}

#[test]
fn c_include_paths_appear_in_imported_by() {
    let (_dir, db) = indexed(&[
        (
            "main.c",
            "#include \"util.h\"\n#include \"inc/deep.h\"\n\nint run(void) {\n    return add(1, 2);\n}\n",
        ),
        ("util.h", "int add(int a, int b);\n"),
        ("inc/deep.h", "int deep(void);\n"),
    ]);

    let sibling = list_dependencies(&db, "util.h").unwrap();
    assert_eq!(sibling.imported_by, vec!["main.c".to_string()]);

    let nested = list_dependencies(&db, "inc/deep.h").unwrap();
    assert_eq!(nested.imported_by, vec!["main.c".to_string()]);
}

#[test]
fn c_directory_include_does_not_claim_unrelated_trees() {
    let (_dir, db) = indexed(&[
        (
            "app/main.c",
            "#include \"sub/foo.h\"\n\nint run(void) {\n    return foo();\n}\n",
        ),
        ("app/sub/foo.h", "int foo(void);\n"),
        ("vendor/sub/foo.h", "int foo_vendor(void);\n"),
    ]);

    let owned = list_dependencies(&db, "app/sub/foo.h").unwrap();
    assert_eq!(owned.imported_by, vec!["app/main.c".to_string()]);

    let vendor = list_dependencies(&db, "vendor/sub/foo.h").unwrap();
    assert!(
        vendor.imported_by.is_empty(),
        "suffix match must not claim vendor/sub/foo.h, got {:?}",
        vendor.imported_by
    );
}

#[test]
fn python_import_still_resolves() {
    let (_dir, db) = indexed(&[
        (
            "main.py",
            "from helpers import assist\n\ndef run():\n    return assist()\n",
        ),
        ("helpers.py", "def assist():\n    return 1\n"),
    ]);

    let deps = list_dependencies(&db, "helpers.py").unwrap();
    assert_eq!(deps.imported_by, vec!["main.py".to_string()]);
}

#[test]
fn unrelated_files_do_not_land_in_imported_by() {
    let (_dir, db) = indexed(&[
        (
            "main.js",
            "import { greet } from './util.js';\n\nexport function run() {\n  return greet('x');\n}\n",
        ),
        ("util.js", "export function greet(n) {\n  return n;\n}\n"),
        (
            "other/util.js",
            "export function shout(n) {\n  return n;\n}\n",
        ),
        ("bystander.js", "export function idle() {\n  return 0;\n}\n"),
    ]);

    // `./util.js` from the root must not claim the same-named file in other/.
    let nested = list_dependencies(&db, "other/util.js").unwrap();
    assert!(
        nested.imported_by.is_empty(),
        "got {:?}",
        nested.imported_by
    );

    let bystander = list_dependencies(&db, "bystander.js").unwrap();
    assert!(bystander.imported_by.is_empty());
}

#[test]
fn unindexed_file_reports_not_found() {
    let (_dir, db) = indexed(&[("main.py", "def run():\n    return 1\n")]);
    let deps = list_dependencies(&db, "missing.py").unwrap();
    assert!(!deps.found);
    assert!(deps.imported_by.is_empty());
}

#[test]
fn batch_matches_single_file_results_in_order() {
    let (_dir, db) = indexed(&[
        (
            "src/lib.rs",
            "pub mod util;\nuse crate::util::helper;\n\npub fn run() -> u32 {\n    helper()\n}\n",
        ),
        ("src/util.rs", "pub fn helper() -> u32 {\n    1\n}\n"),
        ("main.py", "def run():\n    return 1\n"),
    ]);

    let paths = ["src/util.rs", "src/lib.rs", "main.py", "missing.py"];
    let refs: Vec<&str> = paths.to_vec();
    let batched = list_dependencies_batch(&db, &refs).unwrap();
    assert_eq!(batched.len(), paths.len());
    for (entry, path) in batched.iter().zip(paths.iter()) {
        let single = list_dependencies(&db, path).unwrap();
        assert_eq!(entry.file_path, single.file_path);
        assert_eq!(entry.found, single.found);
        assert_eq!(entry.imports, single.imports);
        assert_eq!(entry.imported_by, single.imported_by);
    }
}
