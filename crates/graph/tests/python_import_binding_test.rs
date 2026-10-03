use codesage_graph::{
    ReachabilityOptions, assess_risk, export_context_for_symbol, file_import_pairs,
    find_references, full_index, impact_analysis, list_dependencies,
    recommend_tests_with_reachability, trace_call_path,
};
use codesage_protocol::{
    CallPathRequest, DEFAULT_EMBEDDING_DIM, ExportRequest, FindReferencesRequest, ImpactRequest,
    ImpactTarget, ReferenceKind,
};
use codesage_storage::Database;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let root = tempfile::tempdir().unwrap();
    for (path, source) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let db = Database::open_in_memory().unwrap();
    full_index(root.path(), &db, &[], false).unwrap();
    for (path, source) in files {
        db.insert_chunks(
            path,
            "python",
            &[(
                source,
                1,
                source.lines().count() as u32,
                &vec![0.0; DEFAULT_EMBEDDING_DIM],
            )],
        )
        .unwrap();
    }
    (root, db)
}

fn trace(db: &Database, from: &str, to: &str) -> bool {
    trace_call_path(
        db,
        &CallPathRequest {
            from: from.into(),
            to: to.into(),
            max_depth: 4,
        },
    )
    .unwrap()
    .found
}

fn bundle(db: &Database, from: &str) -> codesage_protocol::ContextBundle {
    export_context_for_symbol(
        db,
        from,
        &ExportRequest {
            query: None,
            symbol: Some(from.into()),
            include_callers: false,
            include_callees: true,
            limit: 20,
        },
    )
    .unwrap()
}

#[test]
fn wildcard_getters_leave_dynamic_export_membership_unknown() {
    for name in ["_patch", "patch"] {
        for state in [
            "absent",
            "annotation",
            "deleted",
            "getter_deleted",
            "empty",
            "member",
            "unknown",
        ] {
            let all = match state {
                "annotation" => "__all__: object\n".to_string(),
                "deleted" => "__all__ = []\ndel __all__\n".to_string(),
                "empty" => "__all__ = []\n".to_string(),
                "member" => format!("__all__ = ['{name}']\n"),
                "unknown" => "def names():\n    return []\n__all__ = names()\n".to_string(),
                _ => String::new(),
            };
            let exported = if name.starts_with('_') {
                format!("from unittest.mock import patch as {name}\n")
            } else {
                "from .api import patch\n".to_string()
            };
            let initializer = format!(
                "{exported}{all}def __getattr__(key):\n    return ('{name}',)\n{}",
                if state == "getter_deleted" {
                    "del __getattr__\n"
                } else {
                    ""
                }
            );
            let caller = format!(
                "from fallback import {name}\nfrom pkg import *\ndef entry():\n    return {name}('x')\n"
            );
            let definition = format!("def {name}(value):\n    return value\n");
            let (_root, db) = project(&[
                ("caller.py", &caller),
                ("fallback.py", &definition),
                ("pkg/api.py", &definition),
                ("pkg/__init__.py", &initializer),
                (
                    "tests/test_caller.py",
                    "from caller import entry\ndef test_entry():\n    return entry()\n",
                ),
            ]);
            let expected = match (state, name.starts_with('_')) {
                ("empty", _) | ("getter_deleted", true) => Some(format!("sym:fallback.py#{name}")),
                ("member" | "getter_deleted", false) => Some(format!("sym:pkg/api.py#{name}")),
                _ => None,
            };
            let rows = find_references(
                &db,
                &FindReferencesRequest {
                    symbol_name: name.into(),
                    kind: Some(ReferenceKind::Call),
                },
            )
            .unwrap();
            let calls: Vec<_> = rows
                .results
                .iter()
                .filter(|row| row.from_file == "caller.py")
                .collect();
            assert_eq!(calls.len(), 1, "{name}/{state}: {rows:?}");
            assert_eq!(calls[0].to, expected, "{name}/{state}: {rows:?}");
            let context = bundle(&db, "entry");
            for path in ["fallback.py", "pkg/api.py"] {
                let handle = format!("sym:{path}#{name}");
                let resolved = expected.as_deref() == Some(&handle);
                assert_eq!(
                    trace(&db, "entry", &handle),
                    resolved,
                    "{name}/{state}/{path}"
                );
                assert_eq!(
                    context.related.iter().any(|row| row.file_path == path),
                    resolved,
                    "{name}/{state}: {context:?}"
                );
                let impact = impact_analysis(
                    &db,
                    &ImpactRequest {
                        target: ImpactTarget::Symbol { name: handle },
                        depth: 3,
                        source_only: false,
                    },
                )
                .unwrap();
                assert_eq!(
                    impact.iter().any(|row| row.file_path == "caller.py"
                        && row
                            .reasons
                            .iter()
                            .any(|reason| reason.kind == ReferenceKind::Call)),
                    resolved,
                    "{name}/{state}: {impact:?}"
                );
            }
            assert!(
                list_dependencies(&db, "fallback.py")
                    .unwrap()
                    .imported_by
                    .contains(&"caller.py".into())
            );
            let tests = recommend_tests_with_reachability(
                &db,
                &["fallback.py".into()],
                &ReachabilityOptions::default(),
            )
            .unwrap();
            assert!(
                tests
                    .reachable
                    .iter()
                    .any(|row| row.path == "tests/test_caller.py"),
                "{name}/{state}: {tests:?}"
            );
        }
    }
}

#[test]
fn external_python_import_forms_never_create_edges_for_one_or_many_namesakes() {
    for other in [false, true] {
        for source in [
            "from unittest.mock import patch\ndef entry():\n    patch('x')\n",
            "from unittest.mock import patch as p\ndef entry():\n    p('x')\n",
            "from unittest import mock\ndef entry():\n    mock.patch('x')\n",
            "from unittest import mock as m\ndef entry():\n    m.patch('x')\n",
            "import unittest.mock\ndef entry():\n    unittest.mock.patch('x')\n",
            "import unittest.mock as mock\ndef entry():\n    mock.patch('x')\n",
            "import copy\ndef entry():\n    copy.copy('x')\n",
            "import copy as c\ndef entry():\n    c.copy('x')\n",
            "from copy import copy\ndef entry():\n    copy('x')\n",
            "from copy import copy as c\ndef entry():\n    c('x')\n",
        ] {
            let mut files = vec![
                ("tests/test_external.py", source),
                (
                    "view.py",
                    "from tests.test_external import entry\nclass View:\n    def patch(self, x):\n        return x\n    def copy(self, x):\n        return x\n",
                ),
            ];
            if other {
                files.push((
                    "other.py",
                    "def patch(x):\n    pass\ndef copy(x):\n    pass\n",
                ));
            }
            let (_root, db) = project(&files);
            for name in ["patch", "copy"] {
                let refs = find_references(
                    &db,
                    &FindReferencesRequest {
                        symbol_name: name.into(),
                        kind: None,
                    },
                )
                .unwrap();
                assert!(
                    refs.results.iter().all(|r| r.to.is_none()),
                    "{source}: {refs:?}"
                );
                assert!(
                    !trace(&db, "entry", &format!("sym:view.py#View.{name}")),
                    "{source}"
                );
            }
            assert!(
                list_dependencies(&db, "view.py")
                    .unwrap()
                    .imported_by
                    .is_empty(),
                "{source}"
            );
            let pairs = file_import_pairs(&db).unwrap();
            assert!(!assess_risk(&db, "view.py").unwrap().in_cycle, "{source}");
            assert!(
                !pairs
                    .eager
                    .contains(&("tests/test_external.py".into(), "view.py".into())),
                "{source}: {pairs:?}"
            );
            let impact = impact_analysis(
                &db,
                &ImpactRequest {
                    target: ImpactTarget::File {
                        path: "view.py".into(),
                    },
                    depth: 3,
                    source_only: false,
                },
            )
            .unwrap();
            assert!(
                !impact
                    .iter()
                    .any(|r| r.file_path == "tests/test_external.py"),
                "{source}: {impact:?}"
            );
            let context = bundle(&db, "entry");
            assert!(
                !context.related.iter().any(|s| s.file_path == "view.py"),
                "{source}: {context:?}"
            );
            let callers = export_context_for_symbol(
                &db,
                "sym:view.py#View.patch",
                &ExportRequest {
                    query: None,
                    symbol: Some("sym:view.py#View.patch".into()),
                    include_callers: true,
                    include_callees: false,
                    limit: 20,
                },
            )
            .unwrap();
            assert!(
                !callers
                    .related
                    .iter()
                    .any(|r| r.file_path == "tests/test_external.py"),
                "{source}: {callers:?}"
            );
            let tests = recommend_tests_with_reachability(
                &db,
                &["view.py".into()],
                &ReachabilityOptions::default(),
            )
            .unwrap();
            assert!(
                !tests
                    .reachable
                    .iter()
                    .any(|r| r.path == "tests/test_external.py"),
                "{source}: {tests:?}"
            );
        }
    }
}

#[test]
fn internal_python_imports_and_reexports_keep_real_edges() {
    for source in [
        "from pkg.api import patch\ndef entry():\n    patch('x')\n",
        "from pkg.api import patch as p\ndef entry():\n    p('x')\n",
        "import pkg.api\ndef entry():\n    pkg.api.patch('x')\n",
        "import pkg.api as api\ndef entry():\n    api.patch('x')\n",
        "from pkg import api\ndef entry():\n    api.patch('x')\n",
        "from pkg import facade\ndef entry():\n    facade.patch('x')\n",
        "from pkg.facade import patch\ndef entry():\n    patch('x')\n",
        "from pkg import exported as p\ndef entry():\n    p('x')\n",
    ] {
        let (_root, db) = project(&[
            ("tests/test_internal.py", source),
            ("pkg/api.py", "def patch(x):\n    return x\n"),
            ("pkg/facade.py", "from .api import patch\n"),
            ("pkg/__init__.py", "from .api import patch as exported\n"),
            ("other.py", "def patch(x):\n    return 0\n"),
        ]);
        assert!(trace(&db, "entry", "sym:pkg/api.py#patch"), "{source}");
        let aliases = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "p".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        assert!(
            aliases
                .results
                .iter()
                .all(|row| row.to.as_deref() == Some("sym:pkg/api.py#patch")),
            "{source}: {aliases:?}"
        );
        assert!(!trace(&db, "entry", "sym:other.py#patch"), "{source}");
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "patch".into(),
                kind: None,
            },
        )
        .unwrap();
        assert!(
            refs.results
                .iter()
                .filter(|r| r.kind != ReferenceKind::Import)
                .all(|r| r.to.as_deref() == Some("sym:pkg/api.py#patch")),
            "{source}: {refs:?}"
        );
        let context = bundle(&db, "entry");
        assert!(
            context.related.iter().any(|s| s.file_path == "pkg/api.py"),
            "{source}: {context:?}"
        );
        let impact = impact_analysis(
            &db,
            &ImpactRequest {
                target: ImpactTarget::File {
                    path: "pkg/api.py".into(),
                },
                depth: 3,
                source_only: false,
            },
        )
        .unwrap();
        assert!(
            impact
                .iter()
                .any(|r| r.file_path == "tests/test_internal.py"),
            "{source}: {impact:?}"
        );
        let tests = recommend_tests_with_reachability(
            &db,
            &["pkg/api.py".into()],
            &ReachabilityOptions::default(),
        )
        .unwrap();
        assert!(
            tests
                .reachable
                .iter()
                .any(|r| r.path == "tests/test_internal.py"),
            "{source}: {tests:?}"
        );
    }
}

#[test]
fn python_binding_scope_and_order_override_same_file_and_name_fallbacks() {
    let cases = [
        (
            "from unittest.mock import patch\ndef patch(x):\n    return x\ndef entry():\n    patch('x')\n",
            vec![(5, true)],
        ),
        (
            "def patch(x):\n    return x\nfrom unittest.mock import patch\ndef entry():\n    patch('x')\n",
            vec![(5, false)],
        ),
        (
            "from unittest.mock import patch\npatch('before')\ndef patch(x):\n    return x\npatch('after')\n",
            vec![(2, false), (5, true)],
        ),
        (
            "def patch(x):\n    return x\npatch('before')\nfrom unittest.mock import patch\npatch('after')\n",
            vec![(3, true), (5, false)],
        ),
        (
            "from unittest.mock import patch\ndef entry():\n    def patch(x):\n        return x\n    patch('before')\n    from unittest.mock import patch\n    patch('after')\n",
            vec![(5, true), (7, false)],
        ),
        (
            "from unittest.mock import patch\ndef entry():\n    patch('unbound')\n    def patch(x):\n        return x\n    patch('bound')\n",
            vec![(3, false), (6, true)],
        ),
        (
            "def patch(x):\n    return x\ndef entry():\n    patch('unbound')\n    patch = object()\n",
            vec![(4, false)],
        ),
        (
            "from unittest.mock import patch\ndef entry(patch):\n    patch('parameter')\n",
            vec![(3, false)],
        ),
    ];
    for (source, expected) in cases {
        let (_root, db) = project(&[
            ("caller.py", source),
            (
                "view.py",
                "class View:\n    def patch(self, x):\n        return x\n",
            ),
        ]);
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "patch".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        for (line, local) in expected {
            let row = refs
                .results
                .iter()
                .find(|r| r.from_file == "caller.py" && r.line == line)
                .unwrap();
            assert_eq!(
                row.to
                    .as_deref()
                    .is_some_and(|target| target.starts_with("sym:caller.py#patch")),
                local,
                "{source}: {refs:?}"
            );
            assert_ne!(
                row.to.as_deref(),
                Some("sym:view.py#View.patch"),
                "{source}"
            );
        }
    }
}

#[test]
fn relative_and_src_layout_python_packages_resolve_without_namesake_fallbacks() {
    for (caller, api, package, source) in [
        (
            "pkg/client.py",
            "pkg/api.py",
            "pkg/__init__.py",
            "from .api import patch as p\ndef entry():\n    p('x')\n",
        ),
        (
            "pkg/client.py",
            "pkg/api.py",
            "pkg/__init__.py",
            "from . import api\ndef entry():\n    api.patch('x')\n",
        ),
        (
            "src/pkg/client.py",
            "src/pkg/api.py",
            "src/pkg/__init__.py",
            "from pkg.api import patch\ndef entry():\n    patch('x')\n",
        ),
    ] {
        let (_root, db) = project(&[
            (caller, source),
            (api, "def patch(x):\n    return x\n"),
            (package, ""),
            ("other.py", "def patch(x):\n    return 0\n"),
        ]);
        assert!(trace(&db, "entry", &format!("sym:{api}#patch")), "{source}");
        assert!(!trace(&db, "entry", "sym:other.py#patch"), "{source}");
        assert!(
            list_dependencies(&db, api)
                .unwrap()
                .imported_by
                .contains(&caller.into()),
            "{source}"
        );
        let pairs = file_import_pairs(&db).unwrap();
        assert!(
            pairs.eager.contains(&(caller.into(), api.into())),
            "{source}: {pairs:?}"
        );
    }
}

#[test]
fn python_local_aliases_and_class_receivers_keep_genuine_calls() {
    let (_root, db) = project(&[
        (
            "caller.py",
            "from unittest.mock import patch\ndef actual(x):\n    return x\npatch = actual\ndef entry():\n    patch('x')\nclass View:\n    def patch(self, x):\n        return x\n    def run(self):\n        self.patch('x')\n",
        ),
        (
            "other.py",
            "class Other:\n    def patch(self, x):\n        return x\n",
        ),
    ]);
    assert!(trace(&db, "entry", "actual"));
    assert!(trace(&db, "View.run", "View.patch"));
    assert!(!trace(&db, "View.run", "Other.patch"));
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "patch".into(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    assert!(
        refs.results
            .iter()
            .any(|r| r.to.as_deref() == Some("sym:caller.py#actual"))
    );
    assert!(
        refs.results
            .iter()
            .any(|r| r.to.as_deref() == Some("sym:caller.py#View.patch"))
    );
}

#[test]
fn stale_python_binding_evidence_remains_unknown_until_incremental_reparse() {
    let (root, db) = project(&[
        (
            "caller.py",
            "from api import patch\ndef entry():\n    patch('x')\n",
        ),
        ("api.py", "def patch(x):\n    return x\n"),
    ]);
    assert!(trace(&db, "entry", "patch"));
    db.execute_raw_for_tests("UPDATE files SET interpretation=NULL WHERE path='caller.py';")
        .unwrap();
    assert!(!trace(&db, "entry", "patch"));
    assert!(db.import_targets_for_file("caller.py").unwrap().is_empty());
    assert!(file_import_pairs(&db).unwrap().eager.is_empty());
    db.execute_raw_for_tests("DELETE FROM python_bindings;
        UPDATE files SET interpretation='codesage/structural/v1;parser-queries=4;extraction=7;trust-boundaries=1';").unwrap();
    assert!(!trace(&db, "entry", "patch"));
    assert!(db.import_sources_for_file("api.py").unwrap().is_empty());
    let rows = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "patch".into(),
            kind: None,
        },
    )
    .unwrap();
    assert!(rows.results.iter().all(|row| row.to.is_none()));
    let stats = codesage_graph::incremental_index(root.path(), &db, &[], false).unwrap();
    assert_eq!(stats.files_indexed, 2);
    assert!(trace(&db, "entry", "patch"));
    assert_eq!(
        db.import_targets_for_file("caller.py").unwrap(),
        vec!["api.py"]
    );
    assert!(
        db.all_file_interpretations()
            .unwrap()
            .values()
            .all(|value| value.as_deref() == Some(codesage_graph::STRUCTURAL_INTERPRETATION))
    );
}

#[test]
fn python_package_wins_over_same_named_module_in_every_file_edge_consumer() {
    let (_root, db) = project(&[
        (
            "caller.py",
            "from pkg import patch\ndef entry():\n    patch('x')\n",
        ),
        ("pkg/__init__.py", "def patch(x):\n    return x\n"),
        ("pkg.py", "def patch(x):\n    return 0\n"),
    ]);
    assert!(trace(&db, "entry", "sym:pkg/__init__.py#patch"));
    assert!(!trace(&db, "entry", "sym:pkg.py#patch"));
    assert!(
        list_dependencies(&db, "pkg/__init__.py")
            .unwrap()
            .imported_by
            .contains(&"caller.py".into())
    );
    assert!(
        list_dependencies(&db, "pkg.py")
            .unwrap()
            .imported_by
            .is_empty()
    );
    assert!(db.import_sources_for_file("pkg.py").unwrap().is_empty());
    assert_eq!(
        db.import_targets_for_file("caller.py").unwrap(),
        vec!["pkg/__init__.py"]
    );
    assert_eq!(
        file_import_pairs(&db).unwrap().eager,
        vec![("caller.py".into(), "pkg/__init__.py".into())]
    );
    assert!(
        impact_analysis(
            &db,
            &ImpactRequest {
                target: ImpactTarget::File {
                    path: "pkg.py".into()
                },
                depth: 2,
                source_only: false,
            }
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn external_python_import_keeps_name_rows_without_project_edges() {
    let (_root, db) = project(&[
        (
            "caller.py",
            "from unittest.mock import patch\n\ndef run():\n    return patch(\"example\")\n",
        ),
        (
            "view.py",
            "class View:\n    def patch(self, request):\n        return request\n",
        ),
    ]);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "patch".into(),
            kind: None,
        },
    )
    .unwrap();
    assert_eq!(refs.results.len(), 3);
    assert!(
        refs.results.iter().all(|row| row.to.is_none()),
        "external bindings must not target View.patch: {refs:?}"
    );
    assert!(
        list_dependencies(&db, "view.py")
            .unwrap()
            .imported_by
            .is_empty()
    );
    let impact = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::File {
                path: "view.py".into(),
            },
            depth: 2,
            source_only: false,
        },
    )
    .unwrap();
    assert!(
        impact.is_empty(),
        "external import created impact: {impact:?}"
    );
}

fn assert_call_targets(db: &Database, source: &str, expected: &[(u32, bool)]) {
    let refs = find_references(
        db,
        &FindReferencesRequest {
            symbol_name: "patch".into(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    for (line, internal) in expected {
        let row = refs
            .results
            .iter()
            .find(|row| row.from_file == "caller.py" && row.line == *line)
            .unwrap();
        assert_eq!(
            row.to.as_deref(),
            internal.then_some("sym:api.py#patch"),
            "{source}: {refs:?}"
        );
    }
}

#[test]
fn python_wildcard_imports_obey_order_and_known_export_membership() {
    for (source, expected) in [
        (
            "from api import patch\nfrom unittest.mock import *\ndef entry():\n    patch('os.getcwd')\n",
            false,
        ),
        (
            "from unittest.mock import *\nfrom api import patch\ndef entry():\n    patch('x')\n",
            true,
        ),
        (
            "from api import patch\nfrom empty import *\ndef entry():\n    patch('x')\n",
            true,
        ),
        (
            "from api import patch\nfrom external import *\ndef entry():\n    patch('os.getcwd')\n",
            false,
        ),
        (
            "from external import *\nfrom api import *\ndef entry():\n    patch('x')\n",
            true,
        ),
    ] {
        let (_root, db) = project(&[
            ("caller.py", source),
            ("api.py", "def patch(x):\n    return 'local'\n"),
            ("empty.py", "other = 1\n"),
            ("external.py", "from unittest.mock import patch\n"),
        ]);
        assert_call_targets(&db, source, &[(4, expected)]);
        assert_eq!(
            trace(&db, "entry", "sym:api.py#patch"),
            expected,
            "{source}"
        );
        assert_eq!(
            bundle(&db, "entry")
                .related
                .iter()
                .any(|r| r.file_path == "api.py"),
            expected,
            "{source}"
        );
    }
}

#[test]
fn python_lexical_targets_and_deletions_suppress_prior_imports() {
    for (source, expected) in [
        (
            "from api import patch\nfrom contextlib import nullcontext\nfrom unittest.mock import patch as external\ndef entry():\n    with nullcontext(external) as patch:\n        patch('os.getcwd')\n    patch('os.getcwd')\n",
            vec![(6, false), (7, false)],
        ),
        (
            "from api import patch\nfrom unittest.mock import patch as external\ndef entry():\n    [patch('os.getcwd') for patch in [external]]\n    patch('x')\n",
            vec![(4, false), (5, true)],
        ),
        (
            "from api import patch\ndef entry():\n    try:\n        raise ValueError()\n    except Exception as patch:\n        patch('x')\n    patch('x')\n",
            vec![(6, false), (7, false)],
        ),
        (
            "from api import patch\npatch('before')\ndel patch\npatch('after')\n",
            vec![(2, true), (4, false)],
        ),
        (
            "from api import patch\ndef entry():\n    patch('before')\n    del patch\n    patch('after')\n",
            vec![(3, false), (5, false)],
        ),
        (
            "from api import patch\ndef entry():\n    for patch in []:\n        patch('loop')\n    patch('after')\n",
            vec![(4, false), (5, false)],
        ),
    ] {
        let (_root, db) = project(&[
            ("caller.py", source),
            ("api.py", "def patch(x):\n    return x\n"),
        ]);
        assert_call_targets(&db, source, &expected);
    }
}

#[test]
fn python_global_and_nonlocal_directives_preserve_and_rebind_outer_names() {
    for (source, expected) in [
        (
            "from api import patch\ndef entry():\n    global patch\n    patch('x')\n",
            vec![(4, true)],
        ),
        (
            "from api import patch\ndef entry():\n    global patch\n    patch('before')\n    from unittest.mock import patch\n    patch('after')\n",
            vec![(4, true), (6, false)],
        ),
        (
            "from api import patch\ndef entry():\n    patch('x')\ndef mutate():\n    global patch\n    from unittest.mock import patch\n",
            vec![(3, true)],
        ),
        (
            "def outer():\n    from api import patch\n    def entry():\n        nonlocal patch\n        patch('x')\n    return entry\n",
            vec![(5, true)],
        ),
        (
            "def outer():\n    from api import patch\n    def entry():\n        nonlocal patch\n        patch('before')\n        from unittest.mock import patch\n        patch('after')\n    return entry\n",
            vec![(5, true), (7, false)],
        ),
    ] {
        let (_root, db) = project(&[
            ("caller.py", source),
            ("api.py", "def patch(x):\n    return x\n"),
        ]);
        assert_call_targets(&db, source, &expected);
    }
}

#[test]
fn python_from_import_export_precedence_agrees_across_file_consumers() {
    for (package, child_loaded) in [("def patch(x):\n    return x\n", false), ("", true)] {
        let (_root, db) = project(&[
            (
                "tests/test_caller.py",
                "from pkg import patch\ndef entry():\n    return patch('x')\n",
            ),
            ("pkg/__init__.py", package),
            (
                "pkg/patch.py",
                "from tests.test_caller import entry\ndef child():\n    return entry()\n",
            ),
        ]);
        assert_eq!(
            db.import_sources_for_file("pkg/patch.py")
                .unwrap()
                .contains(&"tests/test_caller.py".into()),
            child_loaded
        );
        assert_eq!(
            list_dependencies(&db, "pkg/patch.py")
                .unwrap()
                .imported_by
                .contains(&"tests/test_caller.py".into()),
            child_loaded
        );
        assert_eq!(
            file_import_pairs(&db)
                .unwrap()
                .eager
                .contains(&("tests/test_caller.py".into(), "pkg/patch.py".into())),
            child_loaded
        );
        assert_eq!(
            assess_risk(&db, "tests/test_caller.py").unwrap().in_cycle,
            child_loaded
        );
        let impact = impact_analysis(
            &db,
            &ImpactRequest {
                target: ImpactTarget::File {
                    path: "pkg/patch.py".into(),
                },
                depth: 2,
                source_only: false,
            },
        )
        .unwrap();
        assert_eq!(
            impact.iter().any(|r| r.file_path == "tests/test_caller.py"),
            child_loaded
        );
        let tests = recommend_tests_with_reachability(
            &db,
            &["pkg/patch.py".into()],
            &ReachabilityOptions::default(),
        )
        .unwrap();
        assert_eq!(
            tests
                .reachable
                .iter()
                .any(|r| r.path == "tests/test_caller.py"),
            child_loaded
        );
    }
}

#[test]
fn python_dotted_modules_cannot_load_through_a_nonpackage_parent() {
    for (parent, expected) in [("pkg.py", false), ("pkg/__init__.py", true)] {
        let (_root, db) = project(&[
            (
                "caller.py",
                "import pkg.api\ndef entry():\n    pkg.api.patch('x')\n",
            ),
            (parent, ""),
            ("pkg/api.py", "def patch(x):\n    return x\n"),
        ]);
        assert_eq!(trace(&db, "entry", "sym:pkg/api.py#patch"), expected);
        assert_eq!(
            list_dependencies(&db, "pkg/api.py")
                .unwrap()
                .imported_by
                .contains(&"caller.py".into()),
            expected
        );
        assert_eq!(
            file_import_pairs(&db)
                .unwrap()
                .eager
                .contains(&("caller.py".into(), "pkg/api.py".into())),
            expected
        );
        assert_eq!(
            bundle(&db, "entry")
                .related
                .iter()
                .any(|r| r.file_path == "pkg/api.py"),
            expected
        );
    }
}

#[test]
fn python_wildcard_export_lists_and_unknown_values_control_membership() {
    for (exports, expected) in [
        (
            "from unittest.mock import patch\nother = 1\n__all__ = ['other']\n",
            true,
        ),
        (
            "from unittest.mock import patch\n__all__ = ['patch']\n",
            false,
        ),
        (
            "from unittest.mock import patch\n__all__ = names()\n",
            false,
        ),
    ] {
        let source =
            "from api import patch\nfrom restricted import *\ndef entry():\n    patch('x')\n";
        let (_root, db) = project(&[
            ("caller.py", source),
            ("api.py", "def patch(x):\n    return x\n"),
            ("restricted.py", exports),
        ]);
        assert_call_targets(&db, source, &[(4, expected)]);
    }
    let (_root, db) = project(&[
        (
            "caller.py",
            "from pkg import patch\ndef entry():\n    patch('x')\n",
        ),
        ("pkg/__init__.py", "patch = None\nfrom empty import *\n"),
        ("pkg/patch.py", "from caller import entry\n"),
        ("empty.py", "other = 1\n"),
    ]);
    assert!(
        !db.import_sources_for_file("pkg/patch.py")
            .unwrap()
            .contains(&"caller.py".into())
    );
    assert!(!assess_risk(&db, "caller.py").unwrap().in_cycle);
}

#[test]
fn python_binding_forms_preserve_outer_scope_and_expression_order() {
    for (source, expected) in [
        (
            "from api import patch\ndef entry():\n    {patch('x') for patch in []}\n    patch('outer')\n",
            vec![(3, false), (4, true)],
        ),
        (
            "from api import patch\ndef entry():\n    {patch('x'): 1 for patch in []}\n    patch('outer')\n",
            vec![(3, false), (4, true)],
        ),
        (
            "from api import patch\ndef entry():\n    (patch('x') for patch in [])\n    patch('outer')\n",
            vec![(3, false), (4, true)],
        ),
        (
            "from api import patch\nfor patch in patch('iterable'):\n    patch('body')\n",
            vec![(2, true), (3, false)],
        ),
        (
            "from api import patch\ndef entry():\n    match object():\n        case patch:\n            patch('capture')\n",
            vec![(5, false)],
        ),
        (
            "from api import patch\ndef entry():\n    patch: object\n    patch('unbound')\n",
            vec![(4, false)],
        ),
        (
            "from api import patch\npatch: object\npatch('bound')\n",
            vec![(3, true)],
        ),
        (
            "from api import patch\ndef entry():\n    from api import patch\n    patch: object\n    patch('bound')\n",
            vec![(5, true)],
        ),
        (
            "from unittest.mock import patch\ndef patch(x=patch('os.getcwd')):\n    pass\n",
            vec![(2, false)],
        ),
        (
            "from api import patch\ndef entry():\n    try:\n        raise ValueError()\n    except Exception as patch:\n        pass\n    from api import patch\n    patch('rebound')\n",
            vec![(8, true)],
        ),
    ] {
        let (_root, db) = project(&[
            ("caller.py", source),
            ("api.py", "def patch(x):\n    return x\n"),
        ]);
        assert_call_targets(&db, source, &expected);
    }
}

#[test]
fn python_namespace_search_and_deleted_package_exports_allow_genuine_children() {
    for (parent, api, expected) in [
        ("unrelated.py", "pkg/api.py", true),
        ("pkg.py", "src/pkg/api.py", false),
        ("pkg/__init__.py", "src/pkg/api.py", false),
        ("src/pkg/__init__.py", "src/pkg/api.py", true),
    ] {
        let (_root, db) = project(&[
            (
                "caller.py",
                "import pkg.api\ndef entry():\n    pkg.api.patch('x')\n",
            ),
            (parent, ""),
            (api, "def patch(x):\n    return x\n"),
        ]);
        assert_eq!(
            trace(&db, "entry", &format!("sym:{api}#patch")),
            expected,
            "{parent}, {api}"
        );
        assert_eq!(
            list_dependencies(&db, api)
                .unwrap()
                .imported_by
                .contains(&"caller.py".into()),
            expected,
            "{parent}, {api}"
        );
    }
    let (_root, db) = project(&[
        (
            "caller.py",
            "from pkg import patch\ndef entry():\n    patch.patch('x')\n",
        ),
        ("pkg/__init__.py", "patch = None\ndel patch\n"),
        ("pkg/patch.py", "def patch(x):\n    return x\n"),
    ]);
    assert!(trace(&db, "entry", "sym:pkg/patch.py#patch"));
    assert!(
        list_dependencies(&db, "pkg/patch.py")
            .unwrap()
            .imported_by
            .contains(&"caller.py".into())
    );
}

#[test]
fn python_reference_resolution_retains_pair_cap_disclosure() {
    let sources: Vec<_> = (0..270)
        .map(|index| {
            (
                format!("caller_{index}.py"),
                "from api import patch\ndef entry():\n    patch('x')\n".to_string(),
            )
        })
        .chain(std::iter::once((
            "api.py".into(),
            "def patch(x):\n    return x\n".into(),
        )))
        .collect();
    let files: Vec<_> = sources
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect();
    let (_root, db) = project(&files);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "patch".into(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    assert_eq!(refs.results.len(), 270);
    let resolution = refs.to_resolution.unwrap();
    assert!(resolution.capped);
    assert!(resolution.resolved_pairs <= 256);
    assert!(refs.results.iter().any(|row| row.to.is_none()));
}

#[test]
fn python_plain_child_import_overrides_a_package_attribute() {
    for (source, expected) in [
        (
            "import pkg.api\ndef entry():\n    pkg.api.patch('x')\n",
            true,
        ),
        (
            "import pkg.api\nimport pkg.other\ndef entry():\n    pkg.api.patch('x')\n",
            true,
        ),
        (
            "from pkg import api\nimport pkg.api\ndef entry():\n    api.patch('x')\n",
            false,
        ),
        (
            "import pkg.api\nfrom pkg import api\ndef entry():\n    api.patch('x')\n",
            true,
        ),
    ] {
        let (_root, db) = project(&[
            ("caller.py", source),
            ("pkg/__init__.py", "api = None\n"),
            ("pkg/api.py", "def patch(x):\n    return x\n"),
            ("pkg/other.py", ""),
        ]);
        assert_eq!(
            trace(&db, "entry", "sym:pkg/api.py#patch"),
            expected,
            "{source}"
        );
        assert_eq!(
            bundle(&db, "entry")
                .related
                .iter()
                .any(|r| r.file_path == "pkg/api.py"),
            expected,
            "{source}"
        );
    }
}

#[test]
fn python_comprehension_walrus_aliases_keep_the_rhs_evaluation_scope() {
    for (source, expected) in [
        (
            "from api import patch as actual\nfrom unittest.mock import patch as external\ndef entry():\n    [(patch := actual) for actual in [external]]\n    patch('os.getcwd')\n",
            false,
        ),
        (
            "from api import patch as actual\ndef entry():\n    [(patch := actual) for item in [1]]\n    patch('x')\n",
            true,
        ),
    ] {
        let (_root, db) = project(&[
            ("caller.py", source),
            ("api.py", "def patch(x):\n    return x\n"),
        ]);
        assert_call_targets(&db, source, &[(source.lines().count() as u32, expected)]);
        assert_eq!(
            trace(&db, "entry", "sym:api.py#patch"),
            expected,
            "{source}"
        );
        assert_eq!(
            bundle(&db, "entry")
                .related
                .iter()
                .any(|r| r.file_path == "api.py"),
            expected,
            "{source}"
        );
    }
}

#[test]
fn python_comprehension_filters_execute_before_escaping_body_bindings() {
    for (body, filter, expected) in [("external", "actual", false), ("actual", "external", true)] {
        let source = format!(
            "from api import patch as actual\nfrom unittest.mock import patch as external\ndef entry():\n    [(patch := {body}) for item in [1] if (patch := {filter})]\n    patch('os.getcwd')\n"
        );
        let (_root, db) = project(&[
            ("caller.py", &source),
            ("api.py", "def patch(x):\n    return 'local'\n"),
        ]);
        assert_call_targets(&db, &source, &[(5, expected)]);
        assert_eq!(trace(&db, "entry", "sym:api.py#patch"), expected);
        assert_eq!(
            bundle(&db, "entry")
                .related
                .iter()
                .any(|r| r.file_path == "api.py"),
            expected
        );
    }
}

#[test]
fn python_cached_children_retain_later_package_attribute_bindings() {
    for (initializer, expected) in [
        (
            "from .api import patch\nfrom unittest import mock as api\n",
            false,
        ),
        ("from unittest import mock as api\n", true),
        (
            "from unittest import mock as api\nfrom .api import patch\n",
            true,
        ),
    ] {
        let source = "import pkg.api\ndef entry():\n    pkg.api.patch('os.getcwd')\n";
        let (_root, db) = project(&[
            ("caller.py", source),
            ("pkg/__init__.py", initializer),
            ("pkg/api.py", "def patch(x):\n    return 'local'\n"),
        ]);
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "patch".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        assert_eq!(
            refs.results
                .iter()
                .find(|r| r.from_file == "caller.py")
                .unwrap()
                .to
                .as_deref(),
            expected.then_some("sym:pkg/api.py#patch"),
            "{initializer}"
        );
        assert_eq!(
            trace(&db, "entry", "sym:pkg/api.py#patch"),
            expected,
            "{initializer}"
        );
        assert_eq!(
            bundle(&db, "entry")
                .related
                .iter()
                .any(|r| r.file_path == "pkg/api.py"),
            expected,
            "{initializer}"
        );
        assert!(
            list_dependencies(&db, "pkg/api.py")
                .unwrap()
                .imported_by
                .contains(&"caller.py".into())
        );
    }
}

#[test]
fn python_executing_class_scope_directives_update_outer_bindings() {
    for (initial, rebound, expected) in [
        ("api", "unittest.mock", false),
        ("unittest.mock", "api", true),
    ] {
        let source = format!(
            "from {initial} import patch\nclass Bind:\n    global patch\n    from {rebound} import patch\ndef entry():\n    patch('os.getcwd')\n"
        );
        let (_root, db) = project(&[
            ("caller.py", &source),
            ("api.py", "def patch(x):\n    return 'local'\n"),
        ]);
        assert_call_targets(&db, &source, &[(6, expected)]);
        assert_eq!(trace(&db, "entry", "sym:api.py#patch"), expected);
        assert_eq!(
            bundle(&db, "entry")
                .related
                .iter()
                .any(|r| r.file_path == "api.py"),
            expected
        );
    }
}

#[test]
fn python_class_directives_keep_function_activations_and_closure_targets() {
    for (source, expected) in [
        (
            "from api import patch\nfrom unittest.mock import patch as external\ndef entry():\n    patch('before')\n    class Bind:\n        global patch\n        patch = external\n    patch('after')\n",
            vec![(4, true), (8, false)],
        ),
        (
            "from unittest.mock import patch\nfrom api import patch as actual\ndef entry():\n    class Bind:\n        global patch\n        patch = actual\n    patch('after')\n",
            vec![(7, true)],
        ),
        (
            "from api import patch\ndef unused():\n    class Bind:\n        global patch\n        from unittest.mock import patch\ndef entry():\n    patch('x')\n",
            vec![(7, true)],
        ),
        (
            "from unittest.mock import patch as external\ndef entry():\n    from api import patch\n    class Bind:\n        nonlocal patch\n        patch = external\n    patch('after')\n",
            vec![(7, false)],
        ),
        (
            "from api import patch as actual\ndef entry():\n    from unittest.mock import patch\n    class Bind:\n        class Nested:\n            nonlocal patch\n            patch = actual\n    patch('after')\n",
            vec![(8, true)],
        ),
    ] {
        let (_root, db) = project(&[
            ("caller.py", source),
            ("api.py", "def patch(x):\n    return 'local'\n"),
        ]);
        assert_call_targets(&db, source, &expected);
    }
}

#[test]
fn python_eager_comprehensions_order_filters_and_generators_defer_writes() {
    for (expression, expected) in [
        (
            "[(patch := external) for item in [1] if (patch := actual)]",
            false,
        ),
        (
            "{(patch := external) for item in [1] if (patch := actual)}",
            false,
        ),
        (
            "{item: (patch := external) for item in [1] if (patch := actual)}",
            false,
        ),
        (
            "{item: (patch := actual) for item in [1] if (patch := external)}",
            true,
        ),
        (
            "[(patch := actual) for item in [1] if (patch := external) for other in [1]]",
            true,
        ),
        (
            "((patch := external) for item in [1] if (patch := actual))",
            false,
        ),
    ] {
        let source = format!(
            "from api import patch as actual\nfrom unittest.mock import patch as external\ndef entry():\n    {expression}\n    patch('os.getcwd')\n"
        );
        let (_root, db) = project(&[
            ("caller.py", &source),
            ("api.py", "def patch(x):\n    return 'local'\n"),
        ]);
        assert_call_targets(&db, &source, &[(5, expected)]);
    }
    let source = "from api import patch\nfrom unittest.mock import patch as external\ngenerator = ((patch := external) for item in [1])\ndef entry():\n    patch('x')\n";
    let (_root, db) = project(&[
        ("caller.py", source),
        ("api.py", "def patch(x):\n    return 'local'\n"),
    ]);
    assert_call_targets(&db, source, &[(5, true)]);
}

#[test]
fn python_package_attribute_access_respects_cached_deleted_and_transitive_children() {
    for (initializer, caller, expected) in [
        (
            "from .api import patch\nfrom unittest import mock as api\n",
            "import pkg.api\n",
            false,
        ),
        (
            "from .api import patch\nfrom unittest import mock as api\n",
            "import pkg.api as api\n",
            false,
        ),
        (
            "from unittest import mock as api\n",
            "import pkg.api as api\n",
            true,
        ),
        (
            "from .api import patch\ndel api\n",
            "import pkg.api as api\n",
            true,
        ),
        (
            "from .api import patch\ndel api\n",
            "import pkg.api\n",
            false,
        ),
        (
            "from .api import patch\ndel api\n",
            "from pkg import api\n",
            true,
        ),
        (
            "from .facade import patch\nfrom unittest import mock as api\n",
            "import pkg.api\n",
            false,
        ),
        (
            "from unittest import mock as api\nfrom .facade import patch\n",
            "import pkg.api\n",
            true,
        ),
        ("", "import pkg\n", false),
        ("from .api import patch\n", "import pkg\n", true),
    ] {
        let call = if caller.contains(" as api") || caller.contains("from pkg") {
            "api.patch"
        } else {
            "pkg.api.patch"
        };
        let source = format!("{caller}def entry():\n    {call}('os.getcwd')\n");
        let (_root, db) = project(&[
            ("caller.py", &source),
            ("pkg/__init__.py", initializer),
            ("pkg/facade.py", "from .api import patch\n"),
            ("pkg/api.py", "def patch(x):\n    return 'local'\n"),
        ]);
        assert_eq!(
            trace(&db, "entry", "sym:pkg/api.py#patch"),
            expected,
            "{initializer}; {caller}"
        );
        assert_eq!(
            bundle(&db, "entry")
                .related
                .iter()
                .any(|r| r.file_path == "pkg/api.py"),
            expected,
            "{initializer}; {caller}"
        );
    }
}

#[test]
fn python_cached_namespace_children_preserve_the_parent_attribute() {
    for (initializer, expected) in [
        (
            "import pkg.ns.api\nfrom unittest import mock as ns\n",
            false,
        ),
        ("from unittest import mock as ns\nimport pkg.ns.api\n", true),
    ] {
        let source = "import pkg.ns.api\ndef entry():\n    pkg.ns.api.patch('x')\n";
        let (_root, db) = project(&[
            ("caller.py", source),
            ("pkg/__init__.py", initializer),
            ("pkg/ns/api.py", "def patch(x):\n    return 'local'\n"),
        ]);
        assert_eq!(
            trace(&db, "entry", "sym:pkg/ns/api.py#patch"),
            expected,
            "{initializer}"
        );
    }
}

#[test]
fn python_assignment_attribute_captures_preserve_the_bound_receiver() {
    for (initializer, assignment, expected) in [
        (
            "from unittest import mock as api\n",
            "import pkg\ncaptured = pkg.api\n",
            None,
        ),
        (
            "from . import prior as api\n",
            "import pkg\ncaptured = pkg.api\n",
            Some("sym:pkg/prior.py#patch"),
        ),
        (
            "from unittest import mock as api\n",
            "import pkg\ncaptured = pkg\n",
            Some("sym:pkg/api.py#patch"),
        ),
        (
            "from unittest import mock as api\n",
            "from pkg import api\ncaptured = api\n",
            None,
        ),
    ] {
        let call = if assignment.ends_with("captured = pkg\n") {
            "captured.api.patch"
        } else {
            "captured.patch"
        };
        let source = format!("{assignment}import pkg.api\ndef entry():\n    {call}('os.getcwd')\n");
        let (_root, db) = project(&[
            ("caller.py", &source),
            ("pkg/__init__.py", initializer),
            ("pkg/api.py", "def patch(x):\n    return 'child'\n"),
            ("pkg/prior.py", "def patch(x):\n    return 'prior'\n"),
        ]);
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "patch".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        assert_eq!(
            refs.results
                .iter()
                .find(|r| r.from_file == "caller.py")
                .unwrap()
                .to
                .as_deref(),
            expected,
            "{source}; {initializer}"
        );
        for target in ["sym:pkg/api.py#patch", "sym:pkg/prior.py#patch"] {
            assert_eq!(
                trace(&db, "entry", target),
                expected == Some(target),
                "{source}; {initializer}"
            );
            assert_eq!(
                bundle(&db, "entry")
                    .related
                    .iter()
                    .any(|r| format!("sym:{}#patch", r.file_path) == target),
                expected == Some(target),
                "{source}; {initializer}"
            );
        }
    }
}

#[test]
fn python_class_global_bindings_do_not_replace_function_locals() {
    for (source, expected) in [
        (
            "from unittest.mock import patch as external\ndef entry():\n    from api import patch\n    class Bind:\n        global patch\n        patch = external\n    patch('x')\n",
            vec![(7, true)],
        ),
        (
            "from unittest.mock import patch\ndef entry():\n    from unittest.mock import patch\n    class Bind:\n        global patch\n        from api import patch\n    patch('os.getcwd')\n",
            vec![(7, false)],
        ),
        (
            "from unittest.mock import patch\ndef entry():\n    from api import patch\n    class Bind:\n        global patch\n        result = patch('os.getcwd')\n    return Bind.result\n",
            vec![(6, false)],
        ),
        (
            "from api import patch\ndef entry():\n    from unittest.mock import patch\n    class Bind:\n        global patch\n        result = patch('x')\n    return Bind.result\n",
            vec![(6, true)],
        ),
        (
            "from unittest.mock import patch as external\ndef entry():\n    from api import patch\n    class Bind:\n        global patch\n        patch = external\n        def run(self):\n            nonlocal patch\n            return patch('x')\n    return Bind().run()\n",
            vec![(9, true)],
        ),
    ] {
        let (_root, db) = project(&[
            ("caller.py", source),
            ("api.py", "def patch(x):\n    return 'local'\n"),
        ]);
        assert_call_targets(&db, source, &expected);
    }
}

#[test]
fn python_literal_wildcard_exports_load_missing_package_children() {
    for (initializer, expected) in [
        ("__all__ = ['api']\n", true),
        (
            "from unittest import mock as api\n__all__ = ['api']\n",
            false,
        ),
    ] {
        let source = "def entry():\n    return api.patch('os.getcwd')\nfrom pkg import *\n";
        let (_root, db) = project(&[
            ("caller.py", source),
            ("pkg/__init__.py", initializer),
            (
                "pkg/api.py",
                "from caller import entry\ndef patch(x):\n    return 'child'\n",
            ),
            (
                "tests/test_caller.py",
                "from caller import entry\ndef test_entry():\n    entry()\n",
            ),
        ]);
        assert_eq!(trace(&db, "entry", "sym:pkg/api.py#patch"), expected);
        assert_eq!(
            list_dependencies(&db, "pkg/api.py")
                .unwrap()
                .imported_by
                .contains(&"caller.py".into()),
            expected
        );
        assert_eq!(assess_risk(&db, "caller.py").unwrap().in_cycle, expected);
        assert_eq!(
            impact_analysis(
                &db,
                &ImpactRequest {
                    target: ImpactTarget::File {
                        path: "pkg/api.py".into()
                    },
                    depth: 2,
                    source_only: false
                }
            )
            .unwrap()
            .iter()
            .any(|r| r.file_path == "caller.py"),
            expected
        );
    }
}

#[test]
fn python_capture_stages_keep_saved_values_and_live_module_attributes() {
    for (prefix, calls) in [
        (
            "import pkg\ncaptured = pkg.api\nagain = captured.child\nimport pkg.api\nimport pkg.prior.child\n",
            "    captured.child.patch('x')\n    again.patch('os.getcwd')\n",
        ),
        (
            "from pkg import api\nimport pkg.api\nimport pkg.prior.child\n",
            "    api.child.patch('x')\n",
        ),
    ] {
        let source = format!("{prefix}def entry():\n{calls}");
        let (_root, db) = project(&[
            ("caller.py", &source),
            ("pkg/__init__.py", "from . import prior as api\n"),
            ("pkg/api.py", "def patch(x):\n    return 'wrong'\n"),
            (
                "pkg/prior/__init__.py",
                "from unittest import mock as child\n",
            ),
            ("pkg/prior/child.py", "def patch(x):\n    return 'child'\n"),
        ]);
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "patch".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        let calls: Vec<_> = refs
            .results
            .iter()
            .filter(|r| r.from_file == "caller.py")
            .collect();
        assert_eq!(
            calls[0].to.as_deref(),
            Some("sym:pkg/prior/child.py#patch"),
            "{source}"
        );
        if calls.len() == 2 {
            assert!(calls[1].to.is_none(), "{source}: {:?}", calls[1]);
        }
        assert!(trace(&db, "entry", "sym:pkg/prior/child.py#patch"));
        assert!(!trace(&db, "entry", "sym:pkg/api.py#patch"));
    }
}

#[test]
fn python_literal_wildcard_loading_preserves_each_export_and_scope() {
    for initializer in [
        "from unittest import mock as external\n__all__ = ('api', 'external')\n",
        "from unittest import mock as external\nclass Bind:\n    global __all__\n    __all__ = ('api', 'external')\n",
    ] {
        let source = "def entry():\n    return api.patch('x')\nfrom pkg import *\n";
        let (_root, db) = project(&[
            ("caller.py", source),
            ("pkg/__init__.py", initializer),
            (
                "pkg/api.py",
                "from caller import entry\ndef patch(x):\n    return 'child'\n",
            ),
            ("pkg/external.py", "def patch(x):\n    return 'wrong'\n"),
        ]);
        assert!(trace(&db, "entry", "sym:pkg/api.py#patch"));
        assert!(
            list_dependencies(&db, "pkg/api.py")
                .unwrap()
                .imported_by
                .contains(&"caller.py".into())
        );
        assert!(
            !list_dependencies(&db, "pkg/external.py")
                .unwrap()
                .imported_by
                .contains(&"caller.py".into())
        );
        assert!(assess_risk(&db, "caller.py").unwrap().in_cycle);
    }
}

#[test]
fn python_stub_import_bindings_use_the_same_authoritative_gate() {
    let (_root, db) = project(&[
        (
            "caller.pyi",
            "from unittest.mock import patch\ndef entry():\n    patch('os.getcwd')\n",
        ),
        (
            "view.py",
            "class View:\n    def patch(self, x):\n        return x\n",
        ),
    ]);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "patch".into(),
            kind: None,
        },
    )
    .unwrap();
    assert!(!refs.results.is_empty());
    assert!(refs.results.iter().all(|r| r.to.is_none()), "{refs:?}");
    assert!(!trace(&db, "entry", "sym:view.py#View.patch"));
    assert!(
        !bundle(&db, "entry")
            .related
            .iter()
            .any(|r| r.file_path == "view.py")
    );
}

#[test]
fn python_class_attributes_follow_the_recorded_class_namespace() {
    for (body, expected) in [
        (
            "    def patch(self, x):\n        return 'local'\n    patch = staticmethod(external)\n",
            false,
        ),
        (
            "    patch = staticmethod(external)\n    def patch(self, x):\n        return 'local'\n",
            true,
        ),
        (
            "    global patch\n    def patch(x):\n        return 'module'\n",
            false,
        ),
    ] {
        let source = format!(
            "from unittest.mock import patch as external\nclass View:\n{body}    def entry(self):\n        return self.patch('os.getcwd')\n"
        );
        let (_root, db) = project(&[("caller.py", &source)]);
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "patch".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        assert_eq!(refs.results.len(), 1, "{source}: {refs:?}");
        assert_eq!(
            refs.results[0].to.as_deref(),
            expected.then_some("sym:caller.py#View.patch"),
            "{source}"
        );
        assert_eq!(
            trace(&db, "sym:caller.py#View.entry", "sym:caller.py#View.patch"),
            expected,
            "{source}"
        );
    }
    for (body, expected) in [
        (
            "    def patch(x):\n        return 'local'\n    patch = staticmethod(external)\n",
            false,
        ),
        (
            "    patch = staticmethod(external)\n    def patch(x):\n        return 'local'\n",
            true,
        ),
    ] {
        let source = format!("from unittest.mock import patch as external\nclass View:\n{body}");
        let (_root, db) = project(&[
            ("view.py", &source),
            (
                "caller.py",
                "from view import View\ndef entry():\n    return View.patch('os.getcwd')\n",
            ),
        ]);
        assert_eq!(
            trace(&db, "entry", "sym:view.py#View.patch"),
            expected,
            "{source}"
        );
    }
}

#[test]
fn python_captured_classes_keep_their_declaration_namespace() {
    let (_root, db) = project(&[(
        "caller.py",
        "class View:\n    def patch(x):\n        return 'old'\nPrior = View\nclass View:\n    def patch(x):\n        return 'new'\ndef entry():\n    return Prior.patch('x'), View.patch('x')\n",
    )]);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "patch".into(),
            kind: Some(ReferenceKind::Call),
        },
    )
    .unwrap();
    let targets: Vec<_> = refs.results.iter().map(|r| r.to.as_deref()).collect();
    assert_eq!(
        targets,
        [
            Some("sym:caller.py#View.patch@2"),
            Some("sym:caller.py#View.patch@6")
        ]
    );
    assert!(trace(&db, "entry", "sym:caller.py#View.patch@2"));
    assert!(trace(&db, "entry", "sym:caller.py#View.patch@6"));
}

#[test]
fn python_from_directives_share_actual_child_load_state_with_live_receivers() {
    for (initializer, directive, external, expected, child_edge) in [
        (
            "from . import prior as api\n",
            "from pkg.api import patch as p",
            true,
            None,
            true,
        ),
        (
            "from . import prior as api\n",
            "from pkg.api import patch as p",
            false,
            Some("sym:pkg/api.py#patch"),
            true,
        ),
        (
            "__all__ = ['api']\n",
            "from pkg import api",
            false,
            Some("sym:pkg/api.py#patch"),
            true,
        ),
        (
            "__all__ = ['api']\n",
            "from pkg import *",
            false,
            Some("sym:pkg/api.py#patch"),
            true,
        ),
        (
            "from . import prior as api\n",
            "from pkg import api",
            false,
            Some("sym:pkg/prior.py#patch"),
            false,
        ),
        (
            "from . import api\nfrom . import prior as api\n",
            "from pkg.api import patch as p",
            false,
            Some("sym:pkg/prior.py#patch"),
            true,
        ),
        (
            "__all__ = ['api']\nfrom . import *\nfrom . import prior as api\n",
            "from pkg.api import patch as p",
            false,
            Some("sym:pkg/prior.py#patch"),
            true,
        ),
        (
            "__all__ = ['api']\nfrom . import *\nfrom unittest import mock as api\n",
            "from pkg.api import patch as p",
            false,
            None,
            true,
        ),
        (
            "__all__ = ['api']\nfrom . import *\n",
            "from pkg.api import patch as p",
            false,
            Some("sym:pkg/api.py#patch"),
            true,
        ),
        (
            "from . import prior as api\nfrom . import *\n",
            "from pkg import api",
            false,
            Some("sym:pkg/prior.py#patch"),
            false,
        ),
    ] {
        let source = format!(
            "def entry():\n    return pkg.api.patch('os.getcwd')\nimport pkg\n{directive}\n"
        );
        let child = if external {
            "from caller import entry\nfrom unittest.mock import patch\n"
        } else {
            "from caller import entry\ndef patch(x):\n    return 'child'\n"
        };
        let (_root, db) = project(&[
            ("caller.py", &source),
            ("pkg/__init__.py", initializer),
            ("pkg/api.py", child),
            ("pkg/prior.py", "def patch(x):\n    return 'prior'\n"),
            (
                "tests/test_caller.py",
                "from caller import entry\ndef test_entry():\n    return entry()\n",
            ),
        ]);
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "patch".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        let row = refs
            .results
            .iter()
            .find(|r| r.from_file == "caller.py")
            .unwrap();
        assert_eq!(row.to.as_deref(), expected, "{source}; {initializer}");
        for file in ["pkg/prior.py", "pkg/api.py"] {
            if external && file == "pkg/api.py" {
                continue;
            }
            let target = format!("sym:{file}#patch");
            assert_eq!(
                trace(&db, "entry", &target),
                expected == Some(target.as_str()),
                "{source}; {initializer}"
            );
            let impact = impact_analysis(
                &db,
                &ImpactRequest {
                    target: ImpactTarget::Symbol { name: target },
                    depth: 2,
                    source_only: false,
                },
            )
            .unwrap();
            assert_eq!(
                impact.iter().any(|r| r.file_path == "caller.py"
                    && r.reasons
                        .iter()
                        .any(|reason| reason.kind == ReferenceKind::Call)),
                expected == Some(format!("sym:{file}#patch").as_str()),
                "{source}: {impact:?}"
            );
        }
        assert_eq!(
            list_dependencies(&db, "pkg/api.py")
                .unwrap()
                .imported_by
                .contains(&"caller.py".into()),
            child_edge,
            "{source}; {initializer}"
        );
        assert_eq!(
            assess_risk(&db, "caller.py").unwrap().in_cycle,
            child_edge,
            "{source}; {initializer}"
        );
        let context = bundle(&db, "entry");
        assert_eq!(
            context
                .related
                .iter()
                .any(|r| r.file_path == "pkg/prior.py"),
            expected == Some("sym:pkg/prior.py#patch"),
            "{source}: {context:?}"
        );
        let tests = recommend_tests_with_reachability(
            &db,
            &["pkg/api.py".into()],
            &ReachabilityOptions::default(),
        )
        .unwrap();
        assert_eq!(
            tests
                .reachable
                .iter()
                .any(|r| r.path == "tests/test_caller.py"),
            child_edge,
            "{source}: {tests:?}"
        );
    }
}

#[test]
fn python_parenthesized_and_chained_aliases_keep_the_terminal_rhs_binding() {
    for external in [false, true] {
        for assignment in [
            "patch = actual",
            "(patch) = actual",
            "patch = (actual)",
            "patch = ((actual))",
            "patch = other = actual",
            "other = patch = actual",
            "patch = other = ((actual))",
        ] {
            let source = format!(
                "from {} import patch as actual\n{assignment}\ndef entry():\n    return patch('os.getcwd')\n",
                if external { "unittest.mock" } else { "api" }
            );
            let (_root, db) = project(&[
                ("caller.py", &source),
                ("api.py", "def patch(x):\n    return 'local'\n"),
            ]);
            let refs = find_references(
                &db,
                &FindReferencesRequest {
                    symbol_name: "patch".into(),
                    kind: Some(ReferenceKind::Call),
                },
            )
            .unwrap();
            assert_eq!(
                refs.results[0].to.as_deref(),
                (!external).then_some("sym:api.py#patch"),
                "{source}"
            );
            assert_eq!(
                trace(&db, "entry", "sym:api.py#patch"),
                !external,
                "{source}"
            );
            assert_eq!(
                bundle(&db, "entry")
                    .related
                    .iter()
                    .any(|r| r.file_path == "api.py"),
                !external,
                "{source}"
            );
        }
    }
}

#[test]
fn python_from_imports_preserve_captured_receivers_and_relative_origins() {
    for (caller, directive) in [
        ("caller.py", "from pkg.api import patch as p"),
        ("pkg/client.py", "from .api import patch as p"),
    ] {
        let source = format!(
            "import pkg\ncaptured = other = (pkg.api)\n{directive}\ndef entry():\n    captured.patch('x')\n    pkg.api.patch('x')\n"
        );
        let (_root, db) = project(&[
            (caller, &source),
            ("pkg/__init__.py", "from . import prior as api\n"),
            ("pkg/prior.py", "def patch(x):\n    return 'prior'\n"),
            ("pkg/api.py", "def patch(x):\n    return 'child'\n"),
        ]);
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "patch".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        let rows: Vec<_> = refs
            .results
            .iter()
            .filter(|row| row.from_file == caller)
            .map(|row| row.to.as_deref())
            .collect();
        assert_eq!(
            rows,
            [Some("sym:pkg/prior.py#patch"), Some("sym:pkg/api.py#patch")],
            "{source}"
        );
        assert!(trace(&db, "entry", "sym:pkg/prior.py#patch"));
        assert!(trace(&db, "entry", "sym:pkg/api.py#patch"));
    }
}

#[test]
fn python_assignment_alias_normalization_preserves_unknown_return_values() {
    for assignment in ["patch = ((choose()))", "patch = other = choose()"] {
        let source = format!(
            "from api import patch as actual\ndef choose():\n    return actual\n{assignment}\ndef entry():\n    return patch('x')\n"
        );
        let (_root, db) = project(&[
            ("caller.py", &source),
            ("api.py", "def patch(x):\n    return 'local'\n"),
        ]);
        let refs = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "patch".into(),
                kind: Some(ReferenceKind::Call),
            },
        )
        .unwrap();
        assert!(
            refs.results.iter().all(|row| row.to.is_none()),
            "{source}: {refs:?}"
        );
        assert!(!trace(&db, "entry", "sym:api.py#patch"));
    }
}

#[test]
fn python_initializer_deletion_supersedes_an_earlier_wildcard_attribute() {
    let (_root, db) = project(&[
        (
            "caller.py",
            "import pkg\nfrom pkg.api import patch as p\ndef entry():\n    return pkg.api.patch('x')\n",
        ),
        (
            "pkg/__init__.py",
            "from .prior import *\ndel api\nfrom . import api\n",
        ),
        (
            "pkg/prior.py",
            "from unittest import mock as api\n__all__ = ['api']\n",
        ),
        ("pkg/api.py", "def patch(x):\n    return 'child'\n"),
    ]);
    let imported = list_dependencies(&db, "pkg/api.py").unwrap().imported_by;
    assert!(imported.contains(&"pkg/__init__.py".into()), "{imported:?}");
    assert!(imported.contains(&"caller.py".into()), "{imported:?}");
    assert!(trace(&db, "entry", "sym:pkg/api.py#patch"));
}

#[test]
fn python_self_package_from_aliases_capture_the_loaded_child() {
    for external in [false, true] {
        for initializer in [
            "from . import api as captured\nfrom . import prior as api\n",
            "from . import api as captured\ndef api(x):\n    return 'wrong'\n",
        ] {
            let (_root, db) = project(&[
                (
                    "caller.py",
                    "from pkg import captured\ndef entry():\n    return captured.patch('os.getcwd')\n",
                ),
                ("pkg/__init__.py", initializer),
                (
                    "pkg/api.py",
                    if external {
                        "from unittest.mock import patch\n"
                    } else {
                        "def patch(x):\n    return 'child'\n"
                    },
                ),
                ("pkg/prior.py", "def patch(x):\n    return 'prior'\n"),
            ]);
            let refs = find_references(
                &db,
                &FindReferencesRequest {
                    symbol_name: "patch".into(),
                    kind: Some(ReferenceKind::Call),
                },
            )
            .unwrap();
            assert_eq!(
                refs.results[0].to.as_deref(),
                (!external).then_some("sym:pkg/api.py#patch"),
                "{refs:?}"
            );
            assert!(!trace(&db, "entry", "sym:pkg/prior.py#patch"));
            let bindings = find_references(
                &db,
                &FindReferencesRequest {
                    symbol_name: "captured".into(),
                    kind: Some(ReferenceKind::ImportBinding),
                },
            )
            .unwrap();
            assert!(!bindings.results.is_empty());
            assert!(
                bindings.results.iter().all(|row| row.to.is_none()),
                "{bindings:?}"
            );
        }
    }
}

#[test]
fn python_module_getters_require_a_live_binding_to_suppress_child_imports() {
    for external in [false, true] {
        for (initializer, active) in [
            ("", false),
            (
                "def __getattr__(name):\n    raise AttributeError(name)\ndel __getattr__\n",
                false,
            ),
            ("__getattr__: object\n", false),
            (
                "def __getattr__(name):\n    raise AttributeError(name)\ndel __getattr__\n__getattr__: object\n",
                false,
            ),
            (
                "def __getattr__(name):\n    raise AttributeError(name)\n",
                true,
            ),
            (
                "def __getattr__(name):\n    raise AttributeError(name)\n__getattr__: object\n",
                true,
            ),
            (
                "def __getattr__(name):\n    raise AttributeError(name)\ndel __getattr__\ndef __getattr__(name):\n    raise AttributeError(name)\n",
                true,
            ),
            (
                "def getter(name):\n    raise AttributeError(name)\n__getattr__ = getter\n",
                true,
            ),
        ] {
            for (directive, call, explicit) in [
                ("from pkg import api", "api.patch", false),
                ("import pkg.api as api", "api.patch", true),
                ("from pkg.api import patch", "patch", true),
            ] {
                let source = format!("def entry():\n    return {call}('os.getcwd')\n{directive}\n");
                let child = if external {
                    "from caller import entry\nfrom unittest.mock import patch\n"
                } else {
                    "from caller import entry\ndef patch(x):\n    return 'child'\n"
                };
                let (_root, db) = project(&[
                    ("caller.py", &source),
                    ("pkg/__init__.py", initializer),
                    ("pkg/api.py", child),
                    ("view.py", "def patch(x):\n    return 'wrong'\n"),
                    (
                        "tests/test_caller.py",
                        "from caller import entry\ndef test_entry():\n    return entry()\n",
                    ),
                ]);
                let loaded = explicit || !active;
                let resolved = loaded && !external;
                let refs = find_references(
                    &db,
                    &FindReferencesRequest {
                        symbol_name: "patch".into(),
                        kind: Some(ReferenceKind::Call),
                    },
                )
                .unwrap();
                let row = refs
                    .results
                    .iter()
                    .find(|row| row.from_file == "caller.py")
                    .unwrap();
                assert_eq!(
                    row.to.as_deref(),
                    resolved.then_some("sym:pkg/api.py#patch"),
                    "{initializer}; {source}"
                );
                assert!(!trace(&db, "entry", "sym:view.py#patch"));
                if !external {
                    assert_eq!(trace(&db, "entry", "sym:pkg/api.py#patch"), resolved);
                    let impact = impact_analysis(
                        &db,
                        &ImpactRequest {
                            target: ImpactTarget::Symbol {
                                name: "sym:pkg/api.py#patch".into(),
                            },
                            depth: 2,
                            source_only: false,
                        },
                    )
                    .unwrap();
                    assert_eq!(
                        impact.iter().any(|row| row.file_path == "caller.py"
                            && row
                                .reasons
                                .iter()
                                .any(|reason| reason.kind == ReferenceKind::Call)),
                        resolved,
                        "{initializer}; {source}: {impact:?}"
                    );
                }
                let imported = list_dependencies(&db, "pkg/api.py").unwrap().imported_by;
                assert_eq!(imported.contains(&"caller.py".into()), loaded);
                let pairs = file_import_pairs(&db).unwrap();
                assert_eq!(
                    pairs
                        .eager
                        .contains(&("caller.py".into(), "pkg/api.py".into())),
                    loaded
                );
                assert_eq!(assess_risk(&db, "caller.py").unwrap().in_cycle, loaded);
                let impact = impact_analysis(
                    &db,
                    &ImpactRequest {
                        target: ImpactTarget::File {
                            path: "pkg/api.py".into(),
                        },
                        depth: 2,
                        source_only: false,
                    },
                )
                .unwrap();
                assert_eq!(
                    impact.iter().any(|row| row.file_path == "caller.py"),
                    loaded
                );
                let context = bundle(&db, "entry");
                assert_eq!(
                    context
                        .related
                        .iter()
                        .any(|row| row.file_path == "pkg/api.py"),
                    resolved
                );
                assert!(!context.related.iter().any(|row| row.file_path == "view.py"));
                let tests = recommend_tests_with_reachability(
                    &db,
                    &["pkg/api.py".into()],
                    &ReachabilityOptions::default(),
                )
                .unwrap();
                assert_eq!(
                    tests
                        .reachable
                        .iter()
                        .any(|row| row.path == "tests/test_caller.py"),
                    loaded
                );
            }
        }
    }
}

#[test]
fn python_export_list_annotations_preserve_the_prior_binding_and_membership() {
    for external in [false, true] {
        for (declaration, state, included) in [
            ("", "absent", true),
            ("__all__: object\n", "unbound", true),
            ("__all__ = ['patch']\ndel __all__\n", "deleted", true),
            (
                "__all__ = ['patch']\ndel __all__\n__all__: object\n",
                "deleted",
                true,
            ),
            ("__all__ = ['patch']\n", "literal", true),
            ("__all__ = ['patch']\n__all__: object\n", "literal", true),
            ("(__all__) = ['patch']\n__all__: object\n", "literal", true),
            ("__all__: object = ['patch']\n", "literal", true),
            ("__all__ = ('patch',)\n__all__: object\n", "literal", true),
            (
                "exports = __all__ = ['patch']\n__all__: object\n",
                "literal",
                true,
            ),
            ("__all__ = []\n", "empty", false),
            ("__all__ = []\n__all__: object\n", "empty", false),
            (
                "def choose():\n    return ['patch']\n__all__ = choose()\n__all__: object\n",
                "unknown",
                false,
            ),
        ] {
            let origin = if external { "unittest.mock" } else { ".api" };
            let initializer = format!("from {origin} import patch\n{declaration}");
            let source = "from fallback import patch\nfrom pkg import *\ndef entry():\n    return patch('os.getcwd')\n";
            let (_root, db) = project(&[
                ("caller.py", source),
                ("pkg/__init__.py", &initializer),
                ("pkg/api.py", "def patch(x):\n    return 'child'\n"),
                ("fallback.py", "from unittest.mock import patch\n"),
                (
                    "tests/test_caller.py",
                    "from caller import entry\ndef test_entry():\n    return entry()\n",
                ),
                ("view.py", "def patch(x):\n    return 'wrong'\n"),
            ]);
            let all = db
                .python_export_binding("pkg/__init__.py", "__all__")
                .unwrap();
            match state {
                "absent" => assert!(all.is_none()),
                "unbound" => {
                    let all = all.unwrap();
                    assert!(!all.bound && !all.deleted && all.export_names.is_none());
                }
                "deleted" => assert!(all.unwrap().deleted),
                "literal" => {
                    let all = all.unwrap();
                    assert!(all.bound && !all.deleted);
                    assert_eq!(all.export_names, Some(vec!["patch".into()]));
                }
                "empty" => {
                    let all = all.unwrap();
                    assert!(all.bound && !all.deleted);
                    assert_eq!(all.export_names, Some(Vec::new()));
                }
                "unknown" => {
                    let all = all.unwrap();
                    assert!(all.bound && !all.deleted && all.export_names.is_none());
                }
                _ => unreachable!(),
            }
            let resolved = included && !external;
            let refs = find_references(
                &db,
                &FindReferencesRequest {
                    symbol_name: "patch".into(),
                    kind: Some(ReferenceKind::Call),
                },
            )
            .unwrap();
            let row = refs
                .results
                .iter()
                .find(|row| row.from_file == "caller.py")
                .unwrap();
            assert_eq!(
                row.to.as_deref(),
                resolved.then_some("sym:pkg/api.py#patch"),
                "{initializer}: {refs:?}"
            );
            assert_eq!(trace(&db, "entry", "sym:pkg/api.py#patch"), resolved);
            assert!(!trace(&db, "entry", "sym:view.py#patch"));
            let impact = impact_analysis(
                &db,
                &ImpactRequest {
                    target: ImpactTarget::Symbol {
                        name: "sym:pkg/api.py#patch".into(),
                    },
                    depth: 2,
                    source_only: false,
                },
            )
            .unwrap();
            assert_eq!(
                impact.iter().any(|row| row.file_path == "caller.py"
                    && row
                        .reasons
                        .iter()
                        .any(|reason| reason.kind == ReferenceKind::Call)),
                resolved,
                "{initializer}: {impact:?}"
            );
            let imported = list_dependencies(&db, "pkg/api.py").unwrap().imported_by;
            assert_eq!(imported.contains(&"pkg/__init__.py".into()), !external);
            assert!(!imported.contains(&"caller.py".into()));
            assert!(!assess_risk(&db, "caller.py").unwrap().in_cycle);
            let context = bundle(&db, "entry");
            assert_eq!(
                context
                    .related
                    .iter()
                    .any(|row| row.file_path == "pkg/api.py"),
                resolved
            );
            assert!(!context.related.iter().any(|row| row.file_path == "view.py"));
            let tests = recommend_tests_with_reachability(
                &db,
                &["pkg/api.py".into()],
                &ReachabilityOptions::default(),
            )
            .unwrap();
            assert_eq!(
                tests
                    .reachable
                    .iter()
                    .any(|row| row.path == "tests/test_caller.py"),
                resolved
            );
        }
    }
}

#[test]
fn python_missing_package_attributes_honor_live_getters_before_child_fallback() {
    let absent = ("", "missing");
    let annotation = ("api: object\n", "missing");
    let deleted = ("api = None\ndel api\n", "missing");
    let deleted_annotation = ("api = None\ndel api\napi: object\n", "missing");
    let internal = ("api = provider\n", "internal");
    let external = ("api = mock\n", "external");
    let unknown = (
        "def choose_api():\n    return provider\napi = choose_api()\n",
        "unknown",
    );
    let no_getter = ("", false);
    let unbound_getter = ("__getattr__: object\n", false);
    let deleted_getter = (
        "def __getattr__(name):\n    return mock\ndel __getattr__\n",
        false,
    );
    let deleted_getter_annotation = (
        "def __getattr__(name):\n    return mock\ndel __getattr__\n__getattr__: object\n",
        false,
    );
    let external_getter = ("def __getattr__(name):\n    return mock\n", true);
    let internal_getter = ("def __getattr__(name):\n    return provider\n", true);
    let unknown_getter = (
        "def getter(name):\n    return mock\ndef choose_getter():\n    return getter\n__getattr__ = choose_getter()\n",
        true,
    );
    let noncallable_getter = ("__getattr__ = None\n", true);
    let named = ("", "from pkg import api", "api.patch", false, false, true);
    let plain_child = ("", "import pkg.api as api", "api.patch", true, false, true);
    let explicit_child = ("", "from pkg.api import patch", "patch", true, false, true);
    let plain_parent = ("", "import pkg", "pkg.api.patch", false, false, false);
    let self_named = (
        "from . import api as captured\n",
        "from pkg import captured",
        "captured.patch",
        false,
        true,
        true,
    );
    let self_plain = (
        "import pkg.api as captured\n",
        "from pkg import captured",
        "captured.patch",
        true,
        true,
        true,
    );
    let self_explicit = (
        "from .api import patch\n",
        "from pkg import patch",
        "patch",
        true,
        true,
        true,
    );
    let wildcard = (
        "__all__ = ['api']\n",
        "from pkg import *",
        "api.patch",
        false,
        false,
        true,
    );
    let self_wildcard = (
        "__all__ = ['api']\nfrom . import *\ncaptured = api\n",
        "from pkg import captured",
        "captured.patch",
        false,
        true,
        true,
    );
    let groups = [
        (
            vec![annotation, deleted],
            vec![deleted_getter, external_getter],
            vec![named, self_named, wildcard, self_wildcard],
        ),
        (
            vec![absent, deleted_annotation],
            vec![unbound_getter, deleted_getter_annotation],
            vec![named, self_named],
        ),
        (
            vec![internal, external, unknown],
            vec![external_getter, noncallable_getter],
            vec![named, self_named, wildcard, self_wildcard],
        ),
        (
            vec![annotation, deleted],
            vec![internal_getter, unknown_getter, noncallable_getter],
            vec![named, self_named],
        ),
        (
            vec![annotation],
            vec![external_getter],
            vec![plain_child, explicit_child, self_plain, self_explicit],
        ),
        (
            vec![external],
            vec![noncallable_getter],
            vec![plain_child, explicit_child, self_plain, self_explicit],
        ),
        (
            vec![annotation],
            vec![no_getter, external_getter, noncallable_getter],
            vec![plain_parent],
        ),
        (
            vec![internal, unknown],
            vec![external_getter],
            vec![plain_parent],
        ),
    ];
    let mut fixtures = 0;
    for (attributes, getters, modes) in groups {
        for (attribute, present) in attributes {
            for &(getter, active) in &getters {
                for &(self_import, directive, call, explicit, inside, missing_load) in &modes {
                    let initializer = format!(
                        "from unittest import mock\nfrom . import prior as provider\n{attribute}{getter}{self_import}"
                    );
                    let source =
                        format!("def entry():\n    return {call}('os.getcwd')\n{directive}\n");
                    let (_root, db) = project(&[
                        ("caller.py", &source),
                        ("pkg/__init__.py", &initializer),
                        (
                            "pkg/api.py",
                            "from caller import entry\ndef patch(x):\n    return 'child'\n",
                        ),
                        ("pkg/prior.py", "def patch(x):\n    return 'prior'\n"),
                        ("view.py", "def patch(x):\n    return 'wrong'\n"),
                        (
                            "tests/test_caller.py",
                            "from caller import entry\ndef test_entry():\n    return entry()\n",
                        ),
                    ]);
                    let loaded = explicit || (missing_load && present == "missing" && !active);
                    let expected = if loaded {
                        Some("sym:pkg/api.py#patch")
                    } else if present == "internal" {
                        Some("sym:pkg/prior.py#patch")
                    } else {
                        None
                    };
                    let refs = find_references(
                        &db,
                        &FindReferencesRequest {
                            symbol_name: "patch".into(),
                            kind: Some(ReferenceKind::Call),
                        },
                    )
                    .unwrap();
                    let rows = refs
                        .results
                        .iter()
                        .filter(|row| row.from_file == "caller.py")
                        .collect::<Vec<_>>();
                    assert_eq!(rows.len(), 1, "{initializer}; {source}: {refs:?}");
                    assert_eq!(
                        rows[0].to.as_deref(),
                        expected,
                        "{initializer}; {source}: {refs:?}"
                    );
                    for target in [
                        "sym:pkg/api.py#patch",
                        "sym:pkg/prior.py#patch",
                        "sym:view.py#patch",
                    ] {
                        let resolved = Some(target) == expected;
                        assert_eq!(
                            trace(&db, "entry", target),
                            resolved,
                            "{initializer}; {source}; {target}"
                        );
                        let impact = impact_analysis(
                            &db,
                            &ImpactRequest {
                                target: ImpactTarget::Symbol {
                                    name: target.into(),
                                },
                                depth: 2,
                                source_only: false,
                            },
                        )
                        .unwrap();
                        assert_eq!(
                            impact.iter().any(|row| row.file_path == "caller.py"
                                && row
                                    .reasons
                                    .iter()
                                    .any(|reason| reason.kind == ReferenceKind::Call)),
                            resolved,
                            "{initializer}; {source}; {target}: {impact:?}"
                        );
                    }
                    let deps = list_dependencies(&db, "pkg/api.py").unwrap();
                    assert_eq!(
                        deps.imported_by,
                        if loaded {
                            vec![if inside {
                                "pkg/__init__.py".to_string()
                            } else {
                                "caller.py".to_string()
                            }]
                        } else {
                            Vec::new()
                        },
                        "{initializer}; {source}: {deps:?}"
                    );
                    let pairs = file_import_pairs(&db).unwrap();
                    assert_eq!(
                        pairs.eager.contains(&(
                            if inside {
                                "pkg/__init__.py".into()
                            } else {
                                "caller.py".into()
                            },
                            "pkg/api.py".into()
                        )),
                        loaded,
                        "{initializer}; {source}: {pairs:?}"
                    );
                    assert_eq!(
                        assess_risk(&db, "caller.py").unwrap().in_cycle,
                        loaded,
                        "{initializer}; {source}"
                    );
                    let impact = impact_analysis(
                        &db,
                        &ImpactRequest {
                            target: ImpactTarget::File {
                                path: "pkg/api.py".into(),
                            },
                            depth: 2,
                            source_only: false,
                        },
                    )
                    .unwrap();
                    assert_eq!(
                        impact.iter().any(|row| row.file_path == "caller.py"),
                        loaded,
                        "{initializer}; {source}: {impact:?}"
                    );
                    let context = bundle(&db, "entry");
                    assert_eq!(
                        context
                            .related
                            .iter()
                            .any(|row| row.file_path == "pkg/api.py"),
                        loaded,
                        "{initializer}; {source}: {context:?}"
                    );
                    assert!(!context.related.iter().any(|row| row.file_path == "view.py"));
                    let callers = export_context_for_symbol(
                        &db,
                        "sym:pkg/api.py#patch",
                        &ExportRequest {
                            query: None,
                            symbol: Some("sym:pkg/api.py#patch".into()),
                            include_callers: true,
                            include_callees: false,
                            limit: 20,
                        },
                    )
                    .unwrap();
                    assert_eq!(
                        callers
                            .related
                            .iter()
                            .any(|row| row.file_path == "caller.py"),
                        loaded,
                        "{initializer}; {source}: {callers:?}"
                    );
                    let tests = recommend_tests_with_reachability(
                        &db,
                        &["pkg/api.py".into()],
                        &ReachabilityOptions::default(),
                    )
                    .unwrap();
                    assert_eq!(
                        tests
                            .reachable
                            .iter()
                            .any(|row| row.path == "tests/test_caller.py"),
                        loaded,
                        "{initializer}; {source}: {tests:?}"
                    );
                    fixtures += 1;
                }
            }
        }
    }
    assert_eq!(fixtures, 73);
}

#[test]
fn python_self_import_getter_membership_keeps_captured_values_and_actual_loads() {
    for advertised in [false, true] {
        for annotated in [false, true] {
            for later in [false, true] {
                let prior = format!(
                    "from unittest import mock\napi = mock\ndef __getattr__(name):\n    return mock\n__all__ = {}\n",
                    if advertised {
                        "['api', '__getattr__']"
                    } else {
                        "['api']"
                    }
                );
                let initializer = format!(
                    "from .prior import *\ndel api\n{}from . import api as captured\n{}",
                    if annotated { "api: object\n" } else { "" },
                    if later {
                        "from .api import patch\nfrom .prior import api\n"
                    } else {
                        ""
                    }
                );
                let (_root, db) = project(&[
                    (
                        "caller.py",
                        "from pkg import captured\ndef entry():\n    return captured.patch('os.getcwd')\n",
                    ),
                    ("pkg/__init__.py", &initializer),
                    ("pkg/prior.py", &prior),
                    ("pkg/api.py", "def patch(x):\n    return 'child'\n"),
                    ("view.py", "def patch(x):\n    return 'wrong'\n"),
                    (
                        "tests/test_caller.py",
                        "from caller import entry\ndef test_entry():\n    return entry()\n",
                    ),
                ]);
                let refs = find_references(
                    &db,
                    &FindReferencesRequest {
                        symbol_name: "patch".into(),
                        kind: Some(ReferenceKind::Call),
                    },
                )
                .unwrap();
                let row = refs
                    .results
                    .iter()
                    .find(|row| row.from_file == "caller.py")
                    .unwrap();
                assert_eq!(
                    row.to.as_deref(),
                    (!advertised).then_some("sym:pkg/api.py#patch"),
                    "{prior}; {initializer}: {refs:?}"
                );
                assert_eq!(
                    trace(&db, "entry", "sym:pkg/api.py#patch"),
                    !advertised,
                    "{prior}; {initializer}"
                );
                assert!(!trace(&db, "entry", "sym:view.py#patch"));
                let imported = list_dependencies(&db, "pkg/api.py").unwrap().imported_by;
                assert_eq!(
                    imported,
                    if later || !advertised {
                        vec!["pkg/__init__.py".to_string()]
                    } else {
                        Vec::new()
                    },
                    "{prior}; {initializer}: {imported:?}"
                );
                let impact = impact_analysis(
                    &db,
                    &ImpactRequest {
                        target: ImpactTarget::Symbol {
                            name: "sym:pkg/api.py#patch".into(),
                        },
                        depth: 2,
                        source_only: false,
                    },
                )
                .unwrap();
                assert_eq!(
                    impact.iter().any(|row| row.file_path == "caller.py"
                        && row
                            .reasons
                            .iter()
                            .any(|reason| reason.kind == ReferenceKind::Call)),
                    !advertised,
                    "{prior}; {initializer}: {impact:?}"
                );
                let context = bundle(&db, "entry");
                assert_eq!(
                    context
                        .related
                        .iter()
                        .any(|row| row.file_path == "pkg/api.py"),
                    !advertised,
                    "{prior}; {initializer}: {context:?}"
                );
                let tests = recommend_tests_with_reachability(
                    &db,
                    &["pkg/api.py".into()],
                    &ReachabilityOptions::default(),
                )
                .unwrap();
                assert_eq!(
                    tests
                        .reachable
                        .iter()
                        .any(|row| row.path == "tests/test_caller.py"),
                    !advertised,
                    "{prior}; {initializer}: {tests:?}"
                );
            }
        }
    }
}
