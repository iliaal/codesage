use codesage_graph::{find_references, full_index, impact_analysis};
use codesage_protocol::{
    FindReferencesRequest, ImpactRequest, ImpactTarget, Reference, ReferenceKind,
};
use codesage_storage::Database;

const MODELS: &str = "export class A { constructor() {} }\nexport class B extends A {}\nexport class C extends A { constructor() {} }\n";
const JS_CALLS: &str = "import { A, B, C } from './models.js';\nfunction run() {\n    new A();\n    new B();\n    new C();\n}\n";
const TS_CALLS: &str = "import { A, B, C } from './models.ts';\nfunction run() {\n    new A();\n    new B();\n    new C();\n}\n";

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let root = tempfile::tempdir().unwrap();
    for (file, source) in files {
        std::fs::write(root.path().join(file), source).unwrap();
    }
    std::fs::create_dir(root.path().join(".codesage")).unwrap();
    let db = Database::open(&root.path().join(".codesage/index.db")).unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    (root, db)
}

fn references(db: &Database, handle: &str) -> Vec<Reference> {
    find_references(
        db,
        &FindReferencesRequest {
            symbol_name: handle.to_string(),
            kind: Some(ReferenceKind::Instantiation),
        },
    )
    .unwrap()
    .results
}

fn impact_calls(db: &Database, handle: &str) -> Vec<(String, u32)> {
    let mut calls: Vec<_> = impact_analysis(
        db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: handle.to_string(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap()
    .into_iter()
    .flat_map(|entry| {
        entry
            .reasons
            .into_iter()
            .map(move |reason| (entry.file_path.clone(), reason.line))
    })
    .collect();
    calls.sort();
    calls
}

#[test]
fn named_import_distinguishes_a_class_from_a_same_named_constructor_function() {
    for extension in ["js", "ts"] {
        let class_file = format!("class.{extension}");
        let function_file = format!("factory.{extension}");
        let caller = format!(
            "import {{ A }} from './factory.{extension}';\nfunction run() {{ new A(); }}\n"
        );
        let real =
            format!("import {{ A }} from './class.{extension}';\nfunction run() {{ new A(); }}\n");
        let (_root, db) = project(&[
            (&class_file, "export class A { constructor() {} }"),
            (&function_file, "export function A() {}"),
            ("use.js", &caller),
            ("real.js", &real),
        ]);
        let target = format!("sym:{class_file}#A.constructor");
        let rows = references(&db, &target);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].from_file, "real.js");
        assert_eq!(rows[0].to.as_deref(), Some(target.as_str()));
        assert_eq!(impact_calls(&db, &target), [("real.js".into(), 2)]);
        assert_eq!(
            db.find_references("A", Some(ReferenceKind::Instantiation))
                .unwrap()
                .len(),
            2
        );
    }
}

#[test]
fn describe_discloses_constructor_source_limits_in_both_sections() {
    use codesage_graph::{DescribeOptions, describe};
    use codesage_protocol::DescribeDetail;
    let oversized = format!(
        "export class A {{ constructor() {{}} }}\n/*{}*/\n",
        "x".repeat(1024 * 1024)
    );
    let (root, db) = project(&[
        ("models.js", &oversized),
        ("other.ts", "export class A { constructor() {} }"),
        (
            "use.js",
            "import { A } from './models.js';\nfunction run() { new A(); }\n",
        ),
    ]);
    let options = DescribeOptions {
        detail: DescribeDetail::Full,
        sections: Some(vec!["references".into(), "callees".into()]),
        ..Default::default()
    };
    for (target, sections) in [
        ("sym:use.js#run", vec!["callees"]),
        ("sym:models.js#A.constructor", vec!["references", "callees"]),
    ] {
        let card = describe(root.path(), &db, target, &options)
            .unwrap()
            .card
            .unwrap();
        for section in sections {
            let incomplete = card.sections[section].completeness.as_ref().unwrap();
            assert_eq!(incomplete.kind, "bounded");
            assert_eq!(incomplete.reason, "reference_resolution_limit");
        }
    }
}

#[test]
fn named_relative_imports_select_own_constructors_among_js_ts_namesakes() {
    for prefix in ["", "/* é🦀 */ ", "'é🦀'; "] {
        let js_calls = JS_CALLS.replace("new ", &format!("{prefix}new "));
        let ts_calls = TS_CALLS.replace("new ", &format!("{prefix}new "));
        let (_root, db) = project(&[
            ("models.js", MODELS),
            ("models.ts", MODELS),
            ("use.js", &js_calls),
            ("use.ts", &ts_calls),
        ]);
        for _ in 0..2 {
            for extension in ["js", "ts"] {
                for (class, line) in [("A", 3), ("C", 5)] {
                    let handle = format!("sym:models.{extension}#{class}.constructor");
                    let expected = [(format!("use.{extension}"), line)];
                    let rows = references(&db, &handle);
                    assert_eq!(rows.len(), 4, "constructor union: {handle}: {rows:?}");
                    let rows: Vec<_> = rows
                        .into_iter()
                        .filter(|row| row.to.as_deref() == Some(handle.as_str()))
                        .collect();
                    assert_eq!(rows.len(), 1, "{handle}: {rows:?}");
                    assert_eq!(rows[0].to.as_deref(), Some(handle.as_str()));
                    assert_eq!(rows[0].from_symbol.as_deref(), Some("run"));
                    assert_eq!(
                        rows.into_iter()
                            .map(|row| (row.from_file, row.line))
                            .collect::<Vec<_>>(),
                        expected,
                        "{handle}"
                    );
                    assert_eq!(impact_calls(&db, &handle), expected, "{handle}");
                }
                assert!(
                    db.symbols_for_file(&format!("models.{extension}"))
                        .unwrap()
                        .iter()
                        .all(|symbol| symbol.qualified_name != "B.constructor")
                );
                for class in ["A", "B", "C"] {
                    let rows = references(&db, &format!("sym:models.{extension}#{class}"));
                    assert_eq!(rows.len(), 2, "class {class}: {rows:?}");
                    assert!(rows.iter().all(|row| row.to_name == class));
                }
            }
        }
        for class in ["A", "B", "C"] {
            assert_eq!(
                db.find_references(class, Some(ReferenceKind::Instantiation))
                    .unwrap()
                    .len(),
                2
            );
        }
    }
}

#[test]
fn shadowed_reassigned_type_only_and_unbound_names_stay_ambiguous() {
    for (file, caller) in [
        (
            "use.js",
            "import './left.js'; import './right.js'; function run() { new A(); }",
        ),
        (
            "use.js",
            "import { A } from './left.js'; import './right.js'; function run(A) { new A(); }",
        ),
        (
            "use.js",
            "import { A } from './left.js'; import './right.js'; A = replacement; function run() { new A(); }",
        ),
        (
            "use.js",
            "import { A } from './left.js'; import './right.js'; function run() { eval(''); new A(); }",
        ),
        (
            "use.ts",
            "import type { A } from './left.js'; import './right.js'; function run() { new A(); }",
        ),
        (
            "use.ts",
            "import { type A } from './left.js'; import './right.js'; function run() { new A(); }",
        ),
    ] {
        let (_root, db) = project(&[
            ("left.js", "export class A { constructor() {} }"),
            ("right.js", "export class A { constructor() {} }"),
            (file, caller),
        ]);
        for target in ["left.js", "right.js"] {
            let handle = format!("sym:{target}#A.constructor");
            assert!(references(&db, &handle).is_empty(), "{caller}: {handle}");
            assert!(impact_calls(&db, &handle).is_empty(), "{caller}: {handle}");
        }
        assert_eq!(
            db.find_references("A", Some(ReferenceKind::Instantiation))
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn stale_caller_or_export_source_cannot_disambiguate_constructors() {
    for changed in ["use.js", "models.js"] {
        let (root, db) = project(&[
            ("models.js", MODELS),
            ("models.ts", MODELS),
            ("use.js", JS_CALLS),
            ("use.ts", TS_CALLS),
        ]);
        let handle = "sym:models.js#A.constructor";
        let before = references(&db, handle);
        assert_eq!(before.len(), 4);
        assert_eq!(
            before
                .iter()
                .filter(|row| row.to.as_deref() == Some(handle))
                .count(),
            1
        );
        let previous = std::fs::read_to_string(root.path().join(changed)).unwrap();
        std::fs::write(
            root.path().join(changed),
            format!("{previous}// modified\n"),
        )
        .unwrap();
        let after = references(&db, handle);
        assert_eq!(
            after.len(),
            2,
            "remaining TypeScript constructor union: {changed}: {after:?}"
        );
        assert!(
            after.iter().all(|row| row.to.as_deref() != Some(handle)),
            "{changed}"
        );
        assert!(impact_calls(&db, handle).is_empty(), "{changed}");
        assert_eq!(
            after
                .iter()
                .filter(|row| row.to.as_deref() == Some("sym:models.ts#A.constructor"))
                .count(),
            1
        );
    }
}

#[test]
fn reassigned_exports_and_reexport_chains_do_not_prove_class_identity() {
    for source in [
        "export class A { constructor() {} } A = replacement;",
        "class A { constructor() {} } export { A };",
        "export { A } from './right.js'; class A { constructor() {} }",
    ] {
        let (_root, db) = project(&[
            ("left.js", source),
            ("right.js", "export class A { constructor() {} }"),
            (
                "use.js",
                "import { A } from './left.js'; import './right.js'; function run() { new A(); }",
            ),
        ]);
        for file in ["left.js", "right.js"] {
            let handle = format!("sym:{file}#A.constructor");
            assert!(references(&db, &handle).is_empty(), "{source}: {handle}");
            assert!(impact_calls(&db, &handle).is_empty(), "{source}: {handle}");
        }
    }
}
