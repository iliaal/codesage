use codesage_graph::{assess_risk, find_references, full_index, impact_analysis, trace_call_path};
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

fn workspace(caller: &str) -> (tempfile::TempDir, Database) {
    indexed(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = ['crates/graph', 'crates/cli', 'crates/decoy']\n[workspace.dependencies]\ncodesage-graph = { path = 'crates/graph' }\n",
        ),
        (
            "crates/graph/Cargo.toml",
            "[package]\nname = 'codesage-graph'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "crates/cli/Cargo.toml",
            "[package]\nname = 'codesage'\nversion = '0.0.0'\nedition = '2024'\n[dependencies]\ncodesage-graph.workspace = true\n",
        ),
        (
            "crates/decoy/Cargo.toml",
            "[package]\nname = 'decoy'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "crates/graph/src/lib.rs",
            "mod search;\npub use search::{search_page};\n",
        ),
        ("crates/graph/src/search.rs", "pub fn search_page() {}\n"),
        ("crates/decoy/src/lib.rs", "pub fn search_page() {}\n"),
        ("crates/cli/src/main.rs", caller),
    ])
}

fn reaches(db: &Database, from: &str, to: &str) -> bool {
    trace_call_path(
        db,
        &CallPathRequest {
            from: from.into(),
            to: to.into(),
            max_depth: 3,
        },
    )
    .unwrap()
    .found
}

#[test]
fn external_type_imports_do_not_bind_to_local_methods() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n[dependencies]\nregistry = '1'\n",
        ),
        (
            "src/lib.rs",
            "mod options; mod std_call; mod registry_call; mod local_call; mod shadowed;",
        ),
        (
            "src/options.rs",
            "pub struct Options;\nimpl Options { pub fn new() {} }\n",
        ),
        (
            "src/std_call.rs",
            "use ::std::fs::{OpenOptions as Options};\nfn from_std() { Options::new(); }\nmod inner {\nuse super::*;\nuse std::fs::OpenOptions as Options;\nfn explicit_over_glob() { Options::new(); }\n}",
        ),
        (
            "src/registry_call.rs",
            "use registry::Options;\nfn from_registry() { Options::new(); }\n",
        ),
        (
            "src/local_call.rs",
            "use crate::options::Options;\nfn from_local() { Options::new(); }\n",
        ),
        (
            "src/shadowed.rs",
            "use std::fs::OpenOptions as Options;\nfn from_outer() { Options::new(); }\nmod inner {\nuse crate::options::Options;\nfn from_nested() { Options::new(); }\n}\nmod globbed {\nuse super::*;\nfn from_glob() { Options::new(); }\n}\n",
        ),
    ]);
    let target = "sym:src/options.rs#Options::new";
    let references = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "Options::new".into(),
            kind: Some(codesage_protocol::ReferenceKind::Call),
        },
    )
    .unwrap();
    let bindings: std::collections::BTreeMap<_, _> = references
        .results
        .iter()
        .map(|row| (row.from_symbol.as_deref().unwrap(), row.to.as_deref()))
        .collect();
    assert_eq!(
        bindings,
        [
            ("explicit_over_glob", None),
            ("from_glob", None),
            ("from_local", Some(target)),
            ("from_registry", None),
            ("from_nested", Some(target)),
            ("from_outer", None),
            ("from_std", None),
        ]
        .into()
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
    assert_eq!(
        impact
            .iter()
            .map(|row| row.file_path.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        ["src/local_call.rs", "src/shadowed.rs"].into()
    );
    assert!(reaches(&db, "from_local", target));
    assert!(reaches(&db, "from_nested", target));
    assert!(!reaches(&db, "from_outer", target));
    assert!(!reaches(&db, "from_glob", target));
    assert!(!reaches(&db, "explicit_over_glob", target));
    assert!(!reaches(&db, "from_std", target));
    assert!(!reaches(&db, "from_registry", target));
}

#[test]
fn external_type_calls_keep_local_extension_methods() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "src/lib.rs",
            "mod extension; mod nominal; mod caller; mod nominal_impl;",
        ),
        (
            "src/extension.rs",
            "use std::fs::OpenOptions;\npub trait Extra { fn local(); }\nimpl Extra for OpenOptions { fn local() {} }",
        ),
        (
            "src/nominal.rs",
            "pub struct OpenOptions;\nimpl OpenOptions { pub fn local() {} }",
        ),
        (
            "src/nominal_impl.rs",
            "use crate::nominal::OpenOptions;\ntrait Local { fn local(); }\nimpl Local for OpenOptions { fn local() {} }",
        ),
        (
            "src/caller.rs",
            "use std::fs::OpenOptions;\nuse crate::extension::Extra;\nfn extension_call() { OpenOptions::local(); }",
        ),
    ]);
    assert!(reaches(
        &db,
        "extension_call",
        "sym:src/extension.rs#OpenOptions::local"
    ));
    assert!(!reaches(
        &db,
        "extension_call",
        "sym:src/nominal.rs#OpenOptions::local"
    ));
    assert!(!reaches(
        &db,
        "extension_call",
        "sym:src/nominal_impl.rs#OpenOptions::local"
    ));
}

#[test]
fn relatively_imported_local_nominals_do_not_extend_external_types() {
    for (root, nominal_file, implementation, local) in [
        (
            "mod nominal; mod decoy; mod caller; mod local;",
            "src/nominal.rs",
            "use super::nominal::OpenOptions;",
            "use crate::nominal::OpenOptions;",
        ),
        (
            "mod decoy; mod caller; mod local;",
            "src/decoy/nominal.rs",
            "pub mod nominal; use self::nominal::OpenOptions;",
            "use crate::decoy::nominal::OpenOptions;",
        ),
    ] {
        let implementation = format!(
            "{implementation}\npub trait Decoy {{ fn new() -> u8; }}\nimpl Decoy for OpenOptions {{ fn new() -> u8 {{ 1 }} }}"
        );
        let local = format!(
            "{local}\nuse crate::decoy::Decoy;\nfn from_local() -> u8 {{ OpenOptions::new() }}"
        );
        let (_dir, db) = indexed(&[
            (
                "Cargo.toml",
                "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
            ),
            ("src/lib.rs", root),
            (nominal_file, "pub struct OpenOptions;"),
            ("src/decoy.rs", &implementation),
            ("src/local.rs", &local),
            (
                "src/caller.rs",
                "use std::fs::OpenOptions;\nuse crate::decoy::Decoy;\nfn from_std() -> OpenOptions { OpenOptions::new() }",
            ),
        ]);
        let target = "sym:src/decoy.rs#OpenOptions::new";
        assert!(!reaches(&db, "from_std", target));
        assert!(reaches(&db, "from_local", target));
        let references = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "OpenOptions::new".into(),
                kind: Some(codesage_protocol::ReferenceKind::Call),
            },
        )
        .unwrap();
        let bindings: std::collections::BTreeMap<_, _> = references
            .results
            .iter()
            .map(|row| (row.from_symbol.as_deref().unwrap(), row.to.as_deref()))
            .collect();
        assert_eq!(
            bindings,
            [("from_local", Some(target)), ("from_std", None)].into()
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
        assert_eq!(impact.len(), 1);
        assert_eq!(impact[0].file_path, "src/local.rs");
    }
}

#[test]
fn relative_reexports_can_reach_an_external_extension_receiver() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        ("src/lib.rs", "mod facade; mod extension; mod caller;"),
        ("src/facade.rs", "pub use ::std::fs::OpenOptions;"),
        (
            "src/extension.rs",
            "use super::facade::OpenOptions;\nmod facade;\npub trait Extra { fn outer() -> u8; }\nimpl Extra for OpenOptions { fn outer() -> u8 { 1 } }\nmod nested {\n    mod facade { pub use ::std::fs::OpenOptions; }\n    use self::facade::OpenOptions;\n    pub trait InlineExtra { fn inner() -> u8; }\n    impl InlineExtra for OpenOptions { fn inner() -> u8 { 2 } }\n}\npub use nested::InlineExtra;",
        ),
        ("src/extension/facade.rs", "pub struct OpenOptions;"),
        (
            "src/caller.rs",
            "use std::fs::OpenOptions;\nuse crate::extension::{Extra, InlineExtra};\nfn from_outer() -> u8 { OpenOptions::outer() }\nfn from_inline() -> u8 { OpenOptions::inner() }",
        ),
    ]);
    for (caller, method) in [("from_outer", "outer"), ("from_inline", "inner")] {
        let target = format!("sym:src/extension.rs#OpenOptions::{method}");
        assert!(reaches(&db, caller, &target));
    }
}

#[test]
fn a_blanket_impl_can_extend_an_external_type() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        ("src/lib.rs", "mod extension; mod caller;"),
        (
            "src/extension.rs",
            "pub struct OpenOptions;\npub trait Extra { fn project_extra() -> u8; }\nimpl<OpenOptions> Extra for OpenOptions { fn project_extra() -> u8 { 1 } }",
        ),
        (
            "src/caller.rs",
            "use std::fs::OpenOptions;\nuse crate::extension::Extra;\nfn extension_call() -> u8 { OpenOptions::project_extra() }",
        ),
    ]);
    let target = "sym:src/extension.rs#OpenOptions::project_extra";
    assert!(reaches(&db, "extension_call", target));
}

#[test]
fn a_crate_root_extern_alias_does_not_hide_its_own_methods() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "src/lib.rs",
            "extern crate self as std;\npub mod fs; mod caller;",
        ),
        (
            "src/fs.rs",
            "pub struct OpenOptions;\nimpl OpenOptions { pub fn new() {} }",
        ),
        (
            "src/caller.rs",
            "use std::fs::OpenOptions;\nfn aliased_self() { OpenOptions::new(); }",
        ),
    ]);
    assert!(reaches(
        &db,
        "aliased_self",
        "sym:src/fs.rs#OpenOptions::new"
    ));
}

#[test]
fn a_local_type_alias_can_extend_an_external_type() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        ("src/lib.rs", "mod extension; mod caller;"),
        (
            "src/extension.rs",
            "#[cfg(any())]\nstruct OpenOptions;\ntype OpenOptions = std::fs::OpenOptions;\npub trait Extra { fn local(); }\nimpl Extra for OpenOptions { fn local() {} }",
        ),
        (
            "src/caller.rs",
            "use std::fs::OpenOptions;\nuse crate::extension::Extra;\nfn extension_call() { OpenOptions::local(); }",
        ),
    ]);
    assert!(reaches(
        &db,
        "extension_call",
        "sym:src/extension.rs#OpenOptions::local"
    ));
}

#[test]
fn same_file_nominal_methods_do_not_override_external_imports() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "src/lib.rs",
            "use std::fs::OpenOptions as Options;\nfn foreign() { Options::new(); }\nmod internal { pub struct Options; impl Options { pub fn new() {} } fn local() { Options::new(); }}",
        ),
    ]);
    let references = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "Options::new".into(),
            kind: Some(codesage_protocol::ReferenceKind::Call),
        },
    )
    .unwrap();
    assert!(
        references
            .results
            .iter()
            .any(|row| row.from_symbol.as_deref() == Some("foreign") && row.to.is_none())
    );
    assert!(
        references
            .results
            .iter()
            .any(|row| row.from_symbol.as_deref() == Some("local")
                && row.to.as_deref() == Some("sym:src/lib.rs#Options::new"))
    );
}

#[test]
fn a_sibling_macro_can_change_a_type_binding_in_its_block() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "src/lib.rs",
            "use std::fs::OpenOptions;\nmod options;\nmacro_rules! import_local { ($ty:ident) => { use crate::options::$ty; }; }\nfn local() { import_local!(OpenOptions); OpenOptions::new(); }\nfn external() { OpenOptions::new(); }",
        ),
        (
            "src/options.rs",
            "pub struct OpenOptions;\nimpl OpenOptions { pub fn new() {} }",
        ),
    ]);
    let target = "sym:src/options.rs#OpenOptions::new";
    assert!(reaches(&db, "local", target));
    assert!(!reaches(&db, "external", target));
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
    assert_eq!(impact.len(), 1);
    assert_eq!(impact[0].reasons.len(), 1);
    assert_eq!(impact[0].reasons[0].line, 4);
}

#[test]
fn a_disabled_block_import_does_not_override_the_active_local_type() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "src/lib.rs",
            "mod options;\nuse crate::options::OpenOptions;\nfn local() { #[ cfg(any()) ] use std::fs::OpenOptions; OpenOptions::new(); }",
        ),
        (
            "src/options.rs",
            "pub struct OpenOptions;\nimpl OpenOptions { pub fn new() {} }",
        ),
    ]);
    assert!(reaches(&db, "local", "sym:src/options.rs#OpenOptions::new"));
}

#[test]
fn absolute_type_imports_follow_the_callers_edition() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = ['old', 'modern']\n[workspace.package]\nedition = '2024'\n",
        ),
        (
            "old/Cargo.toml",
            "[package]\nname = 'old'\nversion = '0.0.0'\nedition = '2015'\n",
        ),
        (
            "old/src/lib.rs",
            "#![ no_std ]\nmod custom { pub mod std { pub mod fs { pub struct OpenOptions; impl OpenOptions { pub fn old() {} } } } }\npub use custom::*;\nuse ::std::fs::OpenOptions;\npub fn local() { OpenOptions::old(); }",
        ),
        (
            "modern/Cargo.toml",
            "[package]\nname = 'modern'\nversion = '0.0.0'\nedition.workspace = true\n",
        ),
        (
            "modern/src/lib.rs",
            "pub mod std { pub mod fs { pub struct OpenOptions; impl OpenOptions { pub fn new() {} } } }\nuse ::std::fs::OpenOptions;\npub fn external() { OpenOptions::new(); }",
        ),
    ]);
    assert!(reaches(&db, "local", "sym:old/src/lib.rs#OpenOptions::old"));
    assert!(!reaches(
        &db,
        "external",
        "sym:modern/src/lib.rs#OpenOptions::new"
    ));
}

#[test]
fn bare_imports_in_rust_2015_can_reach_a_reexport_at_the_crate_root() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2015'\n",
        ),
        (
            "src/lib.rs",
            "mod caller;\nmod custom { pub mod core { pub mod cell { pub struct Cell; impl Cell { pub fn project_new() {} } } } }\npub use custom::*;",
        ),
        (
            "src/caller.rs",
            "use core::cell::Cell;\npub fn local() { Cell::project_new(); }",
        ),
    ]);
    assert!(reaches(&db, "local", "sym:src/lib.rs#Cell::project_new"));
}

#[test]
fn external_core_and_explicit_alloc_types_stay_external_in_a_no_std_crate() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "src/lib.rs",
            "#![no_std]\nextern crate alloc;\nmod nominal; mod calls;",
        ),
        (
            "src/nominal.rs",
            "pub struct Cell;\nimpl Cell { pub fn new() {} }\npub struct Vec;\nimpl Vec { pub fn new() {} }",
        ),
        (
            "src/calls.rs",
            "use core::cell::Cell;\nuse alloc::vec::Vec;\npub fn core_call() { let _ = Cell::new(1); }\npub fn alloc_call() { let _: Vec<u8> = Vec::new(); }",
        ),
    ]);
    assert!(!reaches(&db, "core_call", "sym:src/nominal.rs#Cell::new"));
    assert!(!reaches(&db, "alloc_call", "sym:src/nominal.rs#Vec::new"));
}

#[test]
fn spaced_imports_and_transparent_macro_calls_keep_per_site_type_proof() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        ("src/lib.rs", "mod nominal; mod calls;"),
        (
            "src/nominal.rs",
            "pub struct OpenOptions;\nimpl OpenOptions { pub fn new() -> u8 { 0 } }",
        ),
        (
            "src/calls.rs",
            "use std /* path comment */:: fs :: /* terminal comment */ OpenOptions;\nfn direct() { let _ = OpenOptions::new(); }\nfn transparent() { let _ = format!(\"{:?}\", OpenOptions::new()); }\nfn changed_scope() { let _ = format!(\"{:?}\", { use crate::nominal::OpenOptions; OpenOptions::new() }); }",
        ),
    ]);
    let target = "sym:src/nominal.rs#OpenOptions::new";
    assert!(!reaches(&db, "direct", target));
    assert!(!reaches(&db, "transparent", target));
    assert!(reaches(&db, "changed_scope", target));
    let references = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "OpenOptions::new".into(),
            kind: Some(codesage_protocol::ReferenceKind::Call),
        },
    )
    .unwrap();
    let bindings: std::collections::BTreeMap<_, _> = references
        .results
        .iter()
        .map(|row| (row.from_symbol.as_deref().unwrap(), row.to.as_deref()))
        .collect();
    assert_eq!(
        bindings,
        [
            ("changed_scope", Some(target)),
            ("direct", None),
            ("transparent", None),
        ]
        .into()
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
    assert_eq!(impact.len(), 1);
    assert_eq!(impact[0].reasons.len(), 1);
    assert_eq!(impact[0].reasons[0].line, 4);
}

#[test]
fn imported_values_can_share_a_name_with_the_local_type() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        ("src/lib.rs", "mod options; mod calls; mod foreign;"),
        (
            "src/options.rs",
            "pub struct Options {}\nimpl Options { pub fn new() -> std::fs::OpenOptions { std::fs::OpenOptions::new() } }",
        ),
        (
            "src/calls.rs",
            "use crate::options::Options;\nfn local() { use std::mem::drop as Options; let _ = Options::new(); Options(1); }\nfn constant_alias() { use std::f32::consts::PI as Options; let _: std::fs::OpenOptions = Options::new(); let _ = Options; }",
        ),
        (
            "src/foreign.rs",
            "use std::fs::OpenOptions as Options;\nfn external() { let _ = Options::new(); }",
        ),
    ]);
    let target = "sym:src/options.rs#Options::new";
    assert!(reaches(&db, "local", target));
    assert!(reaches(&db, "constant_alias", target));
    assert!(!reaches(&db, "external", target));
    let references = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "Options::new".into(),
            kind: Some(codesage_protocol::ReferenceKind::Call),
        },
    )
    .unwrap();
    let bindings: std::collections::BTreeMap<_, _> = references
        .results
        .iter()
        .map(|row| (row.from_symbol.as_deref().unwrap(), row.to.as_deref()))
        .collect();
    assert_eq!(
        bindings,
        [
            ("constant_alias", Some(target)),
            ("external", None),
            ("local", Some(target)),
        ]
        .into()
    );
}

#[test]
fn explicit_external_type_annotations_prove_block_imports_shadow_the_outer_type() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        ("src/lib.rs", "mod options; mod calls;"),
        (
            "src/options.rs",
            "pub struct OpenOptions;\nimpl OpenOptions { pub fn new() {} }\npub struct Options;\nimpl Options { pub fn new() {} }",
        ),
        (
            "src/calls.rs",
            "use crate::options::{OpenOptions, Options};\nuse std::fs as files;\nfn direct() {\n    OpenOptions::new();\n    { use std::fs::OpenOptions; let _: std::fs::OpenOptions = OpenOptions::new(); }\n}\nfn renamed() {\n    Options::new();\n    { use files::OpenOptions as Options; let _: ::std::fs::OpenOptions = Options::new(); }\n}\nfn unwitnessed() { use files::OpenOptions as Options; let _ = Options::new(); }\nfn direct_only() { use std::fs::OpenOptions; let _: std::fs::OpenOptions = OpenOptions::new(); }\nfn renamed_only() { use files::OpenOptions as Options; let _: ::std::fs::OpenOptions = Options::new(); }",
        ),
    ]);
    for (name, expected) in [
        ("OpenOptions::new", vec![(4, true), (5, false), (12, false)]),
        (
            "Options::new",
            vec![(8, true), (9, false), (11, true), (13, false)],
        ),
    ] {
        let target = format!("sym:src/options.rs#{name}");
        let references = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: name.into(),
                kind: Some(codesage_protocol::ReferenceKind::Call),
            },
        )
        .unwrap();
        let mut bindings: Vec<_> = references
            .results
            .iter()
            .map(|row| (row.line, row.to.as_deref() == Some(target.as_str())))
            .collect();
        bindings.sort_unstable();
        assert_eq!(bindings, expected);
        let impact = impact_analysis(
            &db,
            &ImpactRequest {
                target: ImpactTarget::Symbol { name: target },
                depth: 1,
                source_only: false,
            },
        )
        .unwrap();
        assert_eq!(impact.len(), 1);
        let mut lines: Vec<_> = impact[0].reasons.iter().map(|reason| reason.line).collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            expected
                .into_iter()
                .filter_map(|(line, bound)| bound.then_some(line))
                .collect::<Vec<_>>()
        );
    }
    assert!(!reaches(
        &db,
        "direct_only",
        "sym:src/options.rs#OpenOptions::new"
    ));
    assert!(!reaches(
        &db,
        "renamed_only",
        "sym:src/options.rs#Options::new"
    ));
    assert!(reaches(
        &db,
        "unwitnessed",
        "sym:src/options.rs#Options::new"
    ));
}

#[test]
fn an_unavailable_trait_does_not_add_a_method_to_an_imported_type() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        ("src/lib.rs", "mod decoy; mod caller;"),
        (
            "src/decoy.rs",
            "use std::process::Command as OpenOptions;\npub trait Decoy { fn new(); }\nimpl Decoy for OpenOptions { fn new() {} }",
        ),
        (
            "src/caller.rs",
            "use std::fs::OpenOptions;\nfn call() { let _ = OpenOptions::new(); }",
        ),
    ]);
    let target = "sym:src/decoy.rs#OpenOptions::new";
    assert!(!reaches(&db, "call", target));
    let references = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "OpenOptions::new".into(),
            kind: Some(codesage_protocol::ReferenceKind::Call),
        },
    )
    .unwrap();
    assert_eq!(references.results.len(), 1);
    assert!(references.results[0].to.is_none());
}

#[test]
fn an_imported_extension_trait_can_apply_through_standard_reexports() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "src/lib.rs",
            "extern crate alloc;\nmod extension; mod caller; mod anonymous;",
        ),
        (
            "src/extension.rs",
            "use alloc::ffi::CString;\npub trait Extra { fn project_new() -> u8; }\nimpl Extra for CString { fn project_new() -> u8 { 1 } }",
        ),
        (
            "src/caller.rs",
            "use std::ffi::CString;\nuse crate::extension::Extra;\nfn named() -> u8 { CString::project_new() }",
        ),
        (
            "src/anonymous.rs",
            "use std::ffi::CString;\nuse crate::extension::Extra as _;\nfn anonymous() -> u8 { CString::project_new() }",
        ),
    ]);
    let target = "sym:src/extension.rs#CString::project_new";
    assert!(reaches(&db, "named", target));
    assert!(reaches(&db, "anonymous", target));
}

#[test]
fn explicit_external_crate_aliases_do_not_bind_to_local_methods() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "src/lib.rs",
            "extern crate std as global_std;\nmod aliases; mod global; mod local; mod options;",
        ),
        (
            "src/options.rs",
            "pub struct OpenOptions;\nimpl OpenOptions { pub fn new() {} }",
        ),
        (
            "src/aliases.rs",
            "use std as library;\nuse library as system;\nuse system::fs::OpenOptions;\nfn through_use() { let _ = OpenOptions::new(); }",
        ),
        (
            "src/global.rs",
            "use global_std::fs::OpenOptions;\nfn through_extern() { let _ = OpenOptions::new(); }",
        ),
        (
            "src/local.rs",
            "use crate::options as module;\nuse module::OpenOptions;\nfn through_local() { OpenOptions::new(); }",
        ),
    ]);
    let target = "sym:src/options.rs#OpenOptions::new";
    assert!(!reaches(&db, "through_use", target));
    assert!(!reaches(&db, "through_extern", target));
    assert!(reaches(&db, "through_local", target));
}

#[test]
fn value_aliases_do_not_replace_an_outer_module_in_import_paths() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "src/lib.rs",
            "mod local;\npub fn real() { use std::f64::consts::PI as local; use local::OpenOptions; OpenOptions::new(); let _ = local; }\npub fn crate_alias() { use std as local; use local::fs::OpenOptions; let _ = OpenOptions::new(); }\npub fn module_alias() { use std::fs as external_fs; use external_fs::OpenOptions; let _ = OpenOptions::new(); }",
        ),
        (
            "src/local.rs",
            "pub struct OpenOptions;\nimpl OpenOptions { pub fn new() {} }",
        ),
    ]);
    let target = "sym:src/local.rs#OpenOptions::new";
    assert!(reaches(&db, "real", target));
    assert!(!reaches(&db, "crate_alias", target));
    assert!(!reaches(&db, "module_alias", target));
    let references = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "OpenOptions::new".into(),
            kind: Some(codesage_protocol::ReferenceKind::Call),
        },
    )
    .unwrap();
    let bindings: std::collections::BTreeMap<_, _> = references
        .results
        .iter()
        .map(|row| (row.from_symbol.as_deref().unwrap(), row.to.as_deref()))
        .collect();
    assert_eq!(
        bindings,
        [
            ("crate_alias", None),
            ("module_alias", None),
            ("real", Some(target)),
        ]
        .into()
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
    assert_eq!(impact.len(), 1);
    assert_eq!(impact[0].file_path, "src/lib.rs");
    assert_eq!(impact[0].reasons.len(), 1);
    assert_eq!(impact[0].reasons[0].line, 2);
}

#[test]
fn value_imports_do_not_replace_globbed_types_or_module_prefixes() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "src/lib.rs",
            "mod exports; mod prefix; mod receiver; mod blocks;",
        ),
        (
            "src/exports.rs",
            "pub mod local { pub struct OpenOptions {} impl OpenOptions { pub fn new() {} } }\npub use local::OpenOptions;",
        ),
        (
            "src/prefix.rs",
            "use crate::exports::*;\nuse ::std::f64::consts::PI as local;\nuse local::OpenOptions;\nfn prefix() { OpenOptions::new(); let _ = local; }",
        ),
        (
            "src/receiver.rs",
            "use crate::exports::*;\nuse ::std::f64::consts::PI as OpenOptions;\nfn receiver() { OpenOptions::new(); let _ = OpenOptions; }",
        ),
        (
            "src/blocks.rs",
            "fn block_prefix() { use crate::exports::*; use ::std::f64::consts::PI as local; use local::OpenOptions; OpenOptions::new(); let _ = local; }\nfn block_receiver() { use crate::exports::*; use ::std::f64::consts::PI as OpenOptions; OpenOptions::new(); let _ = OpenOptions; }\nfn crate_alias() { use crate::exports::*; use ::std as local; use local::fs::OpenOptions; let _: ::std::fs::OpenOptions = OpenOptions::new(); }\nfn explicit_type() { use crate::exports::*; use ::std::fs::OpenOptions; let _: ::std::fs::OpenOptions = OpenOptions::new(); }",
        ),
    ]);
    let target = "sym:src/exports.rs#OpenOptions::new";
    for caller in ["prefix", "receiver", "block_prefix", "block_receiver"] {
        assert!(reaches(&db, caller, target), "{caller}");
    }
    for caller in ["crate_alias", "explicit_type"] {
        assert!(!reaches(&db, caller, target), "{caller}");
    }
    let references = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "OpenOptions::new".into(),
            kind: Some(codesage_protocol::ReferenceKind::Call),
        },
    )
    .unwrap();
    let bindings: std::collections::BTreeMap<_, _> = references
        .results
        .iter()
        .map(|row| (row.from_symbol.as_deref().unwrap(), row.to.as_deref()))
        .collect();
    assert_eq!(
        bindings,
        [
            ("block_prefix", Some(target)),
            ("block_receiver", Some(target)),
            ("crate_alias", None),
            ("explicit_type", None),
            ("prefix", Some(target)),
            ("receiver", Some(target)),
        ]
        .into()
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
    assert_eq!(
        impact
            .iter()
            .flat_map(|entry| entry
                .reasons
                .iter()
                .map(|reason| (entry.file_path.as_str(), reason.line)))
            .collect::<std::collections::BTreeSet<_>>(),
        [
            ("src/blocks.rs", 1),
            ("src/blocks.rs", 2),
            ("src/prefix.rs", 4),
            ("src/receiver.rs", 3),
        ]
        .into()
    );
}

#[test]
fn a_value_function_does_not_shadow_an_imported_type() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        ("src/lib.rs", "mod options; mod caller;"),
        (
            "src/options.rs",
            "pub struct OpenOptions;\nimpl OpenOptions { pub fn new() {} }",
        ),
        (
            "src/caller.rs",
            "use std::fs::OpenOptions;\nfn OpenOptions() {}\nfn call() { let _ = OpenOptions::new(); OpenOptions(); }",
        ),
    ]);
    assert!(!reaches(&db, "call", "sym:src/options.rs#OpenOptions::new"));
    assert!(reaches(&db, "call", "sym:src/caller.rs#OpenOptions"));
    for (spelling, target) in [
        ("OpenOptions::new", None),
        ("OpenOptions", Some("sym:src/caller.rs#OpenOptions")),
    ] {
        let references = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: spelling.into(),
                kind: Some(codesage_protocol::ReferenceKind::Call),
            },
        )
        .unwrap();
        assert_eq!(references.results.len(), 1);
        assert_eq!(references.results[0].to.as_deref(), target);
    }
}

#[test]
fn qualified_std_expression_macros_require_a_standard_crate_binding() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        ("src/lib.rs", "mod options; mod calls; mod custom;"),
        (
            "src/options.rs",
            "pub struct OpenOptions;\nimpl OpenOptions { pub fn new() {} }",
        ),
        (
            "src/custom.rs",
            "macro_rules! format { ($msg:literal, $ty:ident::$method:ident()) => {{ use crate::options::$ty; $ty::$method() }}; }\npub(crate) use format;",
        ),
        (
            "src/calls.rs",
            "use ::std::fs::OpenOptions;\nfn standard() -> String { std::format!(\"{:?}\", OpenOptions::new()) }\nfn custom() { use crate::custom as std; std::format!(\"{:?}\", OpenOptions::new()); }",
        ),
    ]);
    let target = "sym:src/options.rs#OpenOptions::new";
    assert!(!reaches(&db, "standard", target));
    assert!(reaches(&db, "custom", target));
}

#[test]
fn registry_dev_dependencies_are_external_from_integration_tests() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n[dev-dependencies]\nregistry = '1'\n",
        ),
        (
            "src/lib.rs",
            "pub struct Options;\nimpl Options { pub fn new() {} }",
        ),
        (
            "tests/check.rs",
            "use registry::Options;\n#[test]\nfn integration() { Options::new(); }",
        ),
    ]);
    let target = "sym:src/lib.rs#Options::new";
    assert!(!reaches(&db, "integration", target));
    let references = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "Options::new".into(),
            kind: Some(codesage_protocol::ReferenceKind::Call),
        },
    )
    .unwrap();
    assert_eq!(references.results.len(), 1);
    assert!(references.results[0].to.is_none());
}

#[test]
fn workspace_public_reexport_resolves_in_references_impact_trace_and_risk() {
    let (_dir, db) = workspace(
        "use codesage_graph::{search_page};\nfn main() {\ncodesage_graph::search_page();\nsearch_page();\n}\n",
    );
    let target = "sym:crates/graph/src/search.rs#search_page";
    assert!(reaches(&db, "main", target));
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "search_page".into(),
            kind: None,
        },
    )
    .unwrap();
    let callers: Vec<_> = refs
        .results
        .iter()
        .filter(|row| row.from_file == "crates/cli/src/main.rs")
        .collect();
    assert_eq!(callers.len(), 3);
    assert!(
        callers.iter().all(|row| row.to.as_deref() == Some(target)),
        "{callers:?}"
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
    assert!(
        impact
            .iter()
            .any(|row| row.file_path == "crates/cli/src/main.rs"),
        "{impact:?}"
    );
    assert!(!reaches(
        &db,
        "main",
        "sym:crates/decoy/src/lib.rs#search_page"
    ));
    let risk = assess_risk(&db, "crates/graph/src/search.rs").unwrap();
    assert!(
        risk.top_symbols
            .iter()
            .find(|symbol| symbol.name == "search_page")
            .unwrap()
            .why
            .contains("4 refs"),
        "{:?}",
        risk.top_symbols
    );
}

#[test]
fn dependency_aliases_custom_library_names_and_renamed_exports_resolve() {
    let (_dir, db) = indexed(&[
        ("Cargo.toml", "[workspace]\nmembers = ['api', 'app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname = 'implementation'\nversion = '0.0.0'\nedition = '2024'\n[lib]\nname = 'custom_api'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n[dependencies]\nrenamed = { path = '../api', package = 'implementation' }\n",
        ),
        (
            "api/src/lib.rs",
            "mod internal;\npub use internal::{original as exported};\n",
        ),
        ("api/src/internal.rs", "pub fn original() {}\n"),
        (
            "app/src/main.rs",
            "use renamed::{exported as local};\nfn direct() { renamed::exported(); }\nfn imported() { local(); }\nfn wrong() { implementation::exported(); custom_api::exported(); }\n",
        ),
    ]);
    assert!(reaches(&db, "direct", "original"));
    assert!(reaches(&db, "imported", "original"));
    assert!(!reaches(&db, "wrong", "original"));

    let (_dir, db) = indexed(&[
        (
            "api/Cargo.toml",
            "[package]\nname = 'implementation'\nversion = '0.0.0'\nedition = '2024'\n[lib]\nname = 'custom_api'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname = 'app'\nversion = '0.0.0'\nedition = '2024'\n[dependencies]\nimplementation = { path = '../api' }\n",
        ),
        ("api/src/lib.rs", "pub fn exported() {}\n"),
        (
            "app/src/main.rs",
            "fn main() { custom_api::exported(); }\nfn wrong() { implementation::exported(); }\n",
        ),
    ]);
    assert!(reaches(&db, "main", "exported"));
    assert!(!reaches(&db, "wrong", "exported"));
}

#[test]
fn actual_codesage_search_page_call_resolves_from_cli_source() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let paths = [
        "Cargo.toml",
        "crates/graph/Cargo.toml",
        "crates/cli/Cargo.toml",
        "crates/graph/src/lib.rs",
        "crates/graph/src/search.rs",
        "crates/cli/src/main.rs",
    ];
    let sources: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read_to_string(root.join(path)).unwrap())
        .collect();
    let files: Vec<_> = paths
        .iter()
        .zip(&sources)
        .map(|(path, source)| (*path, source.as_str()))
        .collect();
    let (_dir, db) = indexed(&files);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "search_page".into(),
            kind: None,
        },
    )
    .unwrap();
    let calls: Vec<_> = refs
        .results
        .iter()
        .filter(|row| {
            row.from_file == "crates/cli/src/main.rs"
                && row.to_name == "codesage_graph::search_page"
        })
        .collect();
    assert!(!calls.is_empty());
    assert!(
        calls
            .iter()
            .all(|row| row.to.as_deref() == Some("sym:crates/graph/src/search.rs#search_page")),
        "{calls:?} bounds: {:?}",
        refs.to_resolution
    );
}

#[test]
fn private_imports_modules_and_definitions_are_not_public_exports() {
    for library in [
        "mod search; use search::search_page;",
        "mod search; pub(crate) use search::search_page;",
        "mod search;",
        "mod search; #[cfg(feature = \"unknown\")] pub use search::search_page;",
    ] {
        let (dir, db) = workspace(
            "fn main() { codesage_graph::search_page(); codesage_graph::search::search_page(); }\n",
        );
        std::fs::write(dir.path().join("crates/graph/src/lib.rs"), library).unwrap();
        full_index(dir.path(), &db, &[], false).unwrap();
        assert!(
            !reaches(&db, "main", "sym:crates/graph/src/search.rs#search_page"),
            "{library}"
        );
    }
    let (dir, db) = workspace("fn main() { codesage_graph::search_page(); }\n");
    std::fs::write(
        dir.path().join("crates/graph/src/search.rs"),
        "pub(crate) fn search_page() {}\n",
    )
    .unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert!(!reaches(
        &db,
        "main",
        "sym:crates/graph/src/search.rs#search_page"
    ));
}

#[test]
fn source_divergence_missing_dependencies_and_escape_paths_do_not_create_edges() {
    for path in [
        "crates/graph/src/lib.rs",
        "crates/graph/src/search.rs",
        "crates/cli/src/main.rs",
    ] {
        let (dir, db) = workspace("fn main() { codesage_graph::search_page(); }\n");
        let path = dir.path().join(path);
        let source = std::fs::read_to_string(&path).unwrap();
        std::fs::write(path, format!("{source}\n// changed\n")).unwrap();
        assert!(!reaches(
            &db,
            "main",
            "sym:crates/graph/src/search.rs#search_page"
        ));
    }
    for dependency in [
        "",
        "[dependencies]\ncodesage-graph = { path = '../../../outside' }\n",
        "[dependencies]\ncodesage-graph = { path = '../decoy' }\n",
    ] {
        let (dir, db) = workspace("fn main() { codesage_graph::search_page(); }\n");
        std::fs::write(
            dir.path().join("crates/cli/Cargo.toml"),
            format!(
                "[package]\nname = 'codesage'\nversion = '0.0.0'\nedition = '2024'\n{dependency}"
            ),
        )
        .unwrap();
        assert!(!reaches(
            &db,
            "main",
            "sym:crates/graph/src/search.rs#search_page"
        ));
    }
}

#[test]
fn public_module_paths_and_chained_reexports_resolve_without_private_shortcuts() {
    let (dir, db) = workspace(
        "fn main() { codesage_graph::search_page(); }\nfn private() { codesage_graph::search::search_page(); }\n",
    );
    std::fs::write(
        dir.path().join("crates/graph/src/lib.rs"),
        "pub mod bridge;\nmod search;\npub use bridge::search_page;\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("crates/graph/src/bridge.rs"),
        "pub use crate::search::search_page;\n",
    )
    .unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert!(reaches(
        &db,
        "main",
        "sym:crates/graph/src/search.rs#search_page"
    ));
    assert!(!reaches(
        &db,
        "private",
        "sym:crates/graph/src/search.rs#search_page"
    ));
    std::fs::write(
        dir.path().join("crates/graph/src/lib.rs"),
        "pub mod search;\n",
    )
    .unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert!(reaches(
        &db,
        "private",
        "sym:crates/graph/src/search.rs#search_page"
    ));
}

#[test]
fn shadowed_imports_duplicate_definitions_and_export_cycles_are_unresolved() {
    let (_dir, db) = workspace(
        "use codesage_graph::search_page;\nfn main() { let search_page = || {}; search_page(); }\n",
    );
    assert!(!reaches(
        &db,
        "main",
        "sym:crates/graph/src/search.rs#search_page"
    ));
    let (dir, db) = workspace("fn main() { codesage_graph::search_page(); }\n");
    std::fs::write(
        dir.path().join("crates/graph/src/search.rs"),
        "pub fn search_page() {}\npub fn search_page() {}\n",
    )
    .unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert!(!reaches(
        &db,
        "main",
        "sym:crates/graph/src/search.rs#search_page@1"
    ));
    std::fs::write(
        dir.path().join("crates/graph/src/lib.rs"),
        "pub use self::search_page;\nmod search;\n",
    )
    .unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert!(!reaches(
        &db,
        "main",
        "sym:crates/graph/src/search.rs#search_page@1"
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_manifest_or_export_source_is_not_trusted() {
    for relative in ["crates/graph/Cargo.toml", "crates/graph/src/lib.rs"] {
        let (dir, db) = workspace("fn main() { codesage_graph::search_page(); }\n");
        let path = dir.path().join(relative);
        let saved = path.with_extension("saved");
        std::fs::rename(&path, &saved).unwrap();
        std::os::unix::fs::symlink(&saved, &path).unwrap();
        assert!(!reaches(
            &db,
            "main",
            "sym:crates/graph/src/search.rs#search_page"
        ));
    }
}

#[test]
fn a_packages_binary_can_name_its_library_but_library_source_cannot_name_itself() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname = 'own-api'\nversion = '0.0.0'\nedition = '2024'\n",
        ),
        (
            "src/lib.rs",
            "pub fn exported() {}\npub fn wrong() { own_api::exported(); }\n",
        ),
        ("src/main.rs", "fn main() { own_api::exported(); }\n"),
    ]);
    assert!(reaches(&db, "main", "exported"));
    assert!(!reaches(&db, "wrong", "exported"));
}

#[test]
fn external_export_walk_reports_its_bound() {
    let (dir, db) = workspace("fn main() { codesage_graph::search_page(); }\n");
    std::fs::write(
        dir.path().join("crates/graph/src/lib.rs"),
        "mod bridge0;\npub use bridge0::search_page;\nmod search;\n",
    )
    .unwrap();
    for index in 0..40 {
        let directory = dir.path().join(format!(
            "crates/graph/src/{}",
            (0..index)
                .map(|i| format!("bridge{i}/"))
                .collect::<String>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("bridge{index}.rs")),
            format!(
                "pub mod bridge{}; pub use bridge{}::search_page;",
                index + 1,
                index + 1
            ),
        )
        .unwrap();
    }
    full_index(dir.path(), &db, &[], false).unwrap();
    let report = trace_call_path(
        &db,
        &CallPathRequest {
            from: "main".into(),
            to: "sym:crates/graph/src/search.rs#search_page".into(),
            max_depth: 3,
        },
    )
    .unwrap();
    assert!(!report.found);
    assert!(report.bounded);
}

#[test]
fn external_resolution_preserves_local_edges_with_unrelated_shadowing() {
    let (_dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname='ordinary'\nversion='0.0.0'\nedition='2024'\n",
        ),
        (
            "src/main.rs",
            "fn helper() {}\nfn unrelated(helper: u32) { std::hint::black_box(helper); }\nfn main() { helper(); }\n",
        ),
    ]);
    assert!(reaches(&db, "main", "helper"));
}

#[test]
fn external_resolution_preserves_local_edges_with_stale_source() {
    let (dir, db) = indexed(&[
        (
            "Cargo.toml",
            "[package]\nname='ordinary'\nversion='0.0.0'\nedition='2024'\n",
        ),
        ("src/main.rs", "fn helper() {}\nfn main() { helper(); }\n"),
    ]);
    assert!(reaches(&db, "main", "helper"));
    let path = dir.path().join("src/main.rs");
    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(path, format!("{source}\n// unrelated comment\n")).unwrap();
    assert!(reaches(&db, "main", "helper"));
}

#[test]
fn an_unsupported_dependency_edition_does_not_resolve_to_the_wrong_module() {
    let (_dir, db) = indexed(&[
        ("Cargo.toml", "[workspace]\nmembers=['api','app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'\nedition='2015'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\n",
        ),
        ("api/src/lib.rs", "pub mod inner;\npub mod target;\n"),
        ("api/src/target.rs", "pub fn foo() {}\n"),
        ("api/src/inner.rs", "mod target;\npub use target::foo;\n"),
        ("api/src/inner/target.rs", "pub fn foo() {}\n"),
        ("app/src/main.rs", "fn main() { api::inner::foo(); }\n"),
    ]);
    assert!(!reaches(&db, "main", "sym:api/src/inner/target.rs#foo"));
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "foo".into(),
            kind: None,
        },
    )
    .unwrap();
    let calls: Vec<_> = refs
        .results
        .iter()
        .filter(|row| row.from_file == "app/src/main.rs")
        .collect();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].to.is_none());
}

#[test]
fn pattern_bindings_do_not_become_external_function_calls() {
    for body in [
        "for local in [|| {}] { local(); }",
        "if let Some(local) = Some(|| {}) { local(); }",
        "while let Some(local) = Some(|| {}) { local(); break; }",
        "match Some(|| {}) { Some(local) => local(), None => () }",
        "let call = |(local,): (fn(),)| local(); call((|| {},));",
        "struct Container { local: fn() } let Container { local } = Container { local: || {} }; local();",
    ] {
        let caller = format!("use api::foo as local;\nfn main() {{ {body} }}\n");
        let (_dir, db) = indexed(&[
            ("Cargo.toml", "[workspace]\nmembers=['api','app']\n"),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
            ),
            (
                "app/Cargo.toml",
                "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\n",
            ),
            ("api/src/lib.rs", "pub fn foo() {}\n"),
            ("app/src/main.rs", &caller),
        ]);
        assert!(!reaches(&db, "main", "sym:api/src/lib.rs#foo"), "{body}");
    }
}

#[test]
fn namespace_bindings_and_unsupported_globs_do_not_name_a_homonymous_dependency() {
    let cases = [
        (
            "extern",
            "extern crate other as api; fn main() { api::foo(); }",
            "main",
        ),
        (
            "nested extern",
            "fn main() { extern crate other as api; api::foo(); }",
            "main",
        ),
        (
            "extern import source",
            "extern crate other as api; use api::foo as local; fn main() { local(); }",
            "main",
        ),
        (
            "alias import source",
            "use other as api; use api::foo as local; fn main() { local(); }",
            "main",
        ),
        ("glob", "use other::*; fn main() { api::foo(); }", "main"),
        (
            "nested glob",
            "fn main() { use other::*; api::foo(); }",
            "main",
        ),
        (
            "glob import source",
            "use other::*; use api::foo as local; fn main() { local(); }",
            "main",
        ),
        (
            "generic type",
            "trait Marker { fn foo(); } fn invoke<api: Marker>() { api::foo(); }",
            "invoke",
        ),
    ];
    let mut false_edges = Vec::new();
    for (label, caller, from) in cases {
        let (_dir, db) = indexed(&[
            ("Cargo.toml", "[workspace]\nmembers=['api','other','app']\n"),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
            ),
            (
                "other/Cargo.toml",
                "[package]\nname='other'\nversion='0.0.0'\nedition='2024'\n",
            ),
            (
                "app/Cargo.toml",
                "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\nother={path='../other'}\n",
            ),
            ("api/src/lib.rs", "pub fn foo() {}\n"),
            ("other/src/lib.rs", "pub fn foo() {} pub mod api;\n"),
            ("other/src/api.rs", "pub fn foo() {}\n"),
            ("app/src/main.rs", caller),
        ]);
        if reaches(&db, from, "sym:api/src/lib.rs#foo") {
            false_edges.push(label);
        }
    }
    assert!(
        false_edges.is_empty(),
        "wrong dependency edges: {false_edges:?}"
    );
}

#[test]
fn a_same_file_parent_glob_without_the_dependency_name_keeps_qualified_calls() {
    let (_dir, db) = indexed(&[
        ("Cargo.toml", "[workspace]\nmembers=['api','app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\n",
        ),
        ("api/src/lib.rs", "pub fn foo() {}\n"),
        (
            "app/src/main.rs",
            "fn main() { api::foo(); } mod caller { use super::*; pub fn invoke() { api::foo(); } }\n",
        ),
    ]);
    assert!(reaches(&db, "main", "sym:api/src/lib.rs#foo"));
    assert!(reaches(&db, "invoke", "sym:api/src/lib.rs#foo"));
}

#[test]
fn macro_invocations_that_can_change_the_callers_namespace_are_unresolved() {
    let cases = [
        "macro_rules! shadow { () => { use other as api; } } shadow!(); fn main() { api::foo(); }",
        "macro_rules! shadow { () => { use other as api; } } fn main() { shadow!(); api::foo(); }",
        "#[rewrite] fn main() { api::foo(); }",
        "macro_rules! assert { () => { use other as api; } } fn main() { assert!(); api::foo(); }",
        "macro_rules! shadow { () => { use other as api; } } fn main() { shadow!(); assert!(api::foo() == ()); }",
        "fn main() { assert!({ use other as api; api::foo() } == ()); }",
        "use api::foo; fn main() { let v = [0]; assert!(v.iter().all(|foo| foo())); }",
        "use api::foo; fn main() { assert!(match 1 { foo => foo() }); }",
        "use api::foo; fn main() { assert!({ let foo = || (); foo() } == ()); }",
        "use api::foo; fn main() { let v = [0]; v.iter().all(|foo| foo()); }",
    ];
    let mut false_edges = Vec::new();
    for caller in cases {
        let (_dir, db) = indexed(&[
            ("Cargo.toml", "[workspace]\nmembers=['api','other','app']\n"),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
            ),
            (
                "other/Cargo.toml",
                "[package]\nname='other'\nversion='0.0.0'\nedition='2024'\n",
            ),
            (
                "app/Cargo.toml",
                "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\nother={path='../other'}\n",
            ),
            ("api/src/lib.rs", "pub fn foo() {}\n"),
            ("other/src/lib.rs", "pub fn foo() {}\n"),
            ("app/src/main.rs", caller),
        ]);
        if reaches(&db, "main", "sym:api/src/lib.rs#foo") {
            false_edges.push(caller);
        }
    }
    assert!(
        false_edges.is_empty(),
        "macro namespace edges: {false_edges:?}"
    );
}

#[test]
fn calls_inside_std_expression_macros_resolve_through_dependencies() {
    let (_dir, db) = indexed(&[
        ("Cargo.toml", "[workspace]\nmembers=['api','app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\n",
        ),
        (
            "api/src/lib.rs",
            "pub fn foo() -> i32 { 1 }\npub fn bar() -> i32 { 2 }\n",
        ),
        (
            "app/src/main.rs",
            "fn main() {\n    assert_eq!(api::foo(), 1);\n}\nfn other() {\n    let _v = std::vec![api::bar()];\n}\n",
        ),
    ]);
    assert!(reaches(&db, "main", "sym:api/src/lib.rs#foo"));
    assert!(reaches(&db, "other", "sym:api/src/lib.rs#bar"));
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "foo".into(),
            kind: None,
        },
    )
    .unwrap();
    let rows: Vec<_> = refs
        .results
        .iter()
        .map(|row| (row.from_file.as_str(), row.line, row.to.as_deref()))
        .collect();
    assert_eq!(
        rows,
        [("app/src/main.rs", 2, Some("sym:api/src/lib.rs#foo"))]
    );
}

#[test]
fn imported_and_nested_macros_that_rebind_std_names_keep_calls_unresolved() {
    let (_dir, db) = indexed(&[
        ("Cargo.toml", "[workspace]\nmembers=['api','other','app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'\nedition='2021'\n",
        ),
        (
            "other/Cargo.toml",
            "[package]\nname='other'\nversion='0.0.0'\nedition='2021'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='app'\nversion='0.0.0'\nedition='2021'\n[dependencies]\napi={path='../api'}\nother={path='../other'}\n",
        ),
        ("api/src/lib.rs", "pub fn foo() -> i32 { 1 }\n"),
        (
            "other/src/lib.rs",
            "pub fn foo() -> i32 { 2 }\n#[macro_export]\nmacro_rules! assert { () => { use other as api; }; ($e:expr) => { () }; }\n",
        ),
        (
            "app/src/bin/a.rs",
            "macro_rules! shadow { () => { use other as api; } }\nfn main() {\n    println!(\"{}\", { shadow!(); api::foo() });\n}\n",
        ),
        (
            "app/src/bin/h.rs",
            "macro_rules! shadow { () => { use other as api; } }\nfn main() {\n    let s = format!(\"{}\", { shadow!(); api::foo() });\n    println!(\"{s}\");\n}\n",
        ),
        (
            "app/src/bin/b.rs",
            "use other::assert;\nfn main() {\n    assert!();\n    std::println!(\"{}\", api::foo());\n}\n",
        ),
        (
            "app/src/bin/c.rs",
            "#[macro_use]\nextern crate other;\nfn main() {\n    assert!();\n    println!(\"{}\", api::foo());\n}\n",
        ),
        (
            "app/src/bin/g/m.rs",
            "macro_rules! assert { () => { use other as api; } }\n",
        ),
        (
            "app/src/bin/g/main.rs",
            "#[macro_use]\nmod m;\nfn main() {\n    assert!();\n    println!(\"{}\", api::foo());\n}\n",
        ),
        (
            "app/src/bin/e.rs",
            "use other::assert;\nfn main() {\n    println!(\"{:?}\", assert!(api::foo()));\n}\n",
        ),
        (
            "app/src/bin/e2.rs",
            "macro_rules! assert { ($e:expr) => { () } }\nfn main() {\n    println!(\"{:?}\", assert!(api::foo()));\n}\n",
        ),
        (
            "app/src/bin/ctl.rs",
            "fn main() {\n    assert_eq!(api::foo(), 1);\n}\n",
        ),
        (
            "app/src/bin/d.rs",
            "fn main() {\n    assert_eq!(api::foo(), 1 // why\n    );\n}\n",
        ),
        (
            "app/src/bin/f.rs",
            "fn main() {\n    println!(\"{x}\", x = api::foo());\n}\n",
        ),
    ]);
    let refs = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "foo".into(),
            kind: None,
        },
    )
    .unwrap();
    let api = Some("sym:api/src/lib.rs#foo");
    let mut bound: Vec<_> = refs
        .results
        .iter()
        .filter(|row| row.to.as_deref() == api)
        .map(|row| (row.from_file.as_str(), row.line))
        .collect();
    bound.sort_unstable();
    assert_eq!(
        bound,
        [
            ("app/src/bin/ctl.rs", 2),
            ("app/src/bin/d.rs", 2),
            ("app/src/bin/f.rs", 2),
        ]
    );
}

fn foo_bindings(db: &Database, symbol: &str) -> Vec<(String, u32, Option<String>)> {
    let mut rows: Vec<_> = find_references(
        db,
        &FindReferencesRequest {
            symbol_name: symbol.into(),
            kind: None,
        },
    )
    .unwrap()
    .results
    .into_iter()
    .filter(|row| row.kind == codesage_protocol::ReferenceKind::Call)
    .map(|row| (row.from_file, row.line, row.to))
    .collect();
    rows.sort();
    rows
}

fn api_other_app(files: &[(&'static str, &'static str)]) -> (tempfile::TempDir, Database) {
    let mut all = vec![
        ("Cargo.toml", "[workspace]\nmembers=['api','other','app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'\nedition='2021'\n",
        ),
        (
            "other/Cargo.toml",
            "[package]\nname='other'\nversion='0.0.0'\nedition='2021'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='app'\nversion='0.0.0'\nedition='2021'\n[dependencies]\napi={path='../api'}\nother={path='../other'}\n",
        ),
        ("api/src/lib.rs", "pub fn foo() -> i32 { 1 }\n"),
    ];
    all.extend_from_slice(files);
    indexed(&all)
}

#[test]
fn a_std_named_macro_from_another_file_leaves_its_arguments_unbound() {
    let (_dir, db) = api_other_app(&[
        ("other/src/lib.rs", "pub fn foo() -> i32 { 2 }\n"),
        (
            "app/src/main.rs",
            "#[macro_use]\nmod macros;\nmod x;\nmod y;\nfn main() {\n    x::g();\n    println!(\"{}\", x::f());\n}\n",
        ),
        (
            "app/src/macros.rs",
            "macro_rules! assert { () => { use other as api; }; ($e:expr) => { () }; }\n",
        ),
        (
            "app/src/x.rs",
            "pub fn f() -> i32 {\n    assert!();\n    api::foo()\n}\npub fn helper() -> i32 { 1 }\npub fn g() {\n    assert!(helper());\n}\npub fn nested() -> i32 { 3 }\npub fn h() {\n    println!(\"{:?}\", assert!(nested()));\n}\n",
        ),
        (
            "app/src/y.rs",
            "pub fn k() -> i32 {\n    crate::x::helper()\n}\n",
        ),
    ]);
    assert_eq!(foo_bindings(&db, "foo"), [("app/src/x.rs".into(), 3, None)]);
    assert_eq!(
        foo_bindings(&db, "helper"),
        [
            ("app/src/x.rs".into(), 7, None),
            (
                "app/src/y.rs".into(),
                2,
                Some("sym:app/src/x.rs#helper".into())
            ),
        ]
    );
    assert_eq!(
        foo_bindings(&db, "nested"),
        [("app/src/x.rs".into(), 11, None)]
    );
    assert!(!reaches(&db, "g", "sym:app/src/x.rs#helper"));
    assert!(!reaches(&db, "h", "sym:app/src/x.rs#nested"));
    assert!(reaches(&db, "k", "sym:app/src/x.rs#helper"));
    let impact = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: "sym:app/src/x.rs#helper".into(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();
    let callers = serde_json::to_string(&impact).unwrap();
    assert!(!callers.contains("x.rs#g"), "{callers}");
    let card = codesage_graph::describe(
        _dir.path(),
        &db,
        "sym:app/src/x.rs#helper",
        &codesage_graph::DescribeOptions {
            sections: Some(vec!["hotness".into(), "references".into()]),
            ..Default::default()
        },
    )
    .unwrap()
    .card
    .unwrap();
    let card = serde_json::to_value(&card).unwrap();
    assert_eq!(card["sections"]["hotness"]["data"]["fan_in"], 1, "{card}");
    assert_eq!(
        card["sections"]["references"]["data"]["callers_total"], 1,
        "{card}"
    );
}

#[test]
fn rows_outside_a_shadowed_macro_bind_in_every_file_and_after_edits() {
    let mut files: Vec<(String, String)> = vec![
        (
            "Cargo.toml".into(),
            "[package]\nname='cap'\nversion='0.0.0'\nedition='2021'\n".into(),
        ),
        (
            "src/macros.rs".into(),
            "macro_rules! assert { ($e:expr) => { () }; }\n".into(),
        ),
    ];
    let mut lib = String::from("#[macro_use]\nmod macros;\npub fn helper() -> i32 { 1 }\n");
    for n in 0..70 {
        lib.push_str(&format!("pub mod f{n:02};\n"));
        files.push((
            format!("src/f{n:02}.rs"),
            format!(
                "use crate::helper;\npub fn g{n}() -> i32 {{\n    assert!(true);\n    helper()\n}}\n"
            ),
        ));
    }
    files.push(("src/lib.rs".into(), lib));
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect();
    let (dir, db) = indexed(&borrowed);
    let bound = |db: &Database| {
        codesage_graph::find_references_with_budget(
            db,
            &FindReferencesRequest {
                symbol_name: "helper".into(),
                kind: Some(codesage_protocol::ReferenceKind::Call),
            },
            std::time::Duration::from_secs(600),
        )
        .unwrap()
        .results
        .into_iter()
        .filter(|row| row.to.as_deref() == Some("sym:src/lib.rs#helper"))
        .count()
    };
    assert_eq!(bound(&db), 70);
    let edited = dir.path().join("src/f00.rs");
    let mut source = std::fs::read_to_string(&edited).unwrap();
    source.push_str("// edited\n");
    std::fs::write(&edited, source).unwrap();
    assert_eq!(bound(&db), 70);
}

#[test]
fn a_use_renaming_a_macro_to_a_std_name_leaves_the_scope_unresolved() {
    let (_dir, db) = api_other_app(&[
        (
            "other/src/lib.rs",
            "pub fn foo() -> i32 { 2 }\n#[macro_export]\nmacro_rules! check { () => { use other as api; } }\n",
        ),
        (
            "app/src/main.rs",
            "mod x;\nfn main() {\n    println!(\"{}\", x::f());\n}\n",
        ),
        (
            "app/src/x.rs",
            "use other::check as assert;\npub fn f() -> i32 {\n    assert!();\n    api::foo()\n}\n",
        ),
    ]);
    assert_eq!(foo_bindings(&db, "foo"), [("app/src/x.rs".into(), 4, None)]);
}

#[test]
fn a_std_named_macro_exported_by_another_crate_does_not_shadow_the_caller() {
    let (_dir, db) = api_other_app(&[
        (
            "other/src/lib.rs",
            "pub fn foo() -> i32 { 2 }\n#[macro_export]\nmacro_rules! println { ($($e:tt)*) => { () }; }\n",
        ),
        (
            "app/src/main.rs",
            "fn main() {\n    println!(\"{}\", api::foo());\n}\n",
        ),
        (
            "app/src/bin/q2.rs",
            "fn main() {\n    println!(\"{}\", api::foo());\n}\n",
        ),
        (
            "stray/fixture.rs",
            "macro_rules! println { ($($e:tt)*) => { () }; }\n",
        ),
    ]);
    let api = Some("sym:api/src/lib.rs#foo".to_string());
    assert_eq!(
        foo_bindings(&db, "foo"),
        [
            ("app/src/bin/q2.rs".into(), 2, api.clone()),
            ("app/src/main.rs".into(), 2, api),
        ]
    );
}

#[test]
fn c_defines_with_std_macro_names_do_not_shadow_rust_macros() {
    let (_dir, db) = api_other_app(&[
        ("other/src/lib.rs", "pub fn foo() -> i32 { 2 }\n"),
        (
            "csrc/x.c",
            "#define vec 1\nint main(void) { return vec; }\n",
        ),
        (
            "app/src/main.rs",
            "fn main() {\n    let _v = vec![api::foo()];\n}\n",
        ),
    ]);
    assert_eq!(
        foo_bindings(&db, "foo"),
        [(
            "app/src/main.rs".into(),
            2,
            Some("sym:api/src/lib.rs#foo".into())
        )]
    );
}

#[test]
fn macros_in_unrelated_function_bodies_do_not_hide_qualified_calls() {
    let (_dir, db) = indexed(&[
        ("Cargo.toml", "[workspace]\nmembers=['api','app']\n"),
        (
            "api/Cargo.toml",
            "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\napi={path='../api'}\n",
        ),
        ("api/src/lib.rs", "pub fn foo() {}\n"),
        (
            "app/src/main.rs",
            "macro_rules! expression { () => { () } } fn unrelated() { expression!(); } fn main() { api::foo(); }\n",
        ),
    ]);
    assert!(reaches(&db, "main", "sym:api/src/lib.rs#foo"));
}

#[test]
fn build_scripts_do_not_resolve_through_normal_dependencies() {
    let mut false_edges = Vec::new();
    for build_path in [
        "build.rs",
        "src/build.rs",
        "src/main.rs",
        "src/bin/builder.rs",
    ] {
        let manifest = format!(
            "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\nbuild='{build_path}'\n[dependencies]\napi={{path='../api'}}\n[build-dependencies]\nother={{path='../other'}}\n"
        );
        let build_file = format!("app/{build_path}");
        let mut files = vec![
            ("Cargo.toml", "[workspace]\nmembers=['api','other','app']\n"),
            (
                "api/Cargo.toml",
                "[package]\nname='api'\nversion='0.0.0'\nedition='2024'\n",
            ),
            (
                "other/Cargo.toml",
                "[package]\nname='other'\nversion='0.0.0'\nedition='2024'\n[lib]\nname='api'\n",
            ),
            ("app/Cargo.toml", manifest.as_str()),
            ("api/src/lib.rs", "pub fn foo() {}\n"),
            ("other/src/lib.rs", "pub fn foo() {}\n"),
            (build_file.as_str(), "fn main() { api::foo(); }\n"),
        ];
        if build_path != "src/main.rs" {
            files.push((
                "app/src/main.rs",
                if build_path == "src/build.rs" {
                    "mod build; fn main() {}"
                } else {
                    "fn main() {}"
                },
            ));
        }
        let (_dir, db) = indexed(&files);
        if reaches(
            &db,
            &format!("sym:{build_file}#main"),
            "sym:api/src/lib.rs#foo",
        ) {
            false_edges.push(build_path);
        }
    }
    assert!(
        false_edges.is_empty(),
        "build dependency edges: {false_edges:?}"
    );
}
