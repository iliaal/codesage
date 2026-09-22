//! One target grammar, one ambiguity policy, across the read-side tools.

use codesage_graph::{
    FindSymbolOptions, TargetError, find_references, find_symbol, find_symbol_with_options,
    full_index, impact_analysis,
};
use codesage_protocol::{FindReferencesRequest, FindSymbolRequest, ImpactRequest, ImpactTarget};
use codesage_storage::Database;

/// Two `fn search` definitions in different modules, a `mod render;`
/// declaration beside the module it declares, and a caller of each.
fn project() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/index")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "mod render;\nmod search;\nmod index;\n\npub fn run() {\n    crate::search::search();\n    render::render();\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/search.rs"),
        "pub fn search() -> u32 {\n    1\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/index/mod.rs"),
        "pub fn search() -> u32 {\n    2\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("src/render.rs"), "pub fn render() {}\n").unwrap();
    std::fs::write(
        root.join("src/caller.rs"),
        "use crate::index::search;\n\npub fn call_indexed() -> u32 {\n    search()\n}\n",
    )
    .unwrap();
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();
    (dir, db)
}

fn symbols(db: &Database, name: &str) -> codesage_protocol::FindSymbolResults {
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
fn impact_refuses_a_shared_name_and_accepts_the_handle_it_offers() {
    let (_dir, db) = project();

    let request = |target: &str| ImpactRequest {
        target: ImpactTarget::Symbol {
            name: target.to_string(),
        },
        depth: 1,
        source_only: false,
    };

    let err = impact_analysis(&db, &request("search")).unwrap_err();
    let target = err
        .downcast_ref::<TargetError>()
        .unwrap_or_else(|| panic!("typed target error, got: {err:#}"));
    let TargetError::Ambiguous {
        candidates,
        candidates_total,
        overloads,
        ..
    } = target
    else {
        panic!("expected an ambiguous target, got {target}");
    };
    assert_eq!(*candidates_total, 2);
    assert_eq!(*overloads, 0);
    let first = candidates[0].handle.clone();
    assert_eq!(first, "sym:src/index/mod.rs#search");
    assert!(
        candidates.iter().all(|c| c.confidence >= 0.8),
        "{candidates:?}"
    );

    // The remedy the refusal offers is the input that answers the question.
    let scoped = impact_analysis(&db, &request(&first)).unwrap();
    let paths: Vec<&str> = scoped.iter().map(|e| e.file_path.as_str()).collect();
    assert_eq!(
        paths,
        ["src/caller.rs"],
        "only the caller of the indexed definition"
    );
}

#[test]
fn find_references_returns_the_union_and_says_so() {
    let (_dir, db) = project();

    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "search".to_string(),
            kind: None,
        },
    )
    .unwrap();

    // Union of both definitions' call sites: the answer a reference sweep wants.
    let callers: Vec<&str> = refs.results.iter().map(|r| r.from_file.as_str()).collect();
    assert!(
        callers.contains(&"src/lib.rs") && callers.contains(&"src/caller.rs"),
        "{callers:?}"
    );
    assert_eq!(refs.definition_count, 2);
    assert!(refs.ambiguous);
    assert!(refs.note.is_some());

    let target = refs.target.expect("the union carries its resolution");
    assert!(target.ambiguous);
    assert_eq!(target.candidates_total, 2);
    assert_eq!(
        target.handles(),
        ["sym:src/index/mod.rs#search", "sym:src/search.rs#search"],
        "the agent can split the union by handle"
    );
}

/// `find_references` resolves through the grammar under a symbol kind hint
/// (paths and `path:line` are not in scope, as for `find_similar`), and its
/// rows are keyed on what the handle resolves to rather than the handle text.
#[test]
fn find_references_accepts_a_sym_handle() {
    let (_dir, db) = project();
    let rows = |spelling: &str| {
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: spelling.to_string(),
                kind: None,
            },
        )
        .unwrap();
        let mut rows: Vec<(String, u32)> = refs
            .results
            .iter()
            .map(|r| (r.from_file.clone(), r.line))
            .collect();
        rows.sort();
        (rows, refs)
    };

    let (by_name, _) = rows("search");
    assert!(!by_name.is_empty());

    let (by_handle, refs) = rows("sym:src/search.rs#search");
    assert_eq!(by_handle, by_name, "same union of rows as the bare name");
    // The rows are keyed by the bare name, so the disclosure covers every
    // definition sharing it, while `target` stays the handle's own resolution.
    assert_eq!(refs.definition_count, 2);
    assert!(refs.ambiguous);
    let note = refs
        .note
        .as_deref()
        .expect("a union of two definitions is disclosed");
    assert!(
        note.starts_with("2 definitions share the name 'search'"),
        "{note}"
    );
    assert!(
        note.contains("sym:src/index/mod.rs#search") && note.contains("sym:src/search.rs#search"),
        "{note}"
    );
    assert_eq!(
        refs.target.as_ref().map(|t| t.handles()),
        Some(vec!["sym:src/search.rs#search".to_string()])
    );
    assert_eq!(
        refs.target.as_ref().map(|t| t.ambiguous),
        Some(false),
        "the handle itself names one definition"
    );

    // A handle whose bare name has one definition discloses nothing.
    let (by_render_name, _) = rows("render");
    let (by_render_handle, refs) = rows("sym:src/render.rs#render");
    assert_eq!(by_render_handle, by_render_name);
    assert!(!by_render_handle.is_empty());
    assert_eq!(refs.definition_count, 1);
    assert!(!refs.ambiguous);
    assert_eq!(refs.note, None, "{:?}", refs.note);
    assert_eq!(
        refs.target.as_ref().map(|t| t.handles()),
        Some(vec!["sym:src/render.rs#render".to_string()])
    );
}

/// The disclosure quotes the name the rows are keyed on, which is the
/// caller's own spelling whenever the query used it: a qualified input keeps
/// `Foo::run`, and only an input the rows are not keyed on (a handle) is
/// reduced to the bare name its union shares.
#[test]
fn find_references_quotes_the_spelling_its_rows_are_keyed_on() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub mod a;\npub mod b;\n").unwrap();
    for module in ["a", "b"] {
        std::fs::write(
            root.join(format!("src/{module}.rs")),
            "pub struct Foo;\n\nimpl Foo {\n    pub fn run(&self) -> u32 {\n        1\n    }\n}\n",
        )
        .unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root, &db, &[], false).unwrap();

    let note = |spelling: &str| {
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: spelling.to_string(),
                kind: None,
            },
        )
        .unwrap();
        assert_eq!(refs.definition_count, 2, "{spelling}");
        refs.note.expect("two definitions are disclosed")
    };

    let qualified = note("Foo::run");
    assert!(
        qualified.contains("share the name 'Foo::run'"),
        "{qualified}"
    );
    assert!(
        qualified.contains("indistinguishable by qualified name"),
        "two `Foo::run` and no bare `run` share one qualified name: {qualified}"
    );
    let by_handle = note("sym:src/a.rs#Foo::run");
    assert!(by_handle.contains("share the name 'run'"), "{by_handle}");
}

#[test]
fn find_symbol_discloses_ambiguity_and_drops_module_declarations() {
    let (_dir, db) = project();

    let render = symbols(&db, "render");
    assert_eq!(
        render
            .results
            .iter()
            .map(|s| (s.file_path.as_str(), s.kind.as_str()))
            .collect::<Vec<_>>(),
        [("src/render.rs", "function")],
        "`mod render;` is a declaration, not a definition"
    );
    let target = render.target.expect("every row set carries its resolution");
    assert!(!target.ambiguous);
    assert_eq!(target.candidates_total, 1);

    let with_modules = find_symbol_with_options(
        &db,
        &FindSymbolRequest {
            name: "render".to_string(),
            kind: None,
        },
        &FindSymbolOptions {
            include_modules: true,
        },
    )
    .unwrap();
    assert_eq!(with_modules.results.len(), 2, "{:?}", with_modules.results);
    assert_eq!(with_modules.target.expect("resolution").candidates_total, 2);

    let search = symbols(&db, "search");
    let target = search.target.expect("resolution");
    assert!(target.ambiguous, "two definitions share the name");
    assert_eq!(target.candidates_total, 2);
    assert_eq!(search.results.len(), 2);

    // A handle names one of them, and a rename reports its nearest lead.
    let one = symbols(&db, "sym:src/search.rs#search");
    assert_eq!(one.results.len(), 1);
    assert!(!one.target.expect("resolution").ambiguous);

    let renamed = symbols(&db, "Search");
    assert!(renamed.results.is_empty(), "a guess is not a result");
    let target = renamed.target.expect("resolution");
    assert!(target.guessed());
    assert_eq!(target.candidates_total, 2);
}
