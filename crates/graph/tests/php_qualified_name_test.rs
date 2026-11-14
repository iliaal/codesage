use codesage_graph::{find_references, full_index, impact_analysis};
use codesage_protocol::{FindReferencesRequest, ImpactRequest, ImpactTarget};
use codesage_storage::Database;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let root = tempfile::tempdir().unwrap();
    for (path, source) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    let stats = full_index(root.path(), &db, &[], false).unwrap();
    assert_eq!(stats.files_indexed, files.len(), "{stats:?}");
    (root, db)
}

fn impacted(db: &Database, target: &str) -> Vec<String> {
    let mut files: Vec<String> = impact_analysis(
        db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: target.into(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap()
    .into_iter()
    .map(|entry| entry.file_path)
    .collect();
    files.sort();
    files
}

fn targets(db: &Database, name: &str) -> Vec<(String, String, Option<String>)> {
    let mut rows: Vec<_> = find_references(
        db,
        &FindReferencesRequest {
            symbol_name: name.into(),
            kind: None,
        },
    )
    .unwrap()
    .results
    .into_iter()
    .map(|row| (row.from_file, row.to_name, row.to))
    .collect();
    rows.sort();
    rows.dedup();
    rows
}

const FOO: (&str, &str) = (
    "A/Foo.php",
    "<?php\nnamespace App\\A;\nclass Foo { public static function build() { return 1; } }\n",
);

#[test]
fn qualified_class_references_bind_to_the_fully_qualified_class() {
    let (_root, db) = project(&[
        FOO,
        (
            "Gate.php",
            "<?php\nnamespace App;\nclass Gate { public function check() { $a = new \\App\\A\\Foo(); $b = new A\\Foo(); return \\App\\A\\Foo::build(); } }\n",
        ),
        (
            "Aliased.php",
            "<?php\nnamespace Web;\nuse App\\A as Alpha;\nclass Aliased extends Alpha\\Foo { public function k() { return Alpha\\Foo::class; } }\n",
        ),
        (
            "Elsewhere.php",
            "<?php\nnamespace Web;\nclass Elsewhere { public function run() { return new A\\Foo() ?? new \\Other\\Foo(); } }\n",
        ),
    ]);
    assert_eq!(impacted(&db, "App\\A\\Foo"), ["Aliased.php", "Gate.php"]);
    let handle = Some("sym:A/Foo.php#App\\A\\Foo".to_string());
    assert_eq!(
        targets(&db, "App\\A\\Foo"),
        [
            ("Aliased.php".into(), "App\\A\\Foo".into(), handle.clone()),
            ("Gate.php".into(), "App\\A\\Foo".into(), handle),
        ]
    );
    // `A\Foo` in namespace Web is `Web\A\Foo`, and `\Other\Foo` is exactly
    // that class: neither is indexed, so neither binds to the `Foo` that is.
    let unbound: Vec<_> = targets(&db, "Foo")
        .into_iter()
        .filter(|(file, _, _)| file == "Elsewhere.php")
        .collect();
    assert_eq!(
        unbound,
        [
            ("Elsewhere.php".into(), "Other\\Foo".into(), None),
            ("Elsewhere.php".into(), "Web\\A\\Foo".into(), None),
        ]
    );
}

#[test]
fn leading_backslash_single_segment_names_only_the_global_class() {
    let (_root, db) = project(&[
        (
            "Legacy.php",
            "<?php\nclass Legacy { public static function make() { return 1; } }\n",
        ),
        (
            "Exceptions/Exception.php",
            "<?php\nnamespace App\\Exceptions;\nclass Exception {}\n",
        ),
        (
            "Caller.php",
            "<?php\nnamespace App;\nclass Caller { public function run() { \\Legacy::make(); throw new \\Exception(); } }\n",
        ),
    ]);
    assert_eq!(impacted(&db, "Legacy"), ["Caller.php"]);
    assert!(
        impacted(&db, "App\\Exceptions\\Exception").is_empty(),
        "`\\Exception` names the global class, not App\\Exceptions\\Exception"
    );
    assert_eq!(
        targets(&db, "Exception"),
        [("Caller.php".into(), "\\Exception".into(), None)]
    );
}

#[test]
fn qualified_class_names_bind_case_insensitively() {
    let (_root, db) = project(&[
        (
            "Lib/Models.php",
            "<?php\nnamespace Lib\\Models;\nclass User {}\n",
        ),
        ("Legacy.php", "<?php\nclass Legacy {}\n"),
        (
            "App/Mixed.php",
            "<?php\nnamespace App;\nuse LIB\\models as M;\nclass Mixed { public function a() { new \\lib\\models\\USER(); new m\\user(); return \\LEGACY::class; } }\n",
        ),
    ]);
    assert_eq!(impacted(&db, "Lib\\Models\\User"), ["App/Mixed.php"]);
    assert_eq!(impacted(&db, "Legacy"), ["App/Mixed.php"]);
    let user = Some("sym:Lib/Models.php#Lib\\Models\\User".to_string());
    assert_eq!(
        targets(&db, "Lib\\Models\\User"),
        [
            (
                "App/Mixed.php".into(),
                "LIB\\models\\user".into(),
                user.clone()
            ),
            ("App/Mixed.php".into(), "lib\\models\\USER".into(), user),
        ]
    );
    assert_eq!(
        targets(&db, "User")
            .into_iter()
            .filter(|(file, _, _)| file == "App/Mixed.php")
            .count(),
        2
    );
    assert_eq!(
        targets(&db, "sym:Legacy.php#Legacy"),
        [(
            "App/Mixed.php".into(),
            "\\LEGACY".into(),
            Some("sym:Legacy.php#Legacy".to_string())
        )]
    );
}
