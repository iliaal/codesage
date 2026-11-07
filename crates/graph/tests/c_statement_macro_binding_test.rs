use codesage_graph::full_index;
use codesage_protocol::{ReferenceKind, SymbolKind};
use codesage_storage::Database;

fn indexed(path: &str, source: &str) -> Database {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(path), source).unwrap();
    let db = Database::open_in_memory().unwrap();
    assert_eq!(
        full_index(root.path(), &db, &[], false)
            .unwrap()
            .files_indexed,
        1
    );
    db
}

#[test]
fn recovered_macro_and_following_call_belong_to_the_original_function() {
    let source = "#include \"m.h\"
int helper(int a) { return a + 1; }
int uses_assert(int c) {
    XXH_ASSERT(c)
    return helper(c);
}
";
    for path in ["a.c", "b.cpp"] {
        let db = indexed(path, source);
        let references = db.references_in_file_range(path, 1, 6).unwrap();
        let calls: Vec<_> = references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::Call)
            .map(|reference| {
                (
                    reference.to_name.as_str(),
                    reference.line,
                    reference.col,
                    reference.from_symbol.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            calls,
            [
                ("XXH_ASSERT", 4, 4, Some("uses_assert")),
                ("helper", 5, 11, Some("uses_assert")),
            ],
            "{path}"
        );
        let symbols = db.symbols_for_file(path).unwrap();
        let function = symbols
            .iter()
            .find(|symbol| symbol.name == "uses_assert")
            .unwrap();
        assert_eq!(function.kind, SymbolKind::Function);
        assert_eq!(
            (
                function.line_start,
                function.col_start,
                function.line_end,
                function.col_end
            ),
            (3, 0, 6, 1)
        );
    }
}

#[test]
fn recovery_keeps_ownership_of_initializer_argument_and_control_body_calls() {
    let source = "int recovered(int c) {
    ASSERT(c)
    return helper(c);
}
int unchanged(int c) {
    int values[] = {XXH_ASSERT(c), helper(c)};
    int value = WRAP(XXH_ASSERT(c), helper(c));
    OUTER(XXH_ASSERT(c), helper(c));
    if (c)
        XXH_ASSERT(c)
    return helper(c);
    for (;;)
        XXH_ASSERT(c)
    return helper(c);
    TYPE(int)
    local_variable;
}
";
    let db = indexed("control.c", source);
    let references = db.references_in_file_range("control.c", 1, 17).unwrap();
    let calls: Vec<_> = references
        .iter()
        .filter(|reference| reference.kind == ReferenceKind::Call)
        .map(|reference| {
            (
                reference.to_name.as_str(),
                reference.line,
                reference.col,
                reference.from_symbol.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        calls,
        [
            ("ASSERT", 2, 4, Some("recovered")),
            ("helper", 3, 11, Some("recovered")),
            ("XXH_ASSERT", 6, 20, Some("unchanged")),
            ("helper", 6, 35, Some("unchanged")),
            ("WRAP", 7, 16, Some("unchanged")),
            ("XXH_ASSERT", 7, 21, Some("unchanged")),
            ("helper", 7, 36, Some("unchanged")),
            ("OUTER", 8, 4, Some("unchanged")),
            ("XXH_ASSERT", 8, 10, Some("unchanged")),
            ("helper", 8, 25, Some("unchanged")),
            ("XXH_ASSERT", 10, 8, Some("unchanged")),
            ("helper", 11, 11, Some("unchanged")),
            ("XXH_ASSERT", 13, 8, Some("unchanged")),
            ("helper", 14, 11, Some("unchanged")),
        ]
    );
}
