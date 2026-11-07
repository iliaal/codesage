use codesage_graph::{DescribeOptions, describe, find_references, full_index, impact_analysis};
use codesage_protocol::{
    DescribeDetail, FindReferencesRequest, ImpactRequest, ImpactTarget, ReferenceKind,
};
use codesage_storage::Database;

#[test]
fn construction_reaches_only_own_initializers_and_retains_class_rows() {
    for (definitions, source, calls, caller) in [
        (
            "models.py",
            "class A:\n    def __init__(self): pass\nclass B(A): pass\nclass C(A):\n    def __init__(self): pass\n",
            "use.py",
            "from models import A, B, C\ndef run():\n    A()\n    B()\n    C()\n",
        ),
        (
            "models.php",
            "<?php\nclass A { function __construct() {} }\nclass B extends A {}\nclass C extends A { function __construct() {} }\n",
            "use.php",
            "<?php\nfunction run() {\n    new A();\n    new B();\n    new C();\n}\n",
        ),
        (
            "models.php",
            "<?php\nclass A { function __CONSTRUCT() {} }\nclass B extends A {}\nclass C extends A { function __CONSTRUCT() {} }\n",
            "use.php",
            "<?php\nfunction run() {\n    new A();\n    new B();\n    new C();\n}\n",
        ),
        (
            "models.js",
            "export class A { constructor() {} }\nexport class B extends A {}\nexport class C extends A { constructor() {} }\n",
            "use.js",
            "import { A, B, C } from './models.js';\nfunction run() {\n    new A();\n    new B();\n    new C();\n}\n",
        ),
        (
            "models.ts",
            "export class A { constructor() {} }\nexport class B extends A {}\nexport class C extends A { constructor() {} }\n",
            "use.ts",
            "import { A, B, C } from './models.ts';\nfunction run() {\n    new A();\n    new B();\n    new C();\n}\n",
        ),
        (
            "models.js",
            "export class A { constructor() // initialization\n{} }\nexport class B extends A {}\nexport class C extends A { constructor() // initialization\n{} }\n",
            "use.js",
            "import { A, B, C } from './models.js';\nfunction run() {\n    new A();\n    new B();\n    new C();\n}\n",
        ),
        (
            "models.ts",
            "export class A { constructor() // initialization\n{} }\nexport class B extends A {}\nexport class C extends A { constructor() // initialization\n{} }\n",
            "use.ts",
            "import { A, B, C } from './models.ts';\nfunction run() {\n    new A();\n    new B();\n    new C();\n}\n",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        std::fs::write(dir.path().join(definitions), source).unwrap();
        std::fs::write(dir.path().join(calls), caller).unwrap();
        full_index(dir.path(), &db, &[], false).unwrap();
        let extension = definitions.rsplit('.').next().unwrap();
        let (constructor, separator, kind) = match extension {
            "py" => ("__init__", ".", ReferenceKind::Call),
            "php" => (
                if source.contains("__CONSTRUCT") {
                    "__CONSTRUCT"
                } else {
                    "__construct"
                },
                "\\",
                ReferenceKind::Instantiation,
            ),
            _ => ("constructor", ".", ReferenceKind::Instantiation),
        };
        for (class, line) in [("A", 3), ("C", 5)] {
            let handle = format!("sym:models.{extension}#{class}{separator}{constructor}");
            let impact = impact_analysis(
                &db,
                &ImpactRequest {
                    target: ImpactTarget::Symbol {
                        name: handle.clone(),
                    },
                    depth: 1,
                    source_only: false,
                },
            )
            .unwrap();
            let caller = impact
                .iter()
                .find(|row| row.file_path == format!("use.{extension}"))
                .unwrap_or_else(|| panic!("{handle}: {impact:?}"));
            assert_eq!(
                caller.reasons.iter().map(|r| r.line).collect::<Vec<_>>(),
                [line]
            );
            let refs = find_references(
                &db,
                &FindReferencesRequest {
                    symbol_name: handle.clone(),
                    kind: Some(kind),
                },
            )
            .unwrap();
            let calls: Vec<_> = refs
                .results
                .iter()
                .filter(|row| row.to.as_deref() == Some(&handle))
                .map(|row| (row.from_file.as_str(), row.line))
                .collect();
            assert_eq!(
                calls,
                [(format!("use.{extension}").as_str(), line)],
                "{handle}"
            );
            let options = DescribeOptions {
                detail: DescribeDetail::Full,
                sections: Some(vec!["references".into(), "callees".into()]),
                ..Default::default()
            };
            let incoming = describe(dir.path(), &db, &handle, &options)
                .unwrap()
                .card
                .unwrap();
            let caller = format!("sym:use.{extension}#run");
            assert_eq!(incoming.sections["references"].data["callers_total"], 1);
            assert_eq!(
                incoming.sections["references"].data["callers"],
                serde_json::json!([caller])
            );
            let outgoing = describe(dir.path(), &db, &caller, &options)
                .unwrap()
                .card
                .unwrap();
            assert!(
                outgoing.sections["callees"].data["top"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!(handle))
            );
        }
        for (class, line) in [("A", 3), ("B", 4), ("C", 5)] {
            let handle = format!("sym:models.{extension}#{class}");
            let refs = find_references(
                &db,
                &FindReferencesRequest {
                    symbol_name: handle.clone(),
                    kind: Some(kind),
                },
            )
            .unwrap();
            assert!(
                refs.results.iter().any(|row| {
                    row.from_file == format!("use.{extension}")
                        && row.to_name == class
                        && row.line == line
                }),
                "class row missing: {handle}: {refs:?}"
            );
            if extension != "php" {
                assert!(
                    refs.results.iter().any(|row| {
                        row.from_file == format!("use.{extension}")
                            && row.to.as_deref() == Some(&handle)
                    }),
                    "class target missing: {handle}: {refs:?}"
                );
            }
        }
    }
}

#[test]
fn static_constructor_methods_are_not_instance_initializers() {
    for extension in ["js", "ts"] {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let definitions = format!("models.{extension}");
        let caller = format!("use.{extension}");
        std::fs::write(dir.path().join(&definitions),
            "export class A { static /* modifier */ constructor() {} }\nexport class C {\n  constructor() {}\n  static constructor() {}\n}\n").unwrap();
        std::fs::write(dir.path().join(&caller), format!(
            "import {{ A, C }} from './models.{extension}';\nfunction run() {{ new A(); new C(); }}\nfunction invoke() {{ A.constructor(); C.constructor(); }}\n")).unwrap();
        full_index(dir.path(), &db, &[], false).unwrap();
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "constructor".into(),
                kind: Some(ReferenceKind::Instantiation),
            },
        )
        .unwrap();
        let instance = db
            .find_symbols("C.constructor", None)
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.line_start == 3)
            .unwrap();
        let handle = instance.handle().to_string();
        assert_eq!(refs.results.len(), 1, "{refs:?}");
        assert_eq!(refs.results[0].to.as_deref(), Some(handle.as_str()));
        assert_eq!(refs.results[0].to_name, "C");
        assert_eq!(refs.results[0].line, 2);
        for method in db.find_symbols("constructor", None).unwrap() {
            let impact = impact_analysis(
                &db,
                &ImpactRequest {
                    target: ImpactTarget::Symbol {
                        name: method.handle().to_string(),
                    },
                    depth: 1,
                    source_only: false,
                },
            )
            .unwrap();
            let constructions: Vec<_> = impact
                .iter()
                .flat_map(|entry| &entry.reasons)
                .filter(|reason| reason.kind == ReferenceKind::Instantiation)
                .collect();
            assert_eq!(
                constructions.len(),
                usize::from(method.line_start == 3),
                "{method:?}: {impact:?}"
            );
        }
        let ordinary = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "constructor".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        assert_eq!(ordinary.results.len(), 2);
        assert!(
            ordinary
                .results
                .iter()
                .all(|row| row.from_file == caller && row.line == 3)
        );
        for name in ["A", "C"] {
            assert_eq!(
                db.find_references(name, Some(ReferenceKind::Instantiation))
                    .unwrap()
                    .len(),
                1
            );
        }
    }
}

#[test]
fn ambiguous_classes_do_not_supply_constructor_targets() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    for (file, source) in [
        ("left.js", "export class A { constructor() {} }"),
        ("right.js", "export class A { constructor() {} }"),
        (
            "use.js",
            "import './left.js'; import './right.js'; function run() { new A(); }",
        ),
    ] {
        std::fs::write(dir.path().join(file), source).unwrap();
    }
    full_index(dir.path(), &db, &[], false).unwrap();
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "constructor".into(),
            kind: Some(ReferenceKind::Instantiation),
        },
    )
    .unwrap();
    assert!(refs.results.is_empty(), "{refs:?}");
    assert_eq!(
        db.find_references("A", Some(ReferenceKind::Instantiation))
            .unwrap()
            .len(),
        1
    );
    for file in ["left.js", "right.js"] {
        assert!(
            impact_analysis(
                &db,
                &ImpactRequest {
                    target: ImpactTarget::Symbol {
                        name: format!("sym:{file}#A.constructor")
                    },
                    depth: 1,
                    source_only: false,
                }
            )
            .unwrap()
            .is_empty()
        );
    }
}
