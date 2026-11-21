use codesage_graph::{find_references, full_index, impact_analysis, list_dependencies};
use codesage_protocol::{FindReferencesRequest, ImpactRequest, ImpactTarget};
use codesage_storage::Database;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let root = tempfile::tempdir().unwrap();
    for (path, source) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    std::fs::create_dir(root.path().join(".codesage")).unwrap();
    let db = Database::open(&root.path().join(".codesage/index.db")).unwrap();
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

fn instantiated_from(db: &Database, target: &str) -> Vec<String> {
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
    .filter(|entry| {
        entry
            .reasons
            .iter()
            .any(|reason| reason.kind == codesage_protocol::ReferenceKind::Instantiation)
    })
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

#[test]
fn bare_class_names_bind_by_use_or_caller_namespace_not_by_uniqueness() {
    let (_root, db) = project(&[
        (
            "A/Foo.php",
            "<?php\nnamespace App\\A;\nclass Foo { public function __construct() {} }\n",
        ),
        (
            "B/Caller.php",
            "<?php\nnamespace App\\B;\nclass Caller { public function go() { return new Foo(); } }\n",
        ),
        (
            "A/Sibling.php",
            "<?php\nnamespace App\\A;\nclass Sibling extends Foo { public function go() { return new Foo(); } }\n",
        ),
        (
            "Ctl/UsesFoo.php",
            "<?php\nnamespace App\\Ctl;\nuse App\\A\\Foo;\nclass UsesFoo { public function go() { return new Foo(); } }\n",
        ),
        (
            "Ctl/Other.php",
            "<?php\nnamespace App\\A;\nuse Vendor\\Foo;\nclass Other { public function go() { return new Foo(); } }\n",
        ),
    ]);
    assert_eq!(
        impacted(&db, "App\\A\\Foo"),
        ["A/Sibling.php", "Ctl/UsesFoo.php"]
    );
    assert_eq!(
        impacted(&db, "sym:A/Foo.php#App\\A\\Foo\\__construct"),
        ["A/Sibling.php", "Ctl/UsesFoo.php"]
    );
}

#[test]
fn a_global_use_names_the_global_class_over_a_namespaced_one() {
    let (_root, db) = project(&[
        (
            "Testing/InvalidArgumentException.php",
            "<?php\nnamespace App\\Testing;\nclass InvalidArgumentException extends \\Exception {}\n",
        ),
        (
            "Auth/Gate.php",
            "<?php\nnamespace App\\Auth;\nuse InvalidArgumentException;\nclass Gate { public function check() { throw new InvalidArgumentException('x'); } }\n",
        ),
        (
            "Testing/Local.php",
            "<?php\nnamespace App\\Testing;\nuse InvalidArgumentException;\nclass Local { public function check() { throw new InvalidArgumentException('x'); } }\n",
        ),
        (
            "Testing/Plain.php",
            "<?php\nnamespace App\\Testing;\nclass Plain { public function check() { throw new InvalidArgumentException('x'); } }\n",
        ),
    ]);
    assert_eq!(
        impacted(&db, "App\\Testing\\InvalidArgumentException"),
        ["Testing/Plain.php"]
    );
    assert_eq!(
        targets(&db, "\\InvalidArgumentException"),
        [
            (
                "Auth/Gate.php".into(),
                "\\InvalidArgumentException".into(),
                None
            ),
            (
                "Testing/Local.php".into(),
                "\\InvalidArgumentException".into(),
                None
            ),
        ]
    );
}

#[test]
fn same_named_classes_resolve_to_the_one_the_caller_namespace_names() {
    let (_root, db) = project(&[
        ("A/Foo.php", "<?php\nnamespace App\\A;\nclass Foo {}\n"),
        ("B/Foo.php", "<?php\nnamespace App\\B;\nclass Foo {}\n"),
        (
            "B/Caller.php",
            "<?php\nnamespace App\\B;\nclass Caller { public function go() { return new Foo(); } }\n",
        ),
    ]);
    assert!(impacted(&db, "App\\A\\Foo").is_empty());
    assert_eq!(impacted(&db, "App\\B\\Foo"), ["B/Caller.php"]);
}

#[test]
fn a_renamed_use_leaves_its_last_segment_to_the_caller_namespace() {
    let (_root, db) = project(&[
        (
            "Fs/LocalFilesystemAdapter.php",
            "<?php\nnamespace Illuminate\\Filesystem;\nclass LocalFilesystemAdapter {}\n",
        ),
        (
            "Fs/Manager.php",
            "<?php\nnamespace Illuminate\\Filesystem;\nuse League\\Local\\LocalFilesystemAdapter as LocalAdapter;\nclass Manager { public function a() { return new LocalFilesystemAdapter(new LocalAdapter()); } }\n",
        ),
        (
            "Fs/Plain.php",
            "<?php\nnamespace Illuminate\\Filesystem;\nuse League\\Local\\LocalFilesystemAdapter;\nclass Plain { public function a() { return new LocalFilesystemAdapter(); } }\n",
        ),
    ]);
    assert_eq!(
        impacted(&db, "Illuminate\\Filesystem\\LocalFilesystemAdapter"),
        ["Fs/Manager.php"]
    );
}

#[test]
fn a_renaming_use_overrides_a_same_namespace_class() {
    let (_root, db) = project(&[
        (
            "Queue/QueueManager.php",
            "<?php\nnamespace Illuminate\\Queue;\nclass QueueManager {}\n",
        ),
        (
            "Queue/Worker.php",
            "<?php\nnamespace Illuminate\\Queue;\nuse Illuminate\\Contracts\\Queue\\Factory as QueueManager;\nclass Worker { public function __construct(QueueManager $manager) {} }\n",
        ),
        (
            "Queue/Listener.php",
            "<?php\nnamespace Illuminate\\Queue;\nclass Listener { public function __construct(QueueManager $manager) {} }\n",
        ),
    ]);
    assert_eq!(
        impacted(&db, "Illuminate\\Queue\\QueueManager"),
        ["Queue/Listener.php"]
    );
}

#[test]
fn a_file_without_definitions_resolves_in_the_namespace_its_source_declares() {
    let (_root, db) = project(&[
        ("Legacy.php", "<?php\nclass Legacy {}\n"),
        FOO,
        ("bin/script.php", "<?php\nnew Legacy();\nnew Foo();\n"),
        (
            "bin/namespaced.php",
            "<?php\nnamespace App\\B;\nnew Legacy();\nnew Foo();\n",
        ),
        (
            "bin/imports.php",
            "<?php\nnamespace App\\B;\nuse App\\A\\Foo;\nnew Foo();\n",
        ),
    ]);
    assert_eq!(impacted(&db, "Legacy"), ["bin/script.php"]);
    assert_eq!(impacted(&db, "App\\A\\Foo"), ["bin/imports.php"]);
}

#[test]
fn a_renamed_or_function_use_does_not_decide_the_name_it_renames_away_from() {
    let (_root, db) = project(&[
        FOO,
        (
            "N/RenamedUse.php",
            "<?php\nnamespace App\\N;\nuse App\\A\\Foo as F;\nclass RenamedUse { public function go() { return new Foo(); } }\n",
        ),
        (
            "C/UseFn.php",
            "<?php\nnamespace App\\C;\nuse function App\\A\\Foo;\nclass UseFn { public function go() { return new Foo(); } }\n",
        ),
        ("X/Foo.php", "<?php\nnamespace X;\nclass Foo {}\n"),
        (
            "N/Foo.php",
            "<?php\nnamespace N;\nuse X\\Foo as Base;\nclass Foo extends Base { public function m() { return new Foo(); } }\n",
        ),
        (
            "N/Uses.php",
            "<?php\nnamespace N;\nuse X\\Foo as Base;\nclass Uses { public function m() { return new Foo(); } }\n",
        ),
    ]);
    assert!(instantiated_from(&db, "App\\A\\Foo").is_empty());
    assert!(instantiated_from(&db, "X\\Foo").is_empty());
    assert_eq!(instantiated_from(&db, "N\\Foo"), ["N/Uses.php"]);
}

#[test]
fn a_caller_truncated_after_indexing_resolves_as_indexed() {
    let (root, db) = project(&[
        (
            "A/VeryLongClassNameHere.php",
            "<?php\nnamespace App\\A;\nclass VeryLongClassNameHere {}\n",
        ),
        (
            "A/Stale.php",
            "<?php\nnamespace App\\A;\nclass Stale { public function go() { return new VeryLongClassNameHere(); } }\n",
        ),
    ]);
    let before = impacted(&db, "App\\A\\VeryLongClassNameHere");
    assert_eq!(before, ["A/Stale.php"]);
    let rows = targets(&db, "VeryLongClassNameHere");
    std::fs::write(root.path().join("A/Stale.php"), "<?php").unwrap();
    assert_eq!(impacted(&db, "App\\A\\VeryLongClassNameHere"), before);
    assert_eq!(targets(&db, "VeryLongClassNameHere"), rows);
}

#[test]
fn a_file_indexed_without_php_name_rows_falls_back_to_import_rows_and_definitions() {
    let (_root, db) = project(&[
        FOO,
        (
            "A/Sibling.php",
            "<?php\nnamespace App\\A;\nclass Sibling { public function go() { return new Foo(); } }\n",
        ),
        (
            "B/Caller.php",
            "<?php\nnamespace App\\B;\nclass Caller { public function go() { return new Foo(); } }\n",
        ),
        (
            "Ctl/UsesFoo.php",
            "<?php\nnamespace App\\Ctl;\nuse App\\A\\Foo;\nclass UsesFoo { public function go() { return new Foo(); } }\n",
        ),
    ]);
    db.execute_raw_for_tests("DELETE FROM php_names;").unwrap();
    assert_eq!(
        impacted(&db, "App\\A\\Foo"),
        ["A/Sibling.php", "Ctl/UsesFoo.php"]
    );
}

#[test]
fn a_global_use_is_a_dependency_on_the_indexed_global_class() {
    let (_root, db) = project(&[
        (
            "Exc.php",
            "<?php\nclass InvalidArgumentException extends Exception {}\n",
        ),
        (
            "B/Clos.php",
            "<?php\nnamespace App\\B;\nuse InvalidArgumentException;\nclass Clos { public function go() { throw new InvalidArgumentException(); } }\n",
        ),
    ]);
    let caller = list_dependencies(&db, "B/Clos.php").unwrap();
    assert_eq!(caller.imports, ["file:Exc.php"]);
    assert!(caller.unresolved_imports.is_empty(), "{caller:?}");
    assert_eq!(
        list_dependencies(&db, "Exc.php").unwrap().imported_by,
        ["B/Clos.php"]
    );
}

#[test]
fn a_class_sharing_its_name_with_a_function_or_constant_keeps_both_ambiguous() {
    let callers = [
        (
            "X/Caller.php",
            "<?php\nnamespace App\\X;\nclass Caller { public function go() { Foo::build(); return new Foo(); } }\n",
        ),
        (
            "Y/UsesIt.php",
            "<?php\nnamespace App\\Y;\nuse App\\A\\Foo;\nclass UsesIt { public function go() { return new Foo(); } }\n",
        ),
    ];
    for (other, handle) in [
        (
            ("helpers.php", "<?php\nfunction Foo() { return 1; }\n"),
            "sym:helpers.php#Foo",
        ),
        (
            (
                "Enums/Store.php",
                "<?php\nnamespace App\\Enums;\nenum Store { case Foo; }\n",
            ),
            "sym:Enums/Store.php#App\\Enums\\Store\\Foo",
        ),
    ] {
        let (_root, db) = project(&[FOO, other, callers[0], callers[1]]);
        assert!(impacted(&db, handle).is_empty(), "{handle}");
        assert_eq!(instantiated_from(&db, "App\\A\\Foo"), ["Y/UsesIt.php"]);
    }
}

#[test]
fn a_file_rewritten_under_another_interpretation_ignores_its_php_name_rows() {
    let (_root, db) = project(&[
        FOO,
        (
            "N1/UseLead.php",
            "<?php\nnamespace App\\N1;\nuse App\\A\\Foo;\nclass UseLead { public function go() { return new Foo(); } }\n",
        ),
    ]);
    assert_eq!(instantiated_from(&db, "App\\A\\Foo"), ["N1/UseLead.php"]);
    db.execute_raw_for_tests(
        "DELETE FROM refs WHERE kind = 'import';
         UPDATE files SET interpretation = 'older' WHERE path = 'N1/UseLead.php';",
    )
    .unwrap();
    assert!(instantiated_from(&db, "App\\A\\Foo").is_empty());
}

#[test]
fn a_lowercase_global_use_lists_the_importer_under_the_class_file() {
    let (_root, db) = project(&[
        (
            "Exc.php",
            "<?php\nclass InvalidArgumentException extends Exception {}\n",
        ),
        (
            "B/Lower.php",
            "<?php\nnamespace App\\B;\nuse invalidargumentexception;\nclass Lower {}\n",
        ),
    ]);
    assert_eq!(
        list_dependencies(&db, "B/Lower.php").unwrap().imports,
        ["file:Exc.php"]
    );
    assert_eq!(
        list_dependencies(&db, "Exc.php").unwrap().imported_by,
        ["B/Lower.php"]
    );
}
