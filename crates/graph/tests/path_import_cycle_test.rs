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
    // Cycle-break guidance reads the same path-resolved edges as the SCCs.
    for file in ["js/a.js", "ts/e.ts", "c/x.h"] {
        let notes = assess_risk(&db, file).unwrap().notes;
        assert!(
            notes
                .iter()
                .any(|n| n.starts_with("candidate break point: ")),
            "{file}: {notes:?}"
        );
    }

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

fn assert_no_cycle(db: &Database, files: &[&str]) {
    for file in files {
        let risk = assess_risk(db, file).unwrap();
        assert!(!risk.in_cycle, "{file}: {risk:?}");
    }
}

#[test]
fn typescript_type_only_imports_do_not_close_a_cycle() {
    let (_dir, db) = index_tree(&[
        (
            "t/a.ts",
            "import type { B } from './b';\nexport interface A { b?: B }\n",
        ),
        (
            "t/b.ts",
            "import type { A } from './a';\nexport interface B { a?: A }\n",
        ),
        (
            "t/c.ts",
            "import { type D } from './d';\nexport type C = { d?: D };\n",
        ),
        (
            "t/d.ts",
            "export type { C } from './c';\nexport interface D { n: number }\n",
        ),
        (
            "t/e.ts",
            "import { type F, f } from './f';\nexport const e = (x?: F) => f(x);\n",
        ),
        (
            "t/f.ts",
            "import { e } from './e';\nexport type F = number;\nexport const f = (x?: F) => e;\n",
        ),
    ]);
    for file in ["t/a.ts", "t/b.ts", "t/c.ts", "t/d.ts"] {
        let risk = assess_risk(&db, file).unwrap();
        assert!(!risk.in_cycle, "{file}: {risk:?}");
        assert_eq!(risk.lazy_edges, 2, "{file}: {risk:?}");
    }
    // A clause with one value specifier still loads the module.
    assert_eq!(cycle_of(&db, "t/e.ts"), vec!["t/f.ts".to_string()]);
}

#[test]
fn a_directive_loads_one_file_and_declaration_files_load_nothing() {
    let (_dir, db) = index_tree(&[
        (
            "lib/a.js",
            "const b = require('./b');\nmodule.exports = b;\n",
        ),
        (
            "lib/b.d.ts",
            "import './a';\nexport declare const b: number;\n",
        ),
        // Node resolves `./d` to d.js; the d.ts sibling is never loaded.
        (
            "lib/c.js",
            "const d = require('./d');\nmodule.exports = d;\n",
        ),
        ("lib/d.js", "module.exports = 1;\n"),
        (
            "lib/d.ts",
            "import { c } from './c';\nexport const d = c;\n",
        ),
        // From TypeScript, `./e` skips the declaration file for the runtime module.
        (
            "ts/app.ts",
            "import { e } from './e';\nexport const app = e;\n",
        ),
        ("ts/e.d.ts", "export declare const e: number;\n"),
        (
            "ts/e.js",
            "const app = require('./app');\nmodule.exports = { e: app };\n",
        ),
    ]);
    assert_no_cycle(&db, &["lib/a.js", "lib/b.d.ts", "lib/c.js", "lib/d.ts"]);
    let pairs = file_import_pairs(&db).unwrap();
    assert!(
        !pairs
            .eager
            .iter()
            .any(|(from, to)| from.ends_with(".d.ts") || to.ends_with(".d.ts")),
        "{pairs:?}"
    );
    assert!(
        !pairs
            .eager
            .contains(&("lib/c.js".to_string(), "lib/d.ts".to_string())),
        "{pairs:?}"
    );
    assert_eq!(cycle_of(&db, "ts/app.ts"), vec!["ts/e.js".to_string()]);
    // list_dependencies keeps its over-approximation.
    let deps = list_dependencies(&db, "lib/d.ts").unwrap();
    assert!(deps.imported_by.iter().any(|p| p == "lib/c.js"), "{deps:?}");
}

#[test]
fn a_quoted_include_stops_at_the_includer_directory() {
    let (_dir, db) = index_tree(&[
        ("src/x.h", "#include \"config.h\"\nint x(void);\n"),
        ("src/config.h", "#define LOCAL 1\n"),
        ("config.h", "#include \"src/x.h\"\n#define ROOT 1\n"),
    ]);
    assert_no_cycle(&db, &["src/x.h", "config.h"]);
    let pairs = file_import_pairs(&db).unwrap();
    assert!(
        pairs
            .eager
            .contains(&("src/x.h".to_string(), "src/config.h".to_string())),
        "{pairs:?}"
    );
}

#[test]
fn a_bare_javascript_specifier_is_not_joined_onto_the_importer_directory() {
    let (_dir, db) = index_tree(&[
        (
            "src/a.js",
            "import { b } from 'lib/b.js';\nexport const a = b;\n",
        ),
        (
            "src/lib/b.js",
            "import { a } from '../a.js';\nexport const b = a;\n",
        ),
    ]);
    assert_no_cycle(&db, &["src/a.js", "src/lib/b.js"]);
}

/// Pins the `find_references` `to` skip on its own: the definition sits in
/// the importing file, so no import evidence is consulted.
#[test]
fn a_go_import_in_the_defining_file_carries_no_to() {
    let (_dir, db) = index_tree(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "util/k.go",
            "package util\n\nimport \"K\"\n\ntype K struct{}\n",
        ),
    ]);
    let row = references(&db, "K")
        .into_iter()
        .find(|r| r.kind == ReferenceKind::Import)
        .expect("import \"K\" is indexed as a reference row");
    assert_eq!(row.to, None, "{row:?}");
}

/// Pins the owner-evidence paths: the Go import list feeding `names_owner`
/// and the outgoing-ref scan for a method's owner type. The bare `Run()`
/// spelling resolves to the lone `R.Run` definition, so only owner evidence
/// stands between it and a `to`.
#[test]
fn a_go_import_spelled_like_a_method_owner_is_no_evidence() {
    let (_dir, db) = index_tree(&[
        ("go.mod", "module example.com/app\n\ngo 1.22\n"),
        (
            "util/r.go",
            "package util\n\ntype R struct{}\n\nfunc (r R) Run() int { return 1 }\n",
        ),
        (
            "caller.go",
            "package main\n\nimport \"R\"\n\nfunc use() int { return Run() }\n",
        ),
    ]);
    let row = references(&db, "Run")
        .into_iter()
        .find(|r| r.from_file == "caller.go")
        .expect("Run() is indexed as a reference row");
    assert_eq!(row.kind, ReferenceKind::Call);
    assert_eq!(row.to, None, "{row:?}");
}

/// A class declared in a `.d.ts` joins by symbol name, not by path; the
/// declaration endpoint must still form no cycle edge.
#[test]
fn a_symbol_joined_declaration_file_endpoint_forms_no_edge() {
    let (_dir, db) = index_tree(&[
        (
            "t/a.ts",
            "import { B } from './b';\nexport class A extends B {}\n",
        ),
        ("t/b.d.ts", "export declare class B extends A {}\n"),
    ]);
    let pairs = file_import_pairs(&db).unwrap();
    assert!(
        !pairs
            .eager
            .iter()
            .chain(&pairs.lazy_only)
            .any(|(from, to)| from == "t/b.d.ts" || to == "t/b.d.ts"),
        "{pairs:?}"
    );
    assert_no_cycle(&db, &["t/a.ts", "t/b.d.ts"]);
}
