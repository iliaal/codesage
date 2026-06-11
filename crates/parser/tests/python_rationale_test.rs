use codesage_parser::extract::extract_symbols;
use codesage_parser::parse::parse_file;
use codesage_protocol::{Language, RationaleKind};

fn python_rationale_symbols() -> Vec<codesage_protocol::Symbol> {
    let path = format!(
        "{}/tests/fixtures/python_rationale.py",
        env!("CARGO_MANIFEST_DIR")
    );
    let source = std::fs::read(&path).unwrap();
    let tree = parse_file(&source, Language::Python).unwrap();
    extract_symbols(&tree, &source, Language::Python, "python_rationale.py").unwrap()
}

fn symbol<'a>(
    symbols: &'a [codesage_protocol::Symbol],
    name: &str,
) -> &'a codesage_protocol::Symbol {
    symbols.iter().find(|s| s.name == name).unwrap()
}

#[test]
fn extracts_python_rationale_from_comments_and_docstrings() {
    let symbols = python_rationale_symbols();

    let comment_todo = symbol(&symbols, "comment_todo");
    assert_eq!(comment_todo.rationale.len(), 1);
    assert_eq!(comment_todo.rationale[0].kind, RationaleKind::Todo);
    assert_eq!(comment_todo.rationale[0].text, "replace with async impl");

    let async_fixme = symbol(&symbols, "async_fixme");
    assert_eq!(async_fixme.rationale.len(), 1);
    assert_eq!(async_fixme.rationale[0].kind, RationaleKind::Fixme);
    assert_eq!(
        async_fixme.rationale[0].text,
        "race condition between read and write"
    );

    let docstring_why = symbol(&symbols, "docstring_why");
    assert_eq!(docstring_why.rationale.len(), 1);
    assert_eq!(docstring_why.rationale[0].kind, RationaleKind::Why);
    assert_eq!(docstring_why.rationale[0].text, "bypass cache for hot path");

    let docstring_note = symbol(&symbols, "DocstringNote");
    assert_eq!(docstring_note.rationale.len(), 1);
    assert_eq!(docstring_note.rationale[0].kind, RationaleKind::Note);
    assert_eq!(
        docstring_note.rationale[0].text,
        "thread-safe, see lock in __init__"
    );

    let todo_no_colon = symbol(&symbols, "TodoNoColon");
    assert_eq!(todo_no_colon.rationale.len(), 1);
    assert_eq!(todo_no_colon.rationale[0].kind, RationaleKind::Todo);
    assert_eq!(todo_no_colon.rationale[0].text, "write more tests");
}

#[test]
fn ignores_python_non_rationale_comments_and_string_literals() {
    let symbols = python_rationale_symbols();

    assert!(symbol(&symbols, "normal_comment").rationale.is_empty());
    assert!(
        symbol(&symbols, "string_literal_marker")
            .rationale
            .is_empty()
    );
}

#[test]
fn attaches_rationale_above_decorated_python_defs() {
    // Regression: a rationale comment above a `@decorator`-prefixed def
    // is a sibling of `decorated_definition`, not of the inner
    // `function_definition`. Walking prev_sibling from the inner node
    // hits the `decorator` (non-comment) and breaks before reaching the
    // comment. This pattern dominates real-world Python (`@app.route`,
    // `@property`, `@dataclass`, `@lru_cache`, `@staticmethod`) — every
    // decorated def would silently miss its rationale without the
    // wrapper-anchored walk.
    let symbols = python_rationale_symbols();

    let single = symbol(&symbols, "single_decorator_todo");
    assert_eq!(single.rationale.len(), 1);
    assert_eq!(single.rationale[0].kind, RationaleKind::Todo);
    assert_eq!(single.rationale[0].text, "refactor cache key");

    let stacked = symbol(&symbols, "stacked_decorator_fixme");
    assert_eq!(stacked.rationale.len(), 1);
    assert_eq!(stacked.rationale[0].kind, RationaleKind::Fixme);
    assert_eq!(stacked.rationale[0].text, "race on shared state");

    // Decorated methods inside a class body — same wrapper, different
    // parent (class block instead of module). Verifies the anchor
    // resolves correctly regardless of containing scope.
    let cached = symbol(&symbols, "cached_value");
    assert_eq!(cached.rationale.len(), 1);
    assert_eq!(cached.rationale[0].kind, RationaleKind::Note);
    assert_eq!(
        cached.rationale[0].text,
        "cached for the lifetime of the instance"
    );

    let registered = symbol(&symbols, "registered");
    assert_eq!(registered.rationale.len(), 1);
    assert_eq!(registered.rationale[0].kind, RationaleKind::Why);
    assert_eq!(
        registered.rationale[0].text,
        "must be a classmethod for the registry"
    );

    // Regression: rationale parked at class_definition level (sibling of the
    // body block, not inside it) must attach to the first method.
    let first_method = symbol(&symbols, "first_method");
    assert_eq!(first_method.rationale.len(), 1);
    assert_eq!(first_method.rationale[0].kind, RationaleKind::Why);
    assert_eq!(
        first_method.rationale[0].text,
        "header comment applies to the first method below"
    );
}
