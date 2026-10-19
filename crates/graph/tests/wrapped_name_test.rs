use codesage_graph::{full_index, incremental_index};
use codesage_protocol::{ReferenceKind, SymbolKind};
use codesage_storage::Database;

fn source(cpp: bool, first: &str, second: &str) -> String {
    let declaration = if cpp {
        format!("class Owner {{ public: void {first}(int); void {second}(void); }};\n")
    } else {
        String::new()
    };
    let scope = if cpp { "Owner::" } else { "" };
    format!(
        "#define FNAME(name) name\nvoid sink(void) {{}}\n{declaration}void {scope}FNAME({first})(int value) {{\n    sink();\n}}\nvoid {scope}FNAME({second})(void) {{ sink(); }}\n"
    )
}

fn assert_callers(db: &Database, expected: &[&str]) {
    let references = db
        .find_references("sink", Some(ReferenceKind::Call))
        .unwrap();
    assert_eq!(references.len(), expected.len());
    assert_eq!(
        references
            .iter()
            .map(|reference| reference.from_symbol.as_deref())
            .collect::<Vec<_>>(),
        expected.iter().map(|name| Some(*name)).collect::<Vec<_>>()
    );
}

#[test]
fn wrapped_definitions_and_body_callers_match_after_incremental_and_fresh_indexing() {
    for (file, cpp) in [("api.c", false), ("api.cpp", true)] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(file), source(cpp, "first", "second")).unwrap();
        let db = Database::open_in_memory().unwrap();
        let indexed = full_index(root.path(), &db, &[], false).unwrap();
        assert_eq!(indexed.files_indexed, 1);
        let expected = if cpp {
            ["Owner::first", "Owner::second"]
        } else {
            ["first", "second"]
        };
        assert_callers(&db, &expected);
        let symbols = db.symbols_for_file(file).unwrap();
        for name in ["first", "second"] {
            let definitions: Vec<_> = symbols
                .iter()
                .filter(|symbol| symbol.name == name)
                .collect();
            assert_eq!(definitions.len(), if cpp { 2 } else { 1 });
            assert!(definitions.iter().all(|symbol| symbol.kind
                == if cpp {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                }));
        }

        std::fs::write(
            root.path().join(file),
            source(cpp, "changed_first", "changed_second"),
        )
        .unwrap();
        let updated = incremental_index(root.path(), &db, &[], false).unwrap();
        assert_eq!(updated.files_indexed, 1);
        let expected = if cpp {
            ["Owner::changed_first", "Owner::changed_second"]
        } else {
            ["changed_first", "changed_second"]
        };
        assert_callers(&db, &expected);
        let fresh = Database::open_in_memory().unwrap();
        full_index(root.path(), &fresh, &[], false).unwrap();
        assert_eq!(
            db.symbols_for_file(file).unwrap(),
            fresh.symbols_for_file(file).unwrap()
        );
        assert_eq!(
            db.find_references("sink", Some(ReferenceKind::Call))
                .unwrap(),
            fresh
                .find_references("sink", Some(ReferenceKind::Call))
                .unwrap()
        );
        assert!(
            !db.symbols_for_file(file)
                .unwrap()
                .iter()
                .any(|symbol| symbol.name == "first" || symbol.name == "second")
        );

        std::fs::write(
            root.path().join(file),
            source(cpp, "changed_first", "changed_second").replacen(
                "#define FNAME(name) name",
                "#define FNAME(name) rewritten_##name",
                1,
            ),
        )
        .unwrap();
        incremental_index(root.path(), &db, &[], false).unwrap();
        let references = db
            .find_references("sink", Some(ReferenceKind::Call))
            .unwrap();
        assert_eq!(references.len(), 2);
        assert!(
            references
                .iter()
                .all(|reference| reference.from_symbol.is_none())
        );
        let fresh = Database::open_in_memory().unwrap();
        full_index(root.path(), &fresh, &[], false).unwrap();
        assert_eq!(
            db.symbols_for_file(file).unwrap(),
            fresh.symbols_for_file(file).unwrap()
        );
        assert_eq!(
            references,
            fresh
                .find_references("sink", Some(ReferenceKind::Call))
                .unwrap()
        );
    }
}

#[test]
fn wrapped_qualified_definition_trivia_preserves_body_ownership() {
    for wrapper in [
        "example::Owner :: FNAME",
        "example::Owner:: /* why */ FNAME",
        "example::Owner::\\\nFNAME",
        "example /* outer */ :: Owner /* inner */ :: FNAME",
    ] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("api.cpp"), format!(
            "#define FNAME(name) name\nvoid sink(void) {{}}\nnamespace example {{ class Owner {{ public: void first(); }}; }}\nvoid {wrapper}(first)() {{ sink(); }}\n"
        )).unwrap();
        let db = Database::open_in_memory().unwrap();
        full_index(root.path(), &db, &[], false).unwrap();
        assert_callers(&db, &["example::Owner::first"]);
    }
}
