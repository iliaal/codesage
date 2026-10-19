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

fn disk_index(root: &std::path::Path) -> Database {
    std::fs::create_dir(root.join(".codesage")).unwrap();
    let db = Database::open(&root.join(".codesage/index.db")).unwrap();
    full_index(root, &db, &[], false).unwrap();
    db
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

#[test]
fn outgoing_paths_keep_unknown_and_ambiguous_evidence_without_homonym_edges() {
    let (_dir, db) = indexed(&[
        (
            "src/main.ts",
            "import { value } from './util';\nimport missing from './missing';\nimport other from 'unproven-package';\n",
        ),
        ("src/util.ts", "export const value = 1;"),
        ("src/util.js", "export const value = 2;"),
        ("missing.ts", "export const missing = 3;"),
        ("unrelated.ts", "export const other = 4;"),
    ]);
    let deps = list_dependencies(&db, "src/main.ts").unwrap();
    assert!(deps.imports.is_empty(), "{deps:?}");
    assert_eq!(deps.externals.imports, 0);
    assert_eq!(deps.unresolved_imports.len(), 3);
    let ambiguous = deps
        .unresolved_imports
        .iter()
        .find(|row| row.target == "./util")
        .unwrap();
    assert_eq!(
        ambiguous.candidates,
        vec!["file:src/util.js", "file:src/util.ts"]
    );
    assert!(ambiguous.reason.contains("multiple"));
    for target in ["./missing", "unproven-package"] {
        assert!(
            deps.unresolved_imports
                .iter()
                .find(|row| row.target == target)
                .unwrap()
                .candidates
                .is_empty()
        );
    }
    assert_eq!(deps.raw_imports.unwrap().imports.len(), 3);
}

#[test]
fn outgoing_paths_resolve_c_includes_and_python_modules() {
    let (_dir, db) = indexed(&[
        ("app/main.c", "#include \"util.h\"\n#include <stdio.h>\n"),
        ("app/util.h", "int helper(void);"),
        ("main.py", "from helpers import assist\n"),
        ("helpers.py", "def assist():\n    return 1\n"),
    ]);
    let c = list_dependencies(&db, "app/main.c").unwrap();
    assert_eq!(c.imports, vec!["file:app/util.h"]);
    assert_eq!(c.unresolved_imports.len(), 1);
    assert_eq!(c.unresolved_imports[0].target, "<stdio.h>");
    let python = list_dependencies(&db, "main.py").unwrap();
    assert_eq!(python.imports, vec!["file:helpers.py"]);
    assert!(python.unresolved_imports.is_empty(), "{python:?}");
}

#[test]
fn inline_rust_reimports_are_removed_but_external_self_and_super_edges_survive() {
    let (_dir, db) = indexed(&[
        ("src/lib.rs", "pub mod feature; pub fn parent() {}"),
        (
            "src/feature.rs",
            "mod child;\nuse self::child::helper;\nuse super::parent;\nfn local() {}\n#[cfg(test)] mod tests {\nuse super::*;\nuse super::local;\nuse super::child::helper;\n}\n",
        ),
        ("src/feature/child.rs", "pub fn helper() {}"),
        ("src/decoy.rs", "pub fn local() {} pub fn parent() {}"),
    ]);
    let deps = list_dependencies(&db, "src/feature.rs").unwrap();
    assert_eq!(
        deps.imports,
        vec!["file:src/feature/child.rs", "file:src/lib.rs"]
    );
    assert_eq!(deps.intra_file_imports, 2, "{deps:?}");
    assert!(deps.unresolved_imports.is_empty(), "{deps:?}");
    assert_eq!(
        deps.raw_imports.unwrap().test_imports,
        vec!["super::*", "super::local", "super::child::helper"]
    );
}

#[test]
fn test_only_internal_files_remain_separate() {
    let (_dir, db) = indexed(&[
        ("main.js", "import './product.js';\nfunction unused() {}\n"),
        ("product.js", "export function product() {}"),
        ("tests/check.js", "import '../product.js';\n"),
    ]);
    let deps = list_dependencies(&db, "tests/check.js").unwrap();
    assert!(deps.imports.is_empty());
    assert_eq!(deps.test_imports, vec!["file:product.js"]);
}

#[test]
fn rust_manifest_evidence_separates_internal_external_and_unknown_imports() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\", \"dependency\"]\nresolver = \"2\"\n[workspace.dependencies]\nanyhow = \"1\"\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\ninternal = { package = \"dependency\", path = \"../dependency\" }\nanyhow.workspace = true\n",
        ),
        (
            "dependency/Cargo.toml",
            "[package]\nname = \"dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        (
            "app/src/lib.rs",
            "use internal::helper;\nuse anyhow::Result;\nuse std::collections::HashMap;\nuse unknown::helper;\n",
        ),
        ("dependency/src/lib.rs", "pub fn helper() {}"),
        ("app/src/unrelated.rs", "pub fn helper() {}"),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "app/src/lib.rs").unwrap();
    assert_eq!(deps.imports, vec!["file:dependency/src/lib.rs"]);
    assert_eq!(deps.externals.imports, 2, "{deps:?}");
    assert_eq!(deps.externals.packages_total, 2);
    assert_eq!(
        deps.externals
            .top_packages
            .iter()
            .map(|row| row.package.as_str())
            .collect::<Vec<_>>(),
        vec!["anyhow", "std"]
    );
    assert_eq!(deps.unresolved_imports.len(), 1, "{deps:?}");
    assert_eq!(deps.unresolved_imports[0].target, "unknown::helper");
}

#[test]
fn rust_unknown_and_shadowed_packages_do_not_become_external() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\nanyhow = \"1\"\n",
        ),
        (
            "src/lib.rs",
            "mod anyhow { pub struct Result; }\nuse anyhow::Result;\nuse absent::Result;\n",
        ),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.externals.imports, 0);
    assert!(deps.imports.is_empty());
    assert!(
        deps.unresolved_imports
            .iter()
            .any(|row| row.target == "absent::Result")
    );
}

#[test]
fn rust_undeclared_files_and_nested_homonyms_are_not_outgoing_edges() {
    let (_dir, db) = indexed(&[
        (
            "src/lib.rs",
            "use crate::undeclared::helper;\nuse crate::nested_only;\nmod inner { pub fn nested_only() {} }\n",
        ),
        ("src/undeclared.rs", "pub fn helper() {}"),
    ]);
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert!(deps.imports.is_empty(), "{deps:?}");
    assert_eq!(deps.unresolved_imports.len(), 2);
    assert!(
        deps.unresolved_imports
            .iter()
            .all(|row| row.candidates.is_empty())
    );
}

#[test]
fn rust_external_summary_counts_distinct_spellings_and_bounds_top_packages() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\na=\"1\"\nb=\"1\"\nc=\"1\"\nd=\"1\"\ne=\"1\"\nf=\"1\"\n",
        ),
        (
            "src/lib.rs",
            "use a::One;\nuse a::Two;\nuse b::Three;\nuse c::Four;\nuse d::Five;\nuse e::Six;\n#[cfg(test)] mod tests { use f::Seven; use f::Seven; }\n",
        ),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.externals.imports, 6, "{deps:?}");
    assert_eq!(deps.externals.test_imports, 1);
    assert_eq!(deps.externals.packages_total, 6);
    assert_eq!(
        deps.externals
            .top_packages
            .iter()
            .map(|row| row.package.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c", "d", "e"]
    );
    assert_eq!(deps.externals.top_packages[0].imports, 2);
    assert!(deps.unresolved_imports.is_empty());
}

#[test]
fn rust_registry_overrides_keep_external_classification_unknown() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\nanyhow=\"1\"\n[patch.crates-io]\nanyhow={path=\"vendor/anyhow\"}\n",
        ),
        ("src/lib.rs", "use anyhow::Result;\n"),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.externals.imports, 0);
    assert_eq!(deps.unresolved_imports.len(), 1);
    assert_eq!(deps.unresolved_imports[0].target, "anyhow::Result");
}

#[test]
fn rust_attribute_decorated_imports_resolve_files_without_inventing_item_definitions() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nmodels={path=\"models\"}\n",
        ),
        (
            "models/Cargo.toml",
            "[package]\nname=\"models\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
        ),
        (
            "src/lib.rs",
            "use models::Record;\nuse models::Missing;\nuse models::Private;\n#[cfg(test)] mod tests { use models::testing::TestRecord; }\n",
        ),
        (
            "models/src/lib.rs",
            "#[derive(Clone)]\npub struct Record;\nstruct Private;\npub mod testing;\n",
        ),
        (
            "models/src/testing.rs",
            "#[derive(Clone)]\npub struct TestRecord;\n",
        ),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.imports, vec!["file:models/src/lib.rs"]);
    assert_eq!(deps.test_imports, vec!["file:models/src/testing.rs"]);
    assert_eq!(
        deps.unresolved_imports
            .iter()
            .map(|row| row.target.as_str())
            .collect::<Vec<_>>(),
        vec!["models::Missing", "models::Private"]
    );
}

#[test]
fn rust_external_imports_ignore_unrelated_value_bindings_but_keep_namespace_shadows_unknown() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nglobset=\"0.4\"\n",
        ),
        (
            "src/lib.rs",
            "use globset::GlobSet;\nfn unrelated() { let globset = 1; }\nmod scoped { pub struct globset; use globset::GlobBuilder; }\nmod module_shadow { mod globset {} use globset::GlobSetBuilder; }\nmod alias_shadow { use crate::scoped as globset; use globset::Unknown; }\n",
        ),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.externals.imports, 1, "{deps:?}");
    assert_eq!(deps.externals.top_packages[0].package, "globset");
    for target in [
        "globset::GlobBuilder",
        "globset::GlobSetBuilder",
        "globset::Unknown",
    ] {
        assert!(
            deps.unresolved_imports
                .iter()
                .any(|row| row.target == target),
            "{deps:?}"
        );
    }
}

#[test]
fn proven_internal_import_survives_an_unresolved_occurrence_of_the_same_spelling() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nmodels={path=\"models\"}\n",
        ),
        (
            "models/Cargo.toml",
            "[package]\nname=\"models\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
        ),
        (
            "src/lib.rs",
            "use models::Record;\nfn dynamic() { unknown_macro!(); use models::Record; }\n",
        ),
        ("models/src/lib.rs", "#[derive(Clone)] pub struct Record;\n"),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.imports, vec!["file:models/src/lib.rs"]);
    assert_eq!(deps.unresolved_imports.len(), 1);
    assert_eq!(deps.unresolved_imports[0].target, "models::Record");
    assert_eq!(
        deps.unresolved_imports[0].candidates,
        vec!["file:models/src/lib.rs"]
    );
    assert!(
        deps.unresolved_imports[0]
            .reason
            .contains("some import sites")
    );
}

#[test]
fn proven_external_package_preserves_unknown_sites_of_the_same_import() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nanyhow=\"1\"\n",
        ),
        (
            "src/lib.rs",
            "use anyhow::Result;\nfn dynamic() { unknown_macro!(); use anyhow::Result; }\n",
        ),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.externals.imports, 1);
    assert_eq!(deps.externals.top_packages[0].package, "anyhow");
    assert_eq!(deps.unresolved_imports.len(), 1);
    assert_eq!(deps.unresolved_imports[0].target, "anyhow::Result");
    assert!(
        deps.unresolved_imports[0]
            .reason
            .contains("proven external package is retained")
    );
}

#[test]
fn review_bare_import_uses_its_inline_module_namespace() {
    let (dir, _memory) = indexed(&[
        ("src/lib.rs", "mod target; mod consumer;\n"),
        (
            "src/consumer.rs",
            "mod nested { mod target { pub struct Item; } use target::Item; }\n",
        ),
        ("src/target.rs", "pub struct Item;\n"),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/consumer.rs").unwrap();
    assert!(deps.imports.is_empty(), "{deps:?}");
    assert_eq!(deps.intra_file_imports, 1, "{deps:?}");
    assert_eq!(deps.raw_imports.unwrap().imports, vec!["target::Item"]);
}

#[test]
fn review_identical_import_spellings_keep_each_sites_test_scope() {
    let (_dir, db) = indexed(&[
        (
            "src/lib.rs",
            "mod product { pub mod child; use self::child::helper; }\n#[cfg(test)] mod tests { mod child; use self::child::helper; }\n",
        ),
        ("src/product/child.rs", "pub fn helper() {}\n"),
        ("src/tests/child.rs", "pub fn helper() {}\n"),
    ]);
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.imports, vec!["file:src/product/child.rs"]);
    assert_eq!(deps.test_imports, vec!["file:src/tests/child.rs"]);
    let raw = deps.raw_imports.unwrap();
    assert_eq!(raw.imports, vec!["./product/child", "self::child::helper"]);
    assert_eq!(raw.test_imports, vec!["./tests/child"]);
}

#[test]
fn review_renamed_import_does_not_bind_its_original_tail() {
    let (dir, _memory) = indexed(&[
        (
            "src/lib.rs",
            "mod other; use crate::other::item as renamed; #[cfg(test)] mod tests { use super::item; use super::renamed; }\n",
        ),
        ("src/other.rs", "pub fn item() {}\n"),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.imports, vec!["file:src/other.rs"]);
    assert_eq!(deps.intra_file_imports, 1, "{deps:?}");
    assert_eq!(deps.unresolved_imports.len(), 1, "{deps:?}");
    assert_eq!(deps.unresolved_imports[0].target, "super::item");
    assert!(deps.unresolved_imports[0].is_test);
}

#[test]
fn review_package_alias_in_same_use_tree_is_not_external_evidence() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nanyhow=\"1\"\n",
        ),
        (
            "src/lib.rs",
            "mod local { pub struct Result; } use {crate::local as anyhow, anyhow::Result};\n",
        ),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.externals.imports, 0, "{deps:?}");
    assert!(
        deps.unresolved_imports
            .iter()
            .any(|row| row.target == "anyhow::Result")
    );
}

#[test]
fn review_function_local_items_do_not_prove_module_imports() {
    let (dir, _memory) = indexed(&[
        (
            "src/lib.rs",
            "mod target; fn unrelated() { struct Item; use target::Item as Local; } #[cfg(test)] mod tests { use super::Item; }\n",
        ),
        ("src/target.rs", "pub struct Item;\n"),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.intra_file_imports, 0, "{deps:?}");
    assert!(
        deps.unresolved_imports
            .iter()
            .any(|row| row.target == "super::Item")
    );
}

#[test]
fn review_mixed_local_and_external_sites_keep_the_proven_package() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nanyhow=\"1\"\n",
        ),
        (
            "src/lib.rs",
            "use anyhow::Result; mod local { mod anyhow { pub struct Result; } use anyhow::Result; }\n",
        ),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.externals.imports, 1, "{deps:?}");
    assert_eq!(deps.intra_file_imports, 0);
    assert!(deps.imports.is_empty());
    assert!(deps.unresolved_imports.is_empty(), "{deps:?}");
}

#[test]
fn review_external_test_scope_follows_the_proven_import_site() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nanyhow=\"1\"\n",
        ),
        (
            "src/lib.rs",
            "mod local { mod anyhow { pub struct Result; } use anyhow::Result; } #[cfg(test)] mod tests { use anyhow::Result; }\n",
        ),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.externals.imports, 0, "{deps:?}");
    assert_eq!(deps.externals.test_imports, 1, "{deps:?}");
    assert_eq!(deps.externals.top_packages[0].package, "anyhow");
    assert_eq!(deps.raw_imports.unwrap().imports, vec!["anyhow::Result"]);
}

#[test]
fn review_associated_items_do_not_prove_module_imports() {
    let (dir, _memory) = indexed(&[
        (
            "src/lib.rs",
            "mod other; use crate::other::VALUE as Remote; struct Owner; impl Owner { fn associated() {} const VALUE: u8 = 1; } const MODULE: u8 = 2; #[cfg(test)] mod tests { use super::{associated, VALUE, MODULE}; }\n",
        ),
        (
            "src/other.rs",
            "pub struct Owner; impl Owner { pub const VALUE: u8 = 1; }\n",
        ),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.intra_file_imports, 1, "{deps:?}");
    for target in ["crate::other::VALUE", "super::associated", "super::VALUE"] {
        assert!(
            deps.unresolved_imports
                .iter()
                .any(|row| row.target == target),
            "{deps:?}"
        );
    }
}

#[test]
fn review_same_use_tree_globs_preserve_namespace_uncertainty() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nanyhow=\"1\"\n",
        ),
        (
            "src/lib.rs",
            "mod local { pub mod anyhow { pub struct Result; } } use {crate::local::*, anyhow::Result};\n",
        ),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.externals.imports, 0, "{deps:?}");
    assert!(
        deps.unresolved_imports
            .iter()
            .any(|row| row.target == "anyhow::Result")
    );
}

#[test]
fn review_unrelated_parent_glob_keeps_external_package() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nanyhow=\"1\"\n",
        ),
        (
            "src/lib.rs",
            "mod parent { pub struct Other; mod consumer { use {super::*, anyhow::Result}; } }\n",
        ),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.externals.imports, 1, "{deps:?}");
    assert_eq!(deps.externals.top_packages[0].package, "anyhow");
    assert!(
        !deps
            .unresolved_imports
            .iter()
            .any(|row| row.target == "anyhow::Result")
    );
}

#[test]
fn review_current_glob_import_keeps_external_package() {
    let (dir, _memory) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname=\"app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nanyhow=\"1\"\n",
        ),
        ("src/lib.rs", "use anyhow::*;\n"),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.externals.imports, 1, "{deps:?}");
    assert_eq!(deps.externals.top_packages[0].package, "anyhow");
    assert!(deps.unresolved_imports.is_empty(), "{deps:?}");
}

#[test]
fn review_function_local_modules_do_not_publish_import_bindings() {
    let (dir, _memory) = indexed(&[
        (
            "src/lib.rs",
            "mod other; fn owner() { mod inner { use crate::other::Item; } } #[cfg(test)] mod tests { use super::inner::Item; }\n",
        ),
        ("src/other.rs", "pub struct Item;\n"),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.intra_file_imports, 0, "{deps:?}");
    assert!(
        deps.unresolved_imports
            .iter()
            .any(|row| row.target == "super::inner::Item")
    );
}

#[test]
fn review_nested_module_imports_remain_same_file_bindings() {
    let (dir, _memory) = indexed(&[
        (
            "src/lib.rs",
            "mod other; mod inner { pub use crate::other::Item; } #[cfg(test)] mod tests { use super::inner::Item; }\n",
        ),
        ("src/other.rs", "pub struct Item;\n"),
    ]);
    let db = disk_index(dir.path());
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert_eq!(deps.imports, vec!["file:src/other.rs"]);
    assert_eq!(deps.intra_file_imports, 1, "{deps:?}");
    assert!(deps.unresolved_imports.is_empty(), "{deps:?}");
    assert_eq!(
        deps.raw_imports.unwrap().test_imports,
        vec!["super::inner::Item"]
    );
}
