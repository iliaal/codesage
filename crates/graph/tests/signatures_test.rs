use codesage_graph::{DescribeOptions, describe};
use codesage_protocol::{DescribeDetail, Visibility};
use codesage_storage::Database;

#[test]
fn indexed_signatures_survive_reopen_and_public_cards_disclose_limits() {
    let root = tempfile::tempdir().unwrap();
    for (path, source) in [
        (
            "api.c",
            "int *allocate(unsigned count) { return 0; }\nstatic int hidden(void) { return 1; }",
        ),
        (
            "api.php",
            "<?php class Api { function visible(string $key): ?array { return null; } private function hidden(): int { return 1; } protected function inherited() {} }",
        ),
        (
            "api.py",
            "def fetch(key: str) -> list[str]:\n    return []\n",
        ),
        (
            "api.rs",
            "pub fn fetch(key: &str) -> Option<String> { None }\nfn hidden() {}",
        ),
    ] {
        std::fs::write(root.path().join(path), source).unwrap();
    }
    let db_path = root.path().join("index.db");
    let db = Database::open(&db_path).unwrap();
    codesage_graph::full_index(root.path(), &db, &[], false).unwrap();
    drop(db);
    let db = Database::open_read_only(&db_path).unwrap();
    for (path, name, returns) in [
        ("api.c", "allocate", "int *"),
        ("api.php", "visible", "?array"),
        ("api.py", "fetch", "list[str]"),
        ("api.rs", "fetch", "Option<String>"),
    ] {
        let symbols = db.symbols_for_file(path).unwrap();
        let symbol = symbols.iter().find(|symbol| symbol.name == name).unwrap();
        let sig = symbol.signature.as_ref().unwrap();
        assert_eq!(sig.returns.as_deref(), Some(returns));
        assert_eq!(sig.parameters.len(), 1);
        let card = describe(
            root.path(),
            &db,
            &symbol.handle().to_string(),
            &DescribeOptions {
                sections: Some(vec!["identity".into()]),
                ..Default::default()
            },
        )
        .unwrap()
        .card
        .unwrap();
        assert_eq!(
            card.sections["identity"].data["signature"]["returns"],
            returns
        );
        let card = describe(
            root.path(),
            &db,
            &format!("file:{path}"),
            &DescribeOptions {
                sections: Some(vec!["public_api".into()]),
                detail: DescribeDetail::Full,
                ..Default::default()
            },
        )
        .unwrap()
        .card
        .unwrap();
        let api = &card.sections["public_api"].data;
        if path == "api.py" {
            assert_eq!(api["total"], 0);
            assert_eq!(api["unknown_visibility"], 1);
        } else {
            assert_eq!(api["total"], 1, "{path}: {api}");
            assert_eq!(api["symbols"][0]["handle"], symbol.handle().to_string());
            assert_eq!(api["symbols"][0]["signature"]["returns"], returns);
        }
        assert_eq!(api["truncated"], false);
    }
    let php = db.symbols_for_file("api.php").unwrap();
    assert_eq!(
        php.iter()
            .find(|symbol| symbol.name == "hidden")
            .unwrap()
            .visibility,
        Some(Visibility::Private)
    );
}

#[test]
fn public_api_is_bounded_without_losing_its_total() {
    let root = tempfile::tempdir().unwrap();
    let source: String = (0..25)
        .map(|n| format!("pub fn item_{n}(value: u32) -> u32 {{ value }}\n"))
        .collect();
    std::fs::write(root.path().join("api.rs"), source).unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root.path(), &db, &[], false).unwrap();
    let card = describe(
        root.path(),
        &db,
        "file:api.rs",
        &DescribeOptions {
            sections: Some(vec!["public_api".into()]),
            ..Default::default()
        },
    )
    .unwrap()
    .card
    .unwrap();
    let api = &card.sections["public_api"].data;
    assert_eq!(api["total"], 25);
    assert!(api["symbols"].as_array().unwrap().len() < 25);
    assert_eq!(api["truncated"], true);
    assert!(
        api["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .all(|symbol| symbol["signature"]["returns"] == "u32")
    );
}

#[test]
fn public_overloads_keep_distinct_handles_and_signatures() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("Api.java"), "class Api {\n public static int fetch(int value) { return value; }\n public static int fetch(int value, int extra) { return value + extra; }\n}").unwrap();
    let db = Database::open_in_memory().unwrap();
    codesage_graph::full_index(root.path(), &db, &[], false).unwrap();
    let card = describe(
        root.path(),
        &db,
        "file:Api.java",
        &DescribeOptions {
            sections: Some(vec!["public_api".into()]),
            detail: DescribeDetail::Full,
            ..Default::default()
        },
    )
    .unwrap()
    .card
    .unwrap();
    let api = &card.sections["public_api"].data;
    assert_eq!(api["total"], 2);
    let symbols = api["symbols"].as_array().unwrap();
    assert_ne!(symbols[0]["handle"], symbols[1]["handle"]);
    assert_eq!(symbols[0]["signature"]["arity"]["minimum"], 1);
    assert_eq!(symbols[1]["signature"]["arity"]["minimum"], 2);
}

#[test]
fn legacy_read_only_symbols_remain_available_without_migrating() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("api.rs"),
        "pub fn fetch(key: &str) -> usize { key.len() }",
    )
    .unwrap();
    let path = root.path().join("index.db");
    let db = Database::open(&path).unwrap();
    codesage_graph::full_index(root.path(), &db, &[], false).unwrap();
    db.execute_raw_for_tests("ALTER TABLE symbols DROP COLUMN signature; ALTER TABLE symbols DROP COLUMN arity; ALTER TABLE symbols DROP COLUMN parameters; ALTER TABLE symbols DROP COLUMN return_type; DELETE FROM schema_migrations WHERE name = '0027_symbol_signatures';").unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    let db = Database::open_read_only(&path).unwrap();
    let symbols = db.find_symbols("fetch", None).unwrap();
    assert_eq!(symbols.len(), 1);
    assert!(symbols[0].signature.is_none());
    assert!(
        db.symbols_for_file("api.rs").unwrap()[0]
            .signature
            .is_none()
    );
    assert!(
        db.symbols_for_files(&["api.rs".into()]).unwrap()["api.rs"][0]
            .signature
            .is_none()
    );
    let card = describe(
        root.path(),
        &db,
        &symbols[0].handle().to_string(),
        &DescribeOptions {
            sections: Some(vec!["identity".into()]),
            ..Default::default()
        },
    )
    .unwrap()
    .card
    .unwrap();
    assert!(card.sections["identity"].data["signature"].is_null());
    drop(db);
    assert_eq!(std::fs::read(path).unwrap(), before);
}
