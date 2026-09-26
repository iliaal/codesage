//! Import cycles closed by path specifiers (`'./b.js'`, `"y.h"`) and Go
//! package imports that must not bind to a same-spelled project symbol.

use codesage_graph::{
    assess_risk, assess_risk_diff, build_review_rehearsal, file_import_pairs, find_references,
    full_index, impact_analysis, index_files, list_dependencies, session_start,
};
use codesage_parser::discover::content_hash;
use codesage_protocol::{
    FileInfo, FindReferencesRequest, ImpactRequest, ImpactTarget, Language, ReferenceKind,
};
use codesage_storage::Database;
use std::path::Path;

fn index_tree(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".codesage")).unwrap();
    for (path, source) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, source).unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    (dir, db)
}

const MIXED: &[(&str, &str)] = &[
    (
        "js/a.js",
        "import { b } from './b.js';\nexport function a() { return b(); }\n",
    ),
    (
        "js/b.js",
        "import { a } from './a.js';\nexport function b() { return a(); }\n",
    ),
    (
        "ts/c.ts",
        "import { d } from './d';\nexport function c(): number { return d(); }\n",
    ),
    (
        "ts/d.ts",
        "import { c } from './c';\nexport function d(): number { return c(); }\n",
    ),
    (
        "ts/e.ts",
        "import { f } from './f.js';\nexport function e(): number { return f(); }\n",
    ),
    (
        "ts/f.ts",
        "import { e } from './e.js';\nexport function f(): number { return e(); }\n",
    ),
    ("c/x.h", "#include \"y.h\"\nint x(void) { return y(); }\n"),
    ("c/y.h", "#include \"x.h\"\nint y(void) { return x(); }\n"),
    (
        "c/main.c",
        "#include <stdio.h>\n#include <x.h>\n#include \"x.h\"\n#include \"missing.h\"\nint main(void) { return x(); }\n",
    ),
    (
        "php/A.php",
        "<?php\nnamespace App;\nuse App\\B;\nclass A {}\n",
    ),
    (
        "php/B.php",
        "<?php\nnamespace App;\nuse App\\A;\nclass B {}\n",
    ),
];

fn cycle_of(db: &Database, file: &str) -> Vec<String> {
    let risk = assess_risk(db, file).unwrap();
    assert!(risk.in_cycle, "{file}: {risk:?}");
    assert_eq!(risk.cycle_size, 2, "{file}: {risk:?}");
    risk.cycle_files
}

#[test]
fn javascript_typescript_and_c_header_pairs_cycle_like_php() {
    let (_dir, db) = index_tree(MIXED);

    assert_eq!(cycle_of(&db, "js/a.js"), vec!["js/b.js".to_string()]);
    assert_eq!(cycle_of(&db, "ts/c.ts"), vec!["ts/d.ts".to_string()]);
    // An ESM `.js` specifier names the `.ts` file on disk.
    assert_eq!(cycle_of(&db, "ts/e.ts"), vec!["ts/f.ts".to_string()]);
    assert_eq!(cycle_of(&db, "c/x.h"), vec!["c/y.h".to_string()]);
    assert_eq!(cycle_of(&db, "php/A.php"), vec!["php/B.php".to_string()]);

    let main = assess_risk(&db, "c/main.c").unwrap();
    assert!(!main.in_cycle, "{main:?}");

    let pairs = file_import_pairs(&db).unwrap();
    assert!(pairs.lazy_only.is_empty(), "{pairs:?}");
    // The quoted include resolves; `<stdio.h>`, `<x.h>` (a system include,
    // even though a same-named project header exists), and the unindexed
    // `missing.h` form no edge.
    let from_main: Vec<&str> = pairs
        .eager
        .iter()
        .filter(|(from, _)| from == "c/main.c")
        .map(|(_, to)| to.as_str())
        .collect();
    assert_eq!(from_main, vec!["c/x.h"]);
    let deps = list_dependencies(&db, "c/x.h").unwrap();
    assert!(
        deps.imported_by.iter().any(|p| p == "c/main.c"),
        "cycle edges and list_dependencies agree: {deps:?}"
    );

    let patch = vec!["js/a.js".to_string(), "js/b.js".to_string()];
    let diff = assess_risk_diff(&db, &patch).unwrap();
    assert_eq!(diff.cycles_touching_patch.len(), 1, "{diff:?}");
    assert_eq!(
        diff.cycles_touching_patch[0].members,
        vec!["js/a.js".to_string(), "js/b.js".to_string()]
    );
}

#[test]
fn rehearsal_and_session_snapshot_see_path_import_cycles() {
    let (dir, db) = index_tree(MIXED);

    let report = build_review_rehearsal(
        dir.path(),
        &db,
        &["ts/c.ts".to_string(), "ts/d.ts".to_string()],
    )
    .unwrap();
    assert!(
        report
            .objections
            .iter()
            .any(|o| o.category == "import-cycle"),
        "{:?}",
        report.objections
    );

    let snapshot = session_start(dir.path(), &db, "paths").unwrap();
    let mut cycles = snapshot.cycles.clone();
    cycles.sort();
    assert_eq!(
        cycles,
        vec![
            vec!["c/x.h".to_string(), "c/y.h".to_string()],
            vec!["js/a.js".to_string(), "js/b.js".to_string()],
            vec!["php/A.php".to_string(), "php/B.php".to_string()],
            vec!["ts/c.ts".to_string(), "ts/d.ts".to_string()],
            vec!["ts/e.ts".to_string(), "ts/f.ts".to_string()],
        ]
    );
}

#[test]
fn cpp_header_pair_cycles() {
    let (_dir, db) = index_tree(&[
        ("lib/a.hpp", "#include \"b.hpp\"\nstruct A { int f(); };\n"),
        ("lib/b.hpp", "#include \"a.hpp\"\nstruct B { int g(); };\n"),
        (
            "lib/main.cpp",
            "#include \"lib/a.hpp\"\nint main() { return 0; }\n",
        ),
    ]);
    assert_eq!(cycle_of(&db, "lib/a.hpp"), vec!["lib/b.hpp".to_string()]);
    let main = assess_risk(&db, "lib/main.cpp").unwrap();
    assert!(!main.in_cycle, "{main:?}");
    // A root-relative quoted include resolves exactly.
    let pairs = file_import_pairs(&db).unwrap();
    assert!(
        pairs
            .eager
            .contains(&("lib/main.cpp".to_string(), "lib/a.hpp".to_string())),
        "{pairs:?}"
    );
}

fn index_source(root: &Path, db: &Database, path: &str, language: Language, source: &str) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, source).unwrap();
    let file = FileInfo {
        path: path.into(),
        language,
        content_hash: content_hash(source.as_bytes()),
        is_test: false,
    };
    index_files(root, db, &[file], false).unwrap();
}

/// A specifier becomes resolvable when its target file is indexed; the cached
/// cycle result must not outlive that, whether the write lands on the reading
/// connection or another one.
#[test]
fn cycle_cache_picks_up_a_newly_indexed_path_target() {
    for cross_connection in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("index.db");
        let reader = Database::open(&path).unwrap();
        index_source(
            root.path(),
            &reader,
            "a.js",
            Language::JavaScript,
            "import { b } from './b';\nexport function a() { return b(); }\n",
        );
        assert!(!assess_risk(&reader, "a.js").unwrap().in_cycle);

        let writer = Database::open(&path).unwrap();
        let writer = if cross_connection { &writer } else { &reader };
        index_source(
            root.path(),
            writer,
            "b.js",
            Language::JavaScript,
            "import { a } from './a';\nexport function b() { return a(); }\n",
        );
        let risk = assess_risk(&reader, "a.js").unwrap();
        assert!(
            risk.in_cycle,
            "cross_connection={cross_connection}: {risk:?}"
        );
        assert_eq!(risk.cycle_files, vec!["b.js".to_string()]);
    }
}

const CGO: &[(&str, &str)] = &[
    ("go.mod", "module example.com/app\n\ngo 1.22\n"),
    (
        "main.go",
        "package main\n\n// #include <stdio.h>\nimport \"C\"\nimport \"fmt\"\n\nfunc main() { fmt.Println(\"x\") }\n",
    ),
    (
        "other.go",
        "package main\n\nimport \"errors\"\n\nvar E = errors.New(\"x\")\n",
    ),
    ("util/c.go", "package util\n\ntype C struct{}\n"),
    (
        "util/s.go",
        "package util\n\nfunc errors() int { return 1 }\n",
    ),
];

fn references(db: &Database, name: &str) -> Vec<codesage_protocol::Reference> {
    find_references(
        db,
        &FindReferencesRequest {
            symbol_name: name.to_string(),
            kind: None,
        },
    )
    .unwrap()
    .results
}

fn impacted(db: &Database, path: &str) -> Vec<String> {
    impact_analysis(
        db,
        &ImpactRequest {
            target: ImpactTarget::File {
                path: path.to_string(),
            },
            depth: 2,
            source_only: false,
        },
    )
    .unwrap()
    .into_iter()
    .map(|e| e.file_path)
    .collect()
}

#[test]
fn go_package_imports_bind_to_no_same_named_project_symbol() {
    let (_dir, db) = index_tree(CGO);

    let c_import = references(&db, "C")
        .into_iter()
        .find(|r| r.from_file == "main.go" && r.kind == ReferenceKind::Import)
        .expect("import \"C\" is indexed as a reference row");
    assert_eq!(c_import.to, None, "{c_import:?}");

    let errors_import = references(&db, "errors")
        .into_iter()
        .find(|r| r.from_file == "other.go" && r.kind == ReferenceKind::Import)
        .expect("import \"errors\" is indexed as a reference row");
    assert_eq!(errors_import.to, None, "{errors_import:?}");

    let c_impact = impacted(&db, "util/c.go");
    assert!(!c_impact.iter().any(|p| p == "main.go"), "{c_impact:?}");
    let s_impact = impacted(&db, "util/s.go");
    assert!(!s_impact.iter().any(|p| p == "other.go"), "{s_impact:?}");

    for target in ["util/c.go", "util/s.go"] {
        let deps = list_dependencies(&db, target).unwrap();
        assert!(deps.imported_by.is_empty(), "{target}: {deps:?}");
    }
    let pairs = file_import_pairs(&db).unwrap();
    assert!(
        pairs.eager.is_empty() && pairs.lazy_only.is_empty(),
        "{pairs:?}"
    );
    assert!(db.enumerate_file_import_edges().unwrap().is_empty());
}
