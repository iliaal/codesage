//! A lone same-named definition resolves as a callee only where its recorded
//! visibility lets the caller name it. Fixtures live under
//! `tests/fixtures/visibility/{c,rust}`; each is indexed as its own project.

use std::collections::BTreeSet;
use std::path::PathBuf;

use codesage_graph::{find_references, full_index, impact_analysis, trace_call_path};
use codesage_protocol::{
    CallPathRequest, FindReferencesRequest, ImpactRequest, ImpactTarget, Visibility,
};
use codesage_storage::Database;

fn fixture_db(name: &str) -> Database {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/visibility")
        .join(name);
    let db = Database::open_in_memory().unwrap();
    let stats = full_index(&root, &db, &[], false).unwrap();
    assert!(stats.files_indexed >= 3, "{stats:?}");
    db
}

fn dependents(db: &Database, symbol: &str) -> BTreeSet<String> {
    impact_analysis(
        db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: symbol.to_string(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap()
    .into_iter()
    .map(|e| e.file_path)
    .collect()
}

fn path_found(db: &Database, from: &str, to: &str) -> bool {
    trace_call_path(
        db,
        &CallPathRequest {
            from: from.to_string(),
            to: to.to_string(),
            max_depth: 4,
        },
    )
    .unwrap()
    .found
}

fn visibility_of(db: &Database, file: &str, name: &str) -> Option<Visibility> {
    let syms = db.symbols_for_file(file).unwrap();
    let sym = syms
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("{name} in {file}: {syms:?}"));
    sym.visibility
}

#[test]
fn c_static_helper_is_not_a_cross_file_callee() {
    let db = fixture_db("c");
    assert_eq!(
        visibility_of(&db, "util.c", "helper"),
        Some(Visibility::File)
    );
    // The name-based raw row from main.c stays in storage; callee resolution
    // (impact, trace, and the `to` handle on find_references) drops it.
    let raw = db.find_references("helper", None).unwrap();
    assert!(raw.iter().any(|r| r.from_file == "main.c"), "{raw:?}");

    assert_eq!(dependents(&db, "helper"), BTreeSet::new());
    assert!(!path_found(&db, "main", "helper"));
    assert!(path_found(&db, "util_entry", "helper"));
}

/// A two-file C project whose `only_here` definition carries `storage`
/// (`static` or nothing); `other.c` includes `util.h` and calls it.
fn c_project_with_storage(storage: &str) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("util.h"), "int only_here(int x);\n").unwrap();
    std::fs::write(
        root.join("util.c"),
        format!("#include \"util.h\"\n\n{storage}int only_here(int x) {{ return x + 1; }}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("other.c"),
        "#include \"util.h\"\n\nint other_entry(int x) { return only_here(x); }\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    let stats = full_index(root, &db, &[], false).unwrap();
    assert_eq!(stats.files_indexed, 3, "{stats:?}");
    (dir, db)
}

/// The `to` handles on every `find_references` row that `other.c` emits for
/// `only_here`; the raw rows themselves are name-based and always present.
fn to_handles_from_other(db: &Database) -> Vec<Option<String>> {
    let refs = find_references(
        db,
        &FindReferencesRequest {
            symbol_name: "only_here".to_string(),
            kind: None,
        },
    )
    .unwrap();
    assert_eq!(refs.definition_count, 1, "{:?}", refs.results);
    assert!(refs.to_resolution.is_none(), "{:?}", refs.to_resolution);
    let handles: Vec<Option<String>> = refs
        .results
        .iter()
        .filter(|r| r.from_file == "other.c")
        .map(|r| r.to.clone())
        .collect();
    assert!(
        !handles.is_empty(),
        "a raw row from other.c: {:?}",
        refs.results
    );
    handles
}

#[test]
fn c_static_definition_gets_no_to_handle_from_an_including_file() {
    let (_dir, db) = c_project_with_storage("static ");
    assert_eq!(
        visibility_of(&db, "util.c", "only_here"),
        Some(Visibility::File)
    );
    assert!(
        to_handles_from_other(&db).iter().all(Option::is_none),
        "file-local linkage is not nameable from other.c"
    );
}

#[test]
fn c_external_definition_gets_a_to_handle_through_the_header_include() {
    let (_dir, db) = c_project_with_storage("");
    assert_ne!(
        visibility_of(&db, "util.c", "only_here"),
        Some(Visibility::File)
    );
    let handles = to_handles_from_other(&db);
    assert!(
        handles
            .iter()
            .all(|h| h.as_deref() == Some("sym:util.c#only_here")),
        "{handles:?}"
    );
}

#[test]
fn c_header_static_inline_still_resolves_from_includers() {
    let db = fixture_db("c");
    assert_eq!(visibility_of(&db, "clamp.h", "clamp"), None);
    assert_eq!(
        dependents(&db, "clamp"),
        BTreeSet::from(["main.c".to_string(), "util.c".to_string()])
    );
    assert!(path_found(&db, "main", "clamp"));
}

#[test]
fn rust_private_fn_resolves_only_inside_its_module_subtree() {
    let db = fixture_db("rust");
    assert_eq!(
        visibility_of(&db, "src/store.rs", "open_private"),
        Some(Visibility::Module)
    );
    let raw = db.find_references("open_private", None).unwrap();
    assert!(raw.iter().any(|r| r.from_file == "src/api.rs"), "{raw:?}");

    assert_eq!(
        dependents(&db, "open_private"),
        BTreeSet::from(["src/store/cache.rs".to_string()])
    );
    assert!(path_found(&db, "cached", "open_private"));
    assert!(path_found(&db, "open_public", "open_private"));
    // api.rs calls open_private() directly, but the only admissible route
    // goes through the public wrapper.
    let report = trace_call_path(
        &db,
        &CallPathRequest {
            from: "handle".to_string(),
            to: "open_private".to_string(),
            max_depth: 4,
        },
    )
    .unwrap();
    let names: Vec<&str> = report.steps.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["handle", "open_public", "open_private"]);
}

#[test]
fn rust_crate_fn_resolves_within_src_root_but_not_from_integration_tests() {
    let db = fixture_db("rust");
    assert_eq!(
        visibility_of(&db, "src/store.rs", "crate_only"),
        Some(Visibility::Crate)
    );
    let raw = db.find_references("crate_only", None).unwrap();
    assert!(
        raw.iter().any(|r| r.from_file == "tests/integration.rs"),
        "{raw:?}"
    );

    assert_eq!(
        dependents(&db, "crate_only"),
        BTreeSet::from(["src/api.rs".to_string()])
    );
    assert!(path_found(&db, "handle", "crate_only"));
    assert!(!path_found(&db, "exercises_public_surface", "crate_only"));
}

#[test]
fn rust_pub_super_item_resolves_from_parent_module_file() {
    let db = fixture_db("rust");
    assert_eq!(
        visibility_of(&db, "src/store/cache.rs", "evict"),
        Some(Visibility::Crate)
    );
    assert_eq!(
        dependents(&db, "evict"),
        BTreeSet::from(["src/store.rs".to_string()])
    );
    assert!(path_found(&db, "open_public", "evict"));
}

#[test]
fn rust_pub_items_and_trait_impl_methods_resolve_cross_file() {
    let db = fixture_db("rust");
    assert_eq!(
        visibility_of(&db, "src/store.rs", "open_public"),
        Some(Visibility::Public)
    );
    assert_eq!(
        dependents(&db, "open_public"),
        BTreeSet::from(["src/api.rs".to_string(), "tests/integration.rs".to_string()])
    );
    assert!(path_found(&db, "exercises_public_surface", "open_public"));

    let flush: Vec<_> = db
        .symbols_for_file("src/store.rs")
        .unwrap()
        .into_iter()
        .filter(|s| s.name == "flush")
        .collect();
    assert_eq!(flush.len(), 2, "{flush:?}");
    assert!(
        flush
            .iter()
            .all(|s| s.visibility == Some(Visibility::Public))
    );
    assert_eq!(
        dependents(&db, "Store::flush"),
        BTreeSet::from(["src/api.rs".to_string()])
    );
    assert!(path_found(&db, "flush_all", "flush"));
}
