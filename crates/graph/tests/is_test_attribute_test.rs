//! `is_test` as a stored attribute: file flag from the discovery heuristic,
//! symbol flag from the parser, reference flag derived at read time, and the
//! consumers that read them (dependencies, top symbols, top-risk ranking).

use codesage_graph::{
    assess_risk, find_symbol, full_index, impact_analysis_report, list_dependencies,
    top_risk_files, top_risk_files_with_options,
};
use codesage_protocol::{
    FindReferencesRequest, FindSymbolRequest, ImpactOptions, ImpactRequest, ImpactTarget,
};
use codesage_storage::Database;

const LIB_RS: &str = r#"
pub mod util;

pub fn product() -> u32 {
    util::twice(1)
}

pub fn caller() -> u32 {
    product() + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::twice;

    fn helper() -> u32 {
        product()
    }

    #[test]
    fn product_is_one() {
        assert_eq!(helper(), twice(1));
    }
}
"#;

const UTIL_RS: &str = "pub fn twice(x: u32) -> u32 {\n    x * 2\n}\n";

const IT_RS: &str = r#"
use fixture::product;

fn integration_helper() -> u32 {
    product()
}

#[test]
fn integration() {
    assert_eq!(integration_helper(), 2);
}
"#;

fn indexed_project() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), LIB_RS).unwrap();
    std::fs::write(root.join("src/util.rs"), UTIL_RS).unwrap();
    std::fs::write(root.join("tests/it.rs"), IT_RS).unwrap();
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    (dir, db)
}

fn symbol(db: &Database, name: &str) -> Vec<codesage_protocol::Symbol> {
    find_symbol(
        db,
        &FindSymbolRequest {
            name: name.to_string(),
            kind: None,
        },
    )
    .unwrap()
}

#[test]
fn file_flag_is_stored_and_inherited_by_every_symbol_in_a_test_file() {
    let (_dir, db) = indexed_project();
    let flags = db.all_file_test_flags().unwrap();
    let by_path: std::collections::HashMap<&str, bool> = flags
        .iter()
        .map(|(path, is_test, _)| (path.as_str(), *is_test))
        .collect();
    assert!(!by_path["src/lib.rs"]);
    assert!(!by_path["src/util.rs"]);
    assert!(by_path["tests/it.rs"]);
    for (_, _, interpretation) in &flags {
        assert_eq!(
            interpretation.as_deref(),
            Some(codesage_graph::STRUCTURAL_INTERPRETATION)
        );
    }

    let integration_helper = symbol(&db, "integration_helper");
    assert_eq!(integration_helper.len(), 1);
    assert!(
        integration_helper[0].is_test,
        "product-looking fn in a tests/ file inherits the file flag"
    );
}

#[test]
fn parser_marks_reach_find_symbol_rows_and_json_omits_false() {
    let (_dir, db) = indexed_project();
    let helper = symbol(&db, "helper");
    assert_eq!(helper.len(), 1);
    assert!(helper[0].is_test, "helper lives in #[cfg(test)] mod tests");
    let json = serde_json::to_value(&helper[0]).unwrap();
    assert_eq!(json["is_test"], serde_json::json!(true));

    let product = symbol(&db, "product");
    assert_eq!(product.len(), 1);
    assert!(!product[0].is_test);
    let json = serde_json::to_value(&product[0]).unwrap();
    assert!(json.get("is_test").is_none(), "false is omitted: {json}");
}

#[test]
fn reference_flag_is_derived_from_enclosing_symbol_or_file() {
    let (_dir, db) = indexed_project();
    let refs = codesage_graph::find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "product".to_string(),
            kind: None,
        },
    )
    .unwrap()
    .results;
    let call_from = |from_symbol: &str| {
        refs.iter()
            .find(|r| r.from_symbol.as_deref() == Some(from_symbol))
            .unwrap_or_else(|| panic!("no reference from {from_symbol}: {refs:?}"))
    };
    assert!(!call_from("caller").is_test, "product caller");
    assert!(call_from("helper").is_test, "cfg(test) caller");
    assert!(
        call_from("integration_helper").is_test,
        "tests/it.rs caller inherits the file flag"
    );
    let product_callers = refs.iter().filter(|r| !r.is_test).count();
    assert_eq!(product_callers, 1, "one product caller: {refs:?}");
    let json = serde_json::to_value(call_from("caller")).unwrap();
    assert!(json.get("is_test").is_none(), "{json}");
    let json = serde_json::to_value(call_from("helper")).unwrap();
    assert_eq!(json["is_test"], serde_json::json!(true));
    let file_scope_import = refs
        .iter()
        .find(|r| r.from_file == "tests/it.rs" && r.from_symbol.is_none())
        .expect("file-scope `use fixture::product;`");
    assert!(
        file_scope_import.is_test,
        "file flag covers file-scope rows"
    );
}

#[test]
fn cfg_test_use_directives_move_to_test_imports() {
    let (_dir, db) = indexed_project();
    let deps = list_dependencies(&db, "src/lib.rs").unwrap();
    assert!(deps.found);
    assert!(
        deps.test_imports.iter().any(|i| i == "super::*"),
        "`use super::*;` inside mod tests: {deps:?}"
    );
    assert!(
        deps.test_imports.iter().any(|i| i == "crate::util::twice"),
        "`use crate::util::twice;` inside mod tests: {deps:?}"
    );
    assert!(
        !deps
            .imports
            .iter()
            .any(|i| i.starts_with("super") || i == "crate::util::twice"),
        "test-only directives leave imports[]: {deps:?}"
    );
    let json = serde_json::to_value(&deps).unwrap();
    assert!(json["test_imports"].is_array());

    // A file with no test code keeps the old shape: no `test_imports` key.
    let util = list_dependencies(&db, "src/util.rs").unwrap();
    assert!(util.test_imports.is_empty());
    let json = serde_json::to_value(&util).unwrap();
    assert!(json.get("test_imports").is_none(), "{json}");

    // Every directive in a test file is test code.
    let it = list_dependencies(&db, "tests/it.rs").unwrap();
    assert!(it.imports.is_empty(), "{it:?}");
    assert_eq!(it.test_imports, vec!["fixture::product".to_string()]);
}

#[test]
fn top_symbols_exclude_test_code() {
    let (_dir, db) = indexed_project();
    db.upsert_git_file("src/lib.rs", 10.0, 2, 10, Some(1_700_000_000))
        .unwrap();
    let risk = assess_risk(&db, "src/lib.rs").unwrap();
    let names: Vec<&str> = risk.top_symbols.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"product"), "{names:?}");
    assert!(names.contains(&"caller"), "{names:?}");
    for test_name in ["tests", "helper", "product_is_one"] {
        assert!(
            !names.contains(&test_name),
            "{test_name} excluded: {names:?}"
        );
    }
}

#[test]
fn top_risk_ranking_excludes_test_files_unless_asked() {
    let (_dir, db) = indexed_project();
    // The test file is the hottest, fix-heaviest file in the project.
    db.upsert_git_file("tests/it.rs", 100.0, 40, 80, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("src/lib.rs", 5.0, 1, 10, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("src/util.rs", 1.0, 0, 5, Some(1_700_000_000))
        .unwrap();

    let default = top_risk_files(&db, 50).unwrap();
    let default_files: Vec<&str> = default.iter().map(|e| e.file.as_str()).collect();
    assert!(!default_files.contains(&"tests/it.rs"), "{default_files:?}");
    assert!(default_files.contains(&"src/lib.rs"), "{default_files:?}");

    let widened = top_risk_files_with_options(&db, 50, true).unwrap();
    let widened_files: Vec<&str> = widened.iter().map(|e| e.file.as_str()).collect();
    assert_eq!(widened_files[0], "tests/it.rs", "{widened_files:?}");
    assert_eq!(widened.len(), default.len() + 1);
    let explicit_default = top_risk_files_with_options(&db, 50, false).unwrap();
    assert_eq!(
        serde_json::to_value(&explicit_default).unwrap(),
        serde_json::to_value(&default).unwrap()
    );
}

#[test]
fn rows_indexed_before_the_flag_fall_back_to_the_path_heuristic() {
    let (_dir, db) = indexed_project();
    db.upsert_git_file("tests/it.rs", 100.0, 40, 80, Some(1_700_000_000))
        .unwrap();
    db.upsert_git_file("src/lib.rs", 5.0, 1, 10, Some(1_700_000_000))
        .unwrap();
    // Simulate a row written by a pre-0024 binary: flag 0, stale interpretation.
    let stale_id = db
        .upsert_file(&codesage_protocol::FileInfo {
            path: "tests/it.rs".to_string(),
            language: codesage_protocol::Language::Rust,
            content_hash: "pre-0024".to_string(),
            is_test: false,
        })
        .unwrap();
    db.record_file_interpretation(
        stale_id,
        "codesage/structural/v1;parser-queries=3;extraction=5;trust-boundaries=1",
    )
    .unwrap();
    assert!(
        db.all_file_test_flags()
            .unwrap()
            .iter()
            .any(|(path, is_test, _)| path == "tests/it.rs" && !is_test)
    );
    let default = top_risk_files(&db, 50).unwrap();
    assert!(
        !default.iter().any(|e| e.file == "tests/it.rs"),
        "stale row still excluded through the glob: {default:?}"
    );
}

/// A `#[cfg(test)] mod tests` helper and a product function can share a name,
/// and Rust free functions carry the bare name as their qualified name, so the
/// enclosing-definition lookup has to disambiguate by line range.
const COLLIDE_RS: &str = r#"
pub fn target() -> u32 { 1 }

pub fn setup() -> u32 {
    target()
}

#[cfg(test)]
mod tests {
    fn setup() -> u32 {
        target()
    }

    #[test]
    fn t() {
        let _ = setup();
    }
}
"#;

fn collide_project() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"collide\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), COLLIDE_RS).unwrap();
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    (dir, db)
}

fn references(db: &Database, name: &str) -> Vec<codesage_protocol::Reference> {
    codesage_graph::find_references(
        db,
        &FindReferencesRequest {
            symbol_name: name.to_string(),
            kind: None,
        },
    )
    .unwrap()
    .results
}

#[test]
fn same_named_product_and_test_definitions_do_not_share_a_reference_flag() {
    let (_dir, db) = collide_project();

    // The premise: two `setup` definitions in one file under one key.
    let setups = symbol(&db, "setup");
    assert_eq!(setups.len(), 2, "{setups:?}");
    assert_eq!(
        setups
            .iter()
            .map(|s| s.qualified_name.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["setup"]),
        "both definitions share the bare qualified name: {setups:?}"
    );
    assert_eq!(setups.iter().filter(|s| s.is_test).count(), 1, "{setups:?}");

    let product_setup = setups.iter().find(|s| !s.is_test).unwrap();
    let test_setup = setups.iter().find(|s| s.is_test).unwrap();

    let target_refs: Vec<_> = references(&db, "target")
        .into_iter()
        .filter(|r| r.from_symbol.as_deref() == Some("setup"))
        .collect();
    assert_eq!(target_refs.len(), 2, "{target_refs:?}");

    let from_product = target_refs
        .iter()
        .find(|r| r.line >= product_setup.line_start && r.line <= product_setup.line_end)
        .unwrap_or_else(|| panic!("no `target()` call inside the product setup: {target_refs:?}"));
    assert!(
        !from_product.is_test,
        "product `setup` body must stay product code: {from_product:?}"
    );

    let from_test = target_refs
        .iter()
        .find(|r| r.line >= test_setup.line_start && r.line <= test_setup.line_end)
        .unwrap_or_else(|| {
            panic!("no `target()` call inside the cfg(test) setup: {target_refs:?}")
        });
    assert!(from_test.is_test, "{from_test:?}");

    // The `setup()` call from `#[test] fn t` is test code by its own caller.
    let setup_refs = references(&db, "setup");
    let from_t = setup_refs
        .iter()
        .find(|r| r.from_symbol.as_deref() == Some("t"))
        .unwrap_or_else(|| panic!("no `setup()` call from t: {setup_refs:?}"));
    assert!(from_t.is_test, "{from_t:?}");
}

#[test]
fn forward_dependencies_of_a_test_file_survive_the_test_imports_split() {
    let (_dir, db) = indexed_project();
    let report = impact_analysis_report(
        &db,
        &ImpactRequest {
            target: ImpactTarget::File {
                path: "tests/it.rs".to_string(),
            },
            depth: 2,
            source_only: false,
        },
        &ImpactOptions {
            include_forward: true,
            ..ImpactOptions::default()
        },
    )
    .unwrap();
    assert!(
        report
            .forward_dependencies
            .iter()
            .any(|d| d == "fixture::product"),
        "every directive in tests/it.rs is a test import: {:?}",
        report.forward_dependencies
    );
}

const TS_SUITE: &str = r#"
import { product } from "./product";

const fixture = { a: 1 };

describe("product", () => {
  it("works", () => {
    expect(product(fixture.a)).toBe(1);
  });
});
"#;

#[test]
fn a_top_level_describe_marks_the_file_even_when_the_path_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/product.ts"),
        "export function product(a) { return a; }\n",
    )
    .unwrap();
    std::fs::write(root.join("src/product.check.ts"), TS_SUITE).unwrap();
    assert!(
        !codesage_parser::discover::is_test_like_path("src/product.check.ts"),
        "the path heuristic must not be what marks this file"
    );
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();

    let flags: std::collections::HashMap<String, bool> = db
        .all_file_test_flags()
        .unwrap()
        .into_iter()
        .map(|(path, is_test, _)| (path, is_test))
        .collect();
    assert_eq!(flags.get("src/product.check.ts"), Some(&true), "{flags:?}");
    assert_eq!(flags.get("src/product.ts"), Some(&false), "{flags:?}");
}
