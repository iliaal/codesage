use codesage_graph::full_index;
use codesage_protocol::ReferenceKind;
use codesage_storage::Database;

fn indexed(source_path: &str, source: &str) -> Database {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(source_path), source).unwrap();
    let db = Database::open_in_memory().unwrap();
    assert_eq!(
        full_index(dir.path(), &db, &[], false)
            .unwrap()
            .files_indexed,
        1
    );
    db
}

fn assert_caller(db: &Database, target: &str, caller: Option<&str>) {
    let references = db
        .find_references(target, Some(ReferenceKind::Call))
        .unwrap();
    assert_eq!(references.len(), 1, "{target}: {references:?}");
    assert_eq!(references[0].from_symbol.as_deref(), caller, "{target}");
}

#[test]
fn same_line_c_siblings_have_distinct_callers() {
    let db = indexed(
        "app.c",
        "int alpha(void) { return first(); } int beta(void) { return second(); }\n",
    );
    assert_caller(&db, "first", Some("alpha"));
    assert_caller(&db, "second", Some("beta"));
}

#[test]
fn same_line_nested_functions_choose_innermost_caller() {
    let db = indexed(
        "app.js",
        "function outer() { before(); function inner() { inside(); } after(); }\n",
    );
    assert_caller(&db, "before", Some("outer"));
    assert_caller(&db, "inside", Some("inner"));
    assert_caller(&db, "after", Some("outer"));
}

#[test]
fn calls_before_and_after_same_line_function_remain_file_scope() {
    let db = indexed(
        "app.js",
        "before(); function owner() { inside(); }after();\n",
    );
    assert_caller(&db, "before", None);
    assert_caller(&db, "inside", Some("owner"));
    assert_caller(&db, "after", None);
}

#[test]
fn multiline_nested_functions_and_file_scope_keep_their_callers() {
    let db = indexed(
        "app.py",
        "before()\ndef outer():\n    def inner():\n        inside()\n    outside()\nafter()\n",
    );
    assert_caller(&db, "before", None);
    assert_caller(&db, "inside", Some("inner"));
    assert_caller(&db, "outside", Some("outer"));
    assert_caller(&db, "after", None);
}

#[test]
fn unicode_prefix_preserves_same_line_ownership() {
    let db = indexed(
        "app.js",
        "/* 日本語 🦀 */ function alpha() { first(); } function beta() { second(); }tail();\n",
    );
    assert_caller(&db, "first", Some("alpha"));
    assert_caller(&db, "second", Some("beta"));
    assert_caller(&db, "tail", None);
}
