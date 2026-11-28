use codesage_graph::{
    assess_risk, file_import_pairs, find_references, full_index, impact_analysis,
    list_dependencies, trace_call_path,
};
use codesage_protocol::{CallPathRequest, FindReferencesRequest, ImpactRequest, ImpactTarget};
use codesage_storage::Database;

fn indexed(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    for (path, source) in files {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    std::fs::create_dir(dir.path().join(".codesage")).unwrap();
    let db = Database::open(&dir.path().join(".codesage/index.db")).unwrap();
    let stats = full_index(dir.path(), &db, &[], false).unwrap();
    assert_eq!(stats.files_failed, 0);
    (dir, db)
}

fn reaches(db: &Database, from: &str, to: &str) -> bool {
    trace_call_path(
        db,
        &CallPathRequest {
            from: from.into(),
            to: to.into(),
            max_depth: 2,
        },
    )
    .unwrap()
    .found
}

fn refs(db: &Database, spelling: &str) -> Vec<codesage_protocol::Reference> {
    find_references(
        db,
        &FindReferencesRequest {
            symbol_name: spelling.into(),
            kind: None,
        },
    )
    .unwrap()
    .results
}

#[test]
fn same_module_package_binds_only_its_indexed_members() {
    let (_dir, db) = indexed(&[
        ("go.mod", "module example.com/app\n"),
        (
            "cmd/main.go",
            "package main\nimport company \"example.com/app/pkg/util\"\nfunc main() { _ = company.Join() }\n",
        ),
        (
            "cmd/default.go",
            "package main\nimport \"example.com/app/pkg/util\"\nfunc Default() int { return joining.Join() }\n",
        ),
        (
            "pkg/util/net.go",
            "package joining\nfunc Join() int { return 1 }\n",
        ),
        ("pkg/util/meta.go", "package joining\nconst Base = 1\n"),
        (
            "pkg/util/net_test.go",
            "package joining\nimport \"testing\"\nfunc TestJoin(t *testing.T) { t.Log(Base) }\n",
        ),
        (
            "pkg/decoy/net.go",
            "package joining\nfunc Join() int { return 2 }\n",
        ),
    ]);
    let debt = list_dependencies(&db, "cmd/main.go").unwrap();
    assert_eq!(
        debt.imports,
        ["file:pkg/util/meta.go", "file:pkg/util/net.go"]
    );
    assert!(debt.unresolved_imports.is_empty(), "{debt:?}");
    assert_eq!(
        list_dependencies(&db, "pkg/util/net.go")
            .unwrap()
            .imported_by,
        ["cmd/default.go", "cmd/main.go"]
    );
    assert!(
        list_dependencies(&db, "pkg/util/net_test.go")
            .unwrap()
            .imported_by
            .is_empty()
    );
    let pairs = file_import_pairs(&db).unwrap();
    assert!(
        pairs
            .eager
            .contains(&("cmd/main.go".into(), "pkg/util/net.go".into()))
    );
    assert!(
        pairs
            .eager
            .iter()
            .all(|(_, to)| to.starts_with("pkg/util/") && !to.ends_with("_test.go"))
    );

    let target = "sym:pkg/util/net.go#Join";
    assert!(reaches(&db, "sym:cmd/main.go#main", target));
    assert!(reaches(&db, "sym:cmd/default.go#Default", target));
    assert!(!reaches(
        &db,
        "sym:cmd/main.go#main",
        "sym:pkg/decoy/net.go#Join"
    ));
    for spelling in ["Join", "company.Join", "joining.Join"] {
        let rows = refs(&db, spelling);
        assert!(!rows.is_empty(), "{spelling}");
        assert!(
            rows.iter().all(|row| row.to.as_deref() == Some(target)),
            "{rows:?}"
        );
    }
    let impact = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::File {
                path: "pkg/util/net.go".into(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();
    for caller in ["cmd/main.go", "cmd/default.go"] {
        assert!(
            impact.iter().any(|row| row.file_path == caller),
            "{impact:?}"
        );
    }
    let symbol_impact = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: target.into(),
            },
            depth: 1,
            source_only: true,
        },
    )
    .unwrap();
    let callers: Vec<_> = symbol_impact
        .iter()
        .map(|row| row.file_path.as_str())
        .collect();
    assert_eq!(callers, ["cmd/default.go", "cmd/main.go"]);
}

#[test]
fn unrelated_packages_and_shadowed_receivers_do_not_gain_symbol_edges() {
    let (_dir, db) = indexed(&[
        ("go.mod", "module example.com/app\n"),
        (
            "cmd/std.go",
            "package main\nimport \"errors\"\nfunc Std() { _ = errors.New(\"no\") }\n",
        ),
        (
            "cmd/alias.go",
            "package main\nimport foreign \"other.com/errors\"\nfunc Foreign() { _ = foreign.New(\"no\") }\n",
        ),
        (
            "cmd/shadow.go",
            "package main\nimport \"example.com/app/pkg/errors\"\ntype Local struct{}\nfunc (Local) New() {}\nfunc Shadow() { errors := Local{}; errors.New() }\nvar _ = errors.Package\n",
        ),
        (
            "cmd/nested.go",
            "package main\nimport \"example.com/app/nested/pkg/errors\"\nfunc Nested() { _ = errors.New(\"no\") }\n",
        ),
        (
            "pkg/errors/errors.go",
            "package errors\nconst Package = 1\nfunc New(s string) string { return s }\n",
        ),
        ("nested/go.mod", "module example.com/nested\n"),
        (
            "nested/pkg/errors/errors.go",
            "package errors\nfunc New(s string) string { return s }\n",
        ),
    ]);
    let target = "sym:pkg/errors/errors.go#New";
    assert!(!reaches(&db, "sym:cmd/std.go#Std", target));
    assert!(!reaches(&db, "sym:cmd/alias.go#Foreign", target));
    assert!(!reaches(&db, "sym:cmd/shadow.go#Shadow", target));
    assert!(!reaches(
        &db,
        "sym:cmd/nested.go#Nested",
        "sym:nested/pkg/errors/errors.go#New"
    ));
    for caller in ["cmd/std.go", "cmd/alias.go", "cmd/nested.go"] {
        let deps = list_dependencies(&db, caller).unwrap();
        assert!(deps.imports.is_empty(), "{deps:?}");
        assert_eq!(deps.unresolved_imports.len(), 1, "{deps:?}");
    }
    let rows = refs(&db, "New");
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert!(rows.iter().all(|row| row.to.is_none()), "{rows:?}");
    assert!(
        list_dependencies(&db, "nested/pkg/errors/errors.go")
            .unwrap()
            .imported_by
            .is_empty()
    );
}

#[test]
fn a_proven_go_package_name_binds_at_unshadowed_sites() {
    let (_dir, db) = indexed(&[
        (
            "go.mod",
            "module example.com/app\ngo 1.23\nrequire external.test/util v0.0.0\nreplace external.test/util => ./external\n",
        ),
        ("wire/aaa_docs.go", "package documentation\n"),
        (
            "wire/aab_generate.go",
            "//go:build ignore\n\npackage main\nfunc Join() int { return 99 }\n",
        ),
        (
            "wire/wire.go",
            "package /* Go package docs */ util\nfunc Join() int { return 1 }\n",
        ),
        ("external/go.mod", "module external.test/util\ngo 1.23\n"),
        (
            "external/pkg.go",
            "package stats\nfunc Other() int { return 2 }\n",
        ),
        (
            "cmd/main.go",
            r#"package main
import ("external.test/util"; "example.com/app/wire")
type local struct { x int }
func (local) Join() int { return 0 }
func Imported() int { return util./* rationale */ Join() + stats.Other() }
func Shadow(util local) int { return util.Join() }
func Initializer() int { util := local{x: util.Join()}; return util.Join() }
func main() { _ = Imported(); _ = Shadow(local{}); _ = Initializer() }
"#,
        ),
    ]);
    let rows = refs(&db, "util.Join");
    let commented = rows.iter().find(|row| row.line == 5).unwrap();
    assert_eq!(
        (commented.col, commented.to_name.as_str()),
        (29, "util.Join")
    );
    let mut edges: Vec<_> = rows
        .iter()
        .map(|row| (row.line, row.col, row.to.as_deref()))
        .collect();
    edges.sort_unstable();
    let edges: Vec<_> = edges
        .into_iter()
        .map(|(line, _, edge)| (line, edge))
        .collect();
    let target = "sym:wire/wire.go#Join";
    assert_eq!(
        edges,
        [(5, Some(target)), (6, None), (7, Some(target)), (7, None)]
    );
    assert!(reaches(&db, "sym:cmd/main.go#Imported", target));
    assert!(reaches(&db, "sym:cmd/main.go#Initializer", target));
    assert!(!reaches(&db, "sym:cmd/main.go#Shadow", target));
    assert_eq!(
        list_dependencies(&db, "cmd/main.go").unwrap().imports,
        ["file:wire/wire.go"]
    );
    let impact = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: target.into(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();
    assert_eq!(impact.len(), 1, "{impact:?}");
    assert_eq!(impact[0].file_path, "cmd/main.go");
    let mut lines: Vec<_> = impact[0].reasons.iter().map(|reason| reason.line).collect();
    lines.sort_unstable();
    assert_eq!(lines, [5, 7], "{impact:?}");
}

#[test]
fn raw_imports_and_go_type_scopes_bind_only_visible_package_selectors() {
    let (_dir, db) = indexed(&[
        ("go.mod", "module example.com/app\ngo 1.23\n"),
        (
            "wire/aaa_generate.go",
            "//go:build\tignore\n\npackage main\nfunc Join() int { return 99 }\n",
        ),
        (
            "wire/aab_legacy.go",
            "//+build ignore\n\npackage main\nfunc Join() int { return 98 }\n",
        ),
        (
            "wire/aac_comment.go",
            "//+build ignore\npackage util\nfunc Comment() int { return 2 }\n",
        ),
        (
            "wire/wire.go",
            "package util\nfunc Join() int { return 1 }\n",
        ),
        (
            "caller.go",
            r#"package app
import (`example.com/app/wire`; "unsafe")
type Joiner interface { Join() int }
type Box[T Joiner] struct { value T }
func (r *Box[util]) Method() int { return util.Join(r.value) }
func Switch(x any) int {
    switch util := x.(type) {
    case [unsafe.Sizeof(util.Join())]byte: _ = util
    case Joiner: return util.Join()
    }
    return util.Join()
}
"#,
        ),
        (
            "aliased.go",
            "package app\nimport named \"example.com/app/wire\"\nfunc Aliased() int { return named.Join() }\n",
        ),
        (
            "escaped.go",
            r#"package app
import "example.com/app/\x77ire"
func Escaped() int { return util.Join() + util.Comment() }
"#,
        ),
    ]);
    let target = "sym:wire/wire.go#Join";
    let rows = refs(&db, "util.Join");
    assert_eq!(
        rows.iter()
            .map(|row| (row.from_file.as_str(), row.line, row.to.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("caller.go", 5, None),
            ("caller.go", 8, Some(target)),
            ("caller.go", 9, None),
            ("caller.go", 11, Some(target)),
            ("escaped.go", 3, Some(target))
        ]
    );
    assert!(reaches(&db, "sym:caller.go#Switch", target));
    assert!(reaches(&db, "sym:aliased.go#Aliased", target));
    assert!(reaches(
        &db,
        "sym:escaped.go#Escaped",
        "sym:wire/aac_comment.go#Comment"
    ));
    assert_eq!(
        list_dependencies(&db, "escaped.go").unwrap().imports,
        list_dependencies(&db, "aliased.go").unwrap().imports
    );
    assert!(
        list_dependencies(&db, "caller.go")
            .unwrap()
            .imports
            .contains(&"file:wire/wire.go".to_owned())
    );
}

#[test]
fn go_method_fallback_stays_in_the_attested_package() {
    let (_dir, db) = indexed(&[
        (
            "go.mod",
            "module example.com/app\ngo 1.23\nrequire external.test/unexpected v0.0.0\nreplace external.test/unexpected => ./external\n",
        ),
        (
            "external/go.mod",
            "module external.test/unexpected\ngo 1.23\n",
        ),
        (
            "external/lib.go",
            "package renamed\nfunc Work() int { return 1 }\n",
        ),
        (
            "cmd/caller.go",
            r#"package cmd
import ("math/rand/v2"; "external.test/unexpected")
func Std() int { return rand.IntN(10) }
func External() int { return renamed.Work() }
func Local() int { return Owner.Work(Owner{}) }
"#,
        ),
        (
            "cmd/owner.go",
            "package cmd\ntype Owner struct{}\nfunc (Owner) Work() int { return 2 }\n",
        ),
        (
            "cmd/decoy_test.go",
            "package cmd_test\ntype renamed struct{}\nfunc (renamed) Work() int { return 3 }\n",
        ),
        (
            "decoy/main.go",
            "package decoy\ntype rand struct{}\nfunc (rand) IntN() int { return 4 }\ntype Owner struct{}\nfunc (Owner) Work() int { return 3 }\n",
        ),
    ]);
    let local = "sym:cmd/owner.go#Owner.Work";
    assert!(reaches(&db, "sym:cmd/caller.go#Local", local));
    assert!(!reaches(
        &db,
        "sym:cmd/caller.go#Local",
        "sym:decoy/main.go#Owner.Work"
    ));
    for (spelling, caller, decoy) in [
        ("rand.IntN", "Std", "sym:decoy/main.go#rand.IntN"),
        (
            "renamed.Work",
            "External",
            "sym:cmd/decoy_test.go#renamed.Work",
        ),
    ] {
        let rows = refs(&db, spelling);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].to.is_none(), "{rows:?}");
        assert!(!reaches(&db, &format!("sym:cmd/caller.go#{caller}"), decoy));
    }
    let rows = refs(&db, "Owner.Work");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].to.as_deref(), Some(local));
}

#[test]
fn commented_go_object_calls_do_not_reuse_plain_method_resolution() {
    let ordinary = "func Ordinary() int { return Owner.Work(Owner{}) }\n";
    let commented = "func Commented(Owner Other) int { return Owner./* probe */ Work() }\n";
    for source in [
        format!("package local\n{ordinary}{commented}"),
        format!("package local\nimport \"fmt\"\nvar _ = fmt.Sprint\n{commented}{ordinary}"),
    ] {
        let (_dir, db) = indexed(&[
            ("go.mod", "module example.com/local\ngo 1.23\n"),
            ("calls.go", &source),
            (
                "methods.go",
                "package local\ntype Owner struct{}\ntype Other struct{}\n\
                 func (Owner) Work() int { return 1 }\n\
                 func (Other) Work() int { return 2 }\n",
            ),
        ]);
        let target = "sym:methods.go#Owner.Work";
        let rows = refs(&db, "Owner.Work");
        assert_eq!(rows.len(), 2, "{rows:?}");
        let plain = rows
            .iter()
            .find(|row| row.from_symbol.as_deref() == Some("Ordinary"))
            .unwrap();
        let trivia = rows
            .iter()
            .find(|row| row.from_symbol.as_deref() == Some("Commented"))
            .unwrap();
        assert_eq!(plain.to.as_deref(), Some(target));
        assert!(trivia.to.is_none(), "{trivia:?}");
        assert!(reaches(&db, "sym:calls.go#Ordinary", target));
        assert!(!reaches(&db, "sym:calls.go#Commented", target));
        let impact = impact_analysis(
            &db,
            &ImpactRequest {
                target: ImpactTarget::Symbol {
                    name: target.into(),
                },
                depth: 1,
                source_only: false,
            },
        )
        .unwrap();
        assert_eq!(impact.len(), 1, "{impact:?}");
        assert_eq!(impact[0].file_path, "calls.go");
        assert_eq!(impact[0].reasons.len(), 1, "{impact:?}");
        assert_eq!(impact[0].reasons[0].line, plain.line);
    }
}

#[test]
fn ignored_go_generators_keep_symbols_without_package_import_edges() {
    let unknown_header = format!(
        "// {}\npackage pkg\nconst Marker = 1\n",
        "x".repeat(9 * 1024)
    );
    let (_dir, db) = indexed(&[
        ("go.mod", r#"module "example.com/\x61pp""#),
        (
            "cmd/main.go",
            "package cmd\nimport \"example.com/app/pkg\"\nfunc Run() int { return pkg.Work() }\n",
        ),
        (
            "pkg/member.go",
            "package pkg\nfunc Work() int { return 1 }\n",
        ),
        ("pkg/unknown.go", unknown_header.as_str()),
        (
            "pkg/modern.go",
            "//go:build\tignore\n\npackage main\nimport \"example.com/app/cmd\"\nfunc GeneratorMarker() { cmd.Run() }\n",
        ),
        (
            "pkg/legacy.go",
            "//+build ignore\n\npackage main\nimport \"example.com/app/cmd\"\nfunc LegacyMarker() { cmd.Run() }\n",
        ),
        (
            "pkg/docs.go",
            "package /* clause */ documentation\nimport \"example.com/app/cmd\"\nfunc DocumentationMarker() { cmd.Run() }\n",
        ),
    ]);
    let included = list_dependencies(&db, "cmd/main.go").unwrap();
    assert_eq!(
        included.imports,
        ["file:pkg/member.go", "file:pkg/unknown.go"]
    );
    assert!(included.imported_by.is_empty(), "{included:?}");
    assert!(reaches(
        &db,
        "sym:cmd/main.go#Run",
        "sym:pkg/member.go#Work"
    ));
    for file in ["pkg/modern.go", "pkg/legacy.go", "pkg/docs.go"] {
        let deps = list_dependencies(&db, file).unwrap();
        assert!(
            deps.imports.is_empty() && deps.imported_by.is_empty(),
            "{deps:?}"
        );
        let impact = impact_analysis(
            &db,
            &ImpactRequest {
                target: ImpactTarget::File { path: file.into() },
                depth: 1,
                source_only: false,
            },
        )
        .unwrap();
        assert!(impact.is_empty(), "{impact:?}");
    }
    assert!(!db.find_symbols("GeneratorMarker", None).unwrap().is_empty());
    assert_eq!(
        db.find_symbols("DocumentationMarker", None).unwrap()[0].file_path,
        "pkg/docs.go"
    );
    let pairs = file_import_pairs(&db).unwrap();
    assert_eq!(
        pairs.eager,
        [
            ("cmd/main.go".into(), "pkg/member.go".into()),
            ("cmd/main.go".into(), "pkg/unknown.go".into())
        ]
    );
    let caller_impact = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::File {
                path: "cmd/main.go".into(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();
    assert!(caller_impact.is_empty(), "{caller_impact:?}");
    let member_impact = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::File {
                path: "pkg/member.go".into(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();
    assert_eq!(member_impact.len(), 1, "{member_impact:?}");
    assert_eq!(member_impact[0].file_path, "cmd/main.go");
    assert!(!assess_risk(&db, "cmd/main.go").unwrap().in_cycle);
}

#[test]
fn go_path_matching_respects_the_manifest_and_raw_import_namespace() {
    let (dir, db) = indexed(&[
        ("go.mod", "module example.com/app\n"),
        (
            "app/app.go",
            "package app\nimport \"example.com/app/lib\"\nfunc App() { lib.Lib() }\n",
        ),
        (
            "lib/lib.go",
            "package lib\nimport \"example.com/app/app\"\nfunc Lib() { app.App() }\n",
        ),
        (
            "cmd/main.go",
            "package main\nimport _ \"external.test/lib.go\"\nimport \"example.com/app/testdata/support\"\nimport \"example.com/app/_helpers\"\nfunc main() { support.Use(); helper.Use() }\n",
        ),
        ("external.test/lib.go", "package decoy\n"),
        (
            "testdata/support/support.go",
            "package support\nfunc Use() {}\n",
        ),
        ("_helpers/helper.go", "package helper\nfunc Use() {}\n"),
    ]);
    assert!(assess_risk(&db, "app/app.go").unwrap().in_cycle);
    assert!(
        list_dependencies(&db, "external.test/lib.go")
            .unwrap()
            .imported_by
            .is_empty()
    );
    assert_eq!(
        list_dependencies(&db, "cmd/main.go").unwrap().imports,
        [
            "file:_helpers/helper.go",
            "file:testdata/support/support.go"
        ]
    );

    std::fs::write(dir.path().join("go.mod"), "module example.com/renamed\n").unwrap();
    assert!(
        list_dependencies(&db, "app/app.go")
            .unwrap()
            .imports
            .is_empty()
    );
    assert!(!assess_risk(&db, "app/app.go").unwrap().in_cycle);
}
