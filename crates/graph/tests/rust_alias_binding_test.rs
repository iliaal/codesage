use codesage_graph::{find_references, full_index, impact_analysis, trace_call_path};
use codesage_protocol::{
    CallPathRequest, FindReferencesRequest, ImpactRequest, ImpactTarget, ReferenceKind,
};
use codesage_storage::Database;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let mut all = vec![(
        "Cargo.toml",
        "[package]\nname = \"alias_binding_probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n",
    )];
    all.extend_from_slice(files);
    let (root, db) = indexed(&all);
    (root, db)
}

/// Indexes `files` as written, manifests included.
fn indexed(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let root = tempfile::tempdir().unwrap();
    for (file, source) in files {
        let path = root.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    std::fs::create_dir(root.path().join(".codesage")).unwrap();
    let db = Database::open(&root.path().join(".codesage/index.db")).unwrap();
    let sources = files
        .iter()
        .filter(|(file, _)| file.ends_with(".rs"))
        .count();
    assert_eq!(
        full_index(root.path(), &db, &[], false)
            .unwrap()
            .files_indexed,
        sources
    );
    (root, db)
}

fn trace(db: &Database, from: &str, to: &str) -> bool {
    trace_call_path(
        db,
        &CallPathRequest {
            from: from.to_string(),
            to: to.to_string(),
            max_depth: 1,
        },
    )
    .unwrap()
    .found
}

/// `(file, kind, line)` of every reverse-impact reason on `target`.
fn impact_reasons(db: &Database, target: &str) -> Vec<(String, ReferenceKind, u32)> {
    impact_analysis(
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
    .flat_map(|entry| {
        entry
            .reasons
            .into_iter()
            .map(move |reason| (entry.file_path.clone(), reason.kind, reason.line))
    })
    .collect()
}

/// `(from, to)` handles of the `find_references` rows spelled `spelling`.
fn bound_rows(db: &Database, query: &str, spelling: &str) -> Vec<(String, Option<String>)> {
    bound_rows_of_kind(db, query, spelling, None)
}

fn bound_rows_of_kind(
    db: &Database,
    query: &str,
    spelling: &str,
    kind: Option<ReferenceKind>,
) -> Vec<(String, Option<String>)> {
    find_references(
        db,
        &FindReferencesRequest {
            symbol_name: query.into(),
            kind,
        },
    )
    .unwrap()
    .results
    .into_iter()
    .filter(|row| row.to_name == spelling)
    .map(|row| (row.from_handle().unwrap().to_string(), row.to))
    .collect()
}

#[test]
fn renamed_function_imports_bind_their_callers() {
    let (_root, db) = project(&[
        (
            "src/lib.rs",
            "pub mod a;\npub mod b;\npub mod decoy;\npub mod grouped;\npub mod plain;\npub mod reexport;\npub mod user;\npub fn helper_fn() -> u32 { 7 }\n",
        ),
        (
            "src/a/mod.rs",
            "pub mod caller;\npub mod x;\npub fn top() {}\n",
        ),
        ("src/a/x.rs", "pub fn other() {}\npub fn deep() {}\n"),
        (
            "src/a/caller.rs",
            "use super::x::other as via_super;\n\npub fn calls_super() {\n    via_super();\n}\n",
        ),
        (
            "src/b.rs",
            "pub mod m;\nuse self::m::leaf as via_self;\n\npub fn calls_self() {\n    via_self();\n}\n",
        ),
        ("src/b/m.rs", "pub fn leaf() {}\n"),
        (
            "src/decoy.rs",
            "pub fn via_super() {}\npub fn renamed_fn() {}\n",
        ),
        (
            "src/grouped.rs",
            "use crate::a::{top, x::deep as grouped_deep};\nuse crate::{a::{x::{other as nested_other}}};\nuse crate::a::{self as a_mod};\n\npub fn calls_grouped() {\n    grouped_deep();\n    top();\n}\n\npub fn calls_nested() {\n    nested_other();\n}\n\npub fn calls_module() {\n    a_mod::top();\n}\n",
        ),
        (
            "src/plain.rs",
            "use crate::helper_fn as renamed_fn;\n\npub fn calls_renamed() -> u32 {\n    renamed_fn()\n}\n",
        ),
        (
            "src/reexport.rs",
            "pub use crate::helper_fn as exported_helper;\n",
        ),
        (
            "src/user.rs",
            "use crate::reexport::exported_helper;\nuse crate::reexport::exported_helper as again;\n\npub fn calls_reexport() -> u32 {\n    exported_helper()\n}\n\npub fn calls_again() -> u32 {\n    again()\n}\n",
        ),
    ]);
    let cases = [
        (
            "src/plain.rs",
            "calls_renamed",
            "renamed_fn",
            4,
            "src/lib.rs#helper_fn",
        ),
        (
            "src/a/caller.rs",
            "calls_super",
            "via_super",
            4,
            "src/a/x.rs#other",
        ),
        ("src/b.rs", "calls_self", "via_self", 5, "src/b/m.rs#leaf"),
        (
            "src/grouped.rs",
            "calls_grouped",
            "grouped_deep",
            6,
            "src/a/x.rs#deep",
        ),
        (
            "src/grouped.rs",
            "calls_nested",
            "nested_other",
            11,
            "src/a/x.rs#other",
        ),
        (
            "src/user.rs",
            "calls_reexport",
            "exported_helper",
            5,
            "src/lib.rs#helper_fn",
        ),
        (
            "src/user.rs",
            "calls_again",
            "again",
            9,
            "src/lib.rs#helper_fn",
        ),
    ];
    for (file, caller, spelling, line, target) in cases {
        let caller = format!("sym:{file}#{caller}");
        let target = format!("sym:{target}");
        assert!(trace(&db, &caller, &target), "{caller} -> {target}");
        assert!(
            impact_reasons(&db, &target).contains(&(file.into(), ReferenceKind::Call, line)),
            "{caller} -> {target}: {:?}",
            impact_reasons(&db, &target)
        );
        assert_eq!(
            bound_rows(&db, &target, spelling),
            [(caller.clone(), Some(target.clone()))],
            "{caller} -> {target}"
        );
        assert_eq!(
            bound_rows_of_kind(&db, &target, spelling, Some(ReferenceKind::Call)),
            [(caller.clone(), Some(target.clone()))],
            "{caller} -> {target} with kind call"
        );
        assert_eq!(
            bound_rows(&db, spelling, spelling)
                .into_iter()
                .filter(|(from, _)| *from == caller)
                .collect::<Vec<_>>(),
            [(caller.clone(), Some(target.clone()))],
            "{caller} spelled {spelling}"
        );
    }
    // A renamed module (`{self as a_mod}`) qualifies the call it prefixes.
    assert!(trace(
        &db,
        "sym:src/grouped.rs#calls_module",
        "sym:src/a/mod.rs#top"
    ));
    assert_eq!(
        bound_rows(&db, "a_mod::top", "a_mod::top"),
        [(
            "sym:src/grouped.rs#calls_module".to_string(),
            Some("sym:src/a/mod.rs#top".to_string())
        )]
    );
    // A same-named definition elsewhere is not what the alias names.
    assert!(!trace(
        &db,
        "sym:src/a/caller.rs#calls_super",
        "sym:src/decoy.rs#via_super"
    ));
    assert!(!trace(
        &db,
        "sym:src/plain.rs#calls_renamed",
        "sym:src/decoy.rs#renamed_fn"
    ));
}

#[test]
fn renamed_macro_imports_keep_macros_and_functions_apart() {
    let (_root, db) = project(&[
        (
            "src/lib.rs",
            "#[macro_use]\npub mod macros;\npub mod shadow;\npub mod things;\npub mod user;\n",
        ),
        (
            "src/macros.rs",
            "#[macro_export]\nmacro_rules! make_thing {\n    ($e:expr) => { $e + 1 };\n}\n\n#[macro_export]\nmacro_rules! exported_mac {\n    () => {};\n}\n",
        ),
        (
            "src/things.rs",
            "pub fn helper_fn() {}\npub fn shared_fn() {}\n",
        ),
        (
            "src/user.rs",
            "use crate::make_thing as mk_alias;\nuse crate::make_thing as mac_only;\nuse crate::things::helper_fn as fn_only;\n\npub fn by_alias() -> u32 {\n    mk_alias!(2)\n}\n\npub fn by_path() -> u32 {\n    crate::make_thing!(1)\n}\n\npub fn macro_on_fn() {\n    fn_only!();\n}\n\npub fn fn_on_macro() {\n    mac_only();\n}\n",
        ),
        (
            "src/shadow.rs",
            "use crate::things::helper_fn as local_mac;\nuse crate::things::shared_fn as exported_mac;\n\nmacro_rules! local_mac {\n    () => {};\n}\n\npub fn calls_local() {\n    local_mac!();\n}\n\npub fn calls_exported() {\n    exported_mac!();\n}\n\npub fn calls_fn() {\n    local_mac();\n}\n",
        ),
    ]);
    let make_thing = "sym:src/macros.rs#make_thing";
    let helper_fn = "sym:src/things.rs#helper_fn";
    for (caller, target, found) in [
        ("sym:src/user.rs#by_alias", make_thing, true),
        ("sym:src/user.rs#by_path", make_thing, true),
        // A function import never satisfies `name!()`, nor a macro import `name()`.
        ("sym:src/user.rs#macro_on_fn", helper_fn, false),
        ("sym:src/user.rs#fn_on_macro", make_thing, false),
        // ripwire S1: the same-file `macro_rules!` wins `local_mac!()`.
        (
            "sym:src/shadow.rs#calls_local",
            "sym:src/shadow.rs#local_mac",
            true,
        ),
        ("sym:src/shadow.rs#calls_local", helper_fn, false),
        // ripwire S2: the exported macro wins `exported_mac!()`.
        (
            "sym:src/shadow.rs#calls_exported",
            "sym:src/macros.rs#exported_mac",
            true,
        ),
        (
            "sym:src/shadow.rs#calls_exported",
            "sym:src/things.rs#shared_fn",
            false,
        ),
        // The function namespace still sees the renamed function.
        ("sym:src/shadow.rs#calls_fn", helper_fn, true),
    ] {
        assert_eq!(trace(&db, caller, target), found, "{caller} -> {target}");
    }
    let reasons = impact_reasons(&db, make_thing);
    assert!(
        reasons.contains(&("src/user.rs".into(), ReferenceKind::MacroCall, 6)),
        "{reasons:?}"
    );
    assert!(
        !reasons.contains(&("src/user.rs".into(), ReferenceKind::Call, 18)),
        "{reasons:?}"
    );
    let reasons = impact_reasons(&db, helper_fn);
    assert!(
        reasons.contains(&("src/shadow.rs".into(), ReferenceKind::Call, 17)),
        "{reasons:?}"
    );
    assert!(
        !reasons
            .iter()
            .any(|(_, kind, _)| *kind == ReferenceKind::MacroCall),
        "{reasons:?}"
    );
    assert_eq!(
        bound_rows(&db, "mk_alias", "mk_alias"),
        [(
            "sym:src/user.rs#by_alias".to_string(),
            Some(make_thing.to_string())
        )]
    );
}

/// A workspace whose crate `a` depends on `b`; `c` is unrelated and shares
/// item names with both.
fn workspace(a_files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let mut files = vec![
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
        ),
        (
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nb = { path = \"../b\" }\n",
        ),
        (
            "b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        (
            "b/src/lib.rs",
            "pub mod macros;\npub fn work() {}\npub fn helper() {}\n",
        ),
        (
            "b/src/macros.rs",
            "#[macro_export]\nmacro_rules! mk {\n    () => {};\n}\n",
        ),
        (
            "c/Cargo.toml",
            "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        (
            "c/src/lib.rs",
            "pub fn exposed() {}\npub fn crate_work() {}\npub fn dual() {}\npub fn inner() {}\n",
        ),
    ];
    files.extend_from_slice(a_files);
    indexed(&files)
}

#[test]
fn same_file_macro_wins_over_a_same_named_dependency_import() {
    let (_root, db) = workspace(&[
        ("a/src/lib.rs", "pub mod locmac;\npub mod locmac2;\n"),
        (
            "a/src/locmac.rs",
            "use b::mk;\n\nmacro_rules! mk {\n    () => {};\n}\n\npub fn lm1() {\n    mk!();\n}\n",
        ),
        (
            "a/src/locmac2.rs",
            "use b::macros;\n\nmacro_rules! macros {\n    () => {};\n}\n\npub fn lm2() {\n    macros!();\n}\n",
        ),
    ]);
    for (caller, target, found) in [
        ("sym:a/src/locmac.rs#lm1", "sym:a/src/locmac.rs#mk", true),
        ("sym:a/src/locmac.rs#lm1", "sym:b/src/macros.rs#mk", false),
        (
            "sym:a/src/locmac2.rs#lm2",
            "sym:a/src/locmac2.rs#macros",
            true,
        ),
    ] {
        assert_eq!(trace(&db, caller, target), found, "{caller} -> {target}");
    }
}

#[test]
fn crate_paths_bind_inside_the_callers_crate_only() {
    let (_root, db) = workspace(&[
        (
            "a/src/lib.rs",
            "#[macro_use]\npub mod macros;\npub mod util;\npub mod globby;\npub mod privy;\npub mod consumer;\npub mod crossdep;\npub mod attr;\npub mod user;\npub mod dual;\npub mod dualuse;\npub mod exported;\npub mod blockglob;\npub fn helper() {}\npub fn dual() {}\n",
        ),
        (
            "a/src/macros.rs",
            "#[macro_export]\nmacro_rules! mk {\n    () => {};\n}\n",
        ),
        ("a/src/util.rs", "pub fn work() {}\n"),
        (
            "a/src/exported.rs",
            "#[macro_export]\nmacro_rules! em {\n    () => {};\n}\n",
        ),
        (
            "a/src/globby.rs",
            "use super::*;\nuse crate::util::work as helper;\n\npub fn g1() {\n    helper();\n}\n",
        ),
        (
            "a/src/blockglob.rs",
            "use b::helper as work;\n\npub fn g2() {\n    use crate::util::*;\n    work();\n}\n",
        ),
        (
            "a/src/privy.rs",
            "mod inner {\n    pub(crate) fn hidden() {}\n}\npub(crate) use self::inner::hidden as exposed;\npub(crate) use crate::util::work as crate_work;\n",
        ),
        (
            "a/src/consumer.rs",
            "use crate::privy::exposed;\nuse crate::privy::crate_work as cw;\n\npub fn p1() {\n    exposed();\n}\n\npub fn p2() {\n    cw();\n}\n\npub fn p3() {\n    crate::privy::crate_work();\n}\n",
        ),
        (
            "a/src/crossdep.rs",
            "use crate::exported::em as via_module;\n\npub fn x1() {\n    b::mk!();\n}\n\npub fn x5() {\n    b::macros::mk!();\n}\n\npub fn x8() {\n    via_module!();\n}\n",
        ),
        (
            "a/src/attr.rs",
            "#[b::rewrite]\npub fn x6() {\n    b::mk!();\n}\n",
        ),
        (
            "a/src/user.rs",
            "pub fn c6_super() {\n    super::mk!();\n}\n",
        ),
        ("a/src/dual.rs", "pub fn inner() {}\n"),
        (
            "a/src/dualuse.rs",
            "use crate::dual as d;\n\npub fn d1() {\n    d();\n}\n\npub fn d2() {\n    d::inner();\n}\n",
        ),
    ]);
    let dual = db
        .find_symbols("dual", None)
        .unwrap()
        .into_iter()
        .find(|symbol| {
            symbol.file_path == "a/src/lib.rs"
                && symbol.kind == codesage_protocol::SymbolKind::Function
        })
        .unwrap()
        .handle()
        .to_string();
    let work = "sym:a/src/util.rs#work";
    for (caller, target, found) in [
        // An explicit `use` outranks the glob's `helper`.
        ("sym:a/src/globby.rs#g1", work, true),
        ("sym:a/src/globby.rs#g1", "sym:a/src/lib.rs#helper", false),
        // A glob inside the function shadows the file's explicit `use`
        // there, so the alias's target is no longer certain.
        (
            "sym:a/src/blockglob.rs#g2",
            "sym:b/src/lib.rs#helper",
            false,
        ),
        // A re-export the walk cannot follow (an inline module) stays unbound
        // rather than taking an unrelated crate's same-named item.
        (
            "sym:a/src/consumer.rs#p1",
            "sym:c/src/lib.rs#exposed",
            false,
        ),
        ("sym:a/src/consumer.rs#p2", work, true),
        ("sym:a/src/consumer.rs#p3", work, true),
        (
            "sym:a/src/consumer.rs#p3",
            "sym:c/src/lib.rs#crate_work",
            false,
        ),
        // An exported macro is reached at its crate root, not its module path.
        ("sym:a/src/crossdep.rs#x1", "sym:b/src/macros.rs#mk", true),
        ("sym:a/src/crossdep.rs#x5", "sym:b/src/macros.rs#mk", false),
        (
            "sym:a/src/crossdep.rs#x8",
            "sym:a/src/exported.rs#em",
            false,
        ),
        // A rewriting attribute keeps the invocations inside it unresolved.
        ("sym:a/src/attr.rs#x6", "sym:b/src/macros.rs#mk", false),
        // `super::` from a direct child of the root names the root.
        ("sym:a/src/user.rs#c6_super", "sym:a/src/macros.rs#mk", true),
        (
            "sym:a/src/user.rs#c6_super",
            "sym:b/src/macros.rs#mk",
            false,
        ),
        // A function beside a same-named module stays reachable.
        ("sym:a/src/dualuse.rs#d1", dual.as_str(), true),
        ("sym:a/src/dualuse.rs#d1", "sym:c/src/lib.rs#dual", false),
        ("sym:a/src/dualuse.rs#d2", "sym:a/src/dual.rs#inner", true),
        ("sym:a/src/dualuse.rs#d2", "sym:c/src/lib.rs#inner", false),
    ] {
        assert_eq!(trace(&db, caller, target), found, "{caller} -> {target}");
    }
}

#[test]
fn binary_roots_resolve_their_own_modules() {
    let (_root, db) = indexed(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\", \"other\"]\nresolver = \"2\"\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        (
            "app/src/main.rs",
            "mod util;\nmod caller;\nuse crate::util::work as w;\n\nfn main() {\n    w();\n    caller::go();\n}\n",
        ),
        ("app/src/util.rs", "pub fn work() {}\n"),
        (
            "app/src/caller.rs",
            "use crate::util::work as cw;\n\npub fn go() {\n    cw();\n}\n",
        ),
        (
            "app/src/bin/tool/main.rs",
            "mod helpers;\nuse crate::helpers::run as r;\n\nfn main() {\n    r();\n}\n",
        ),
        ("app/src/bin/tool/helpers.rs", "pub fn run() {}\n"),
        (
            "other/Cargo.toml",
            "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        (
            "other/src/lib.rs",
            "pub fn w() {}\npub fn cw() {}\npub fn r() {}\n",
        ),
    ]);
    for (caller, target, found) in [
        ("sym:app/src/main.rs#main", "sym:app/src/util.rs#work", true),
        ("sym:app/src/main.rs#main", "sym:other/src/lib.rs#w", false),
        ("sym:app/src/caller.rs#go", "sym:app/src/util.rs#work", true),
        ("sym:app/src/caller.rs#go", "sym:other/src/lib.rs#cw", false),
        (
            "sym:app/src/bin/tool/main.rs#main",
            "sym:app/src/bin/tool/helpers.rs#run",
            true,
        ),
        (
            "sym:app/src/bin/tool/main.rs#main",
            "sym:other/src/lib.rs#r",
            false,
        ),
    ] {
        assert_eq!(trace(&db, caller, target), found, "{caller} -> {target}");
    }
}

#[test]
fn renamed_import_of_a_self_reexported_macro_binds_at_its_module_path() {
    let (_root, db) = project(&[
        (
            "src/lib.rs",
            "pub mod reexp;\npub mod user;\npub mod plain;\n",
        ),
        (
            "src/reexp.rs",
            "macro_rules! inner_mac {\n    () => {};\n}\npub(crate) use inner_mac;\n",
        ),
        (
            "src/plain.rs",
            "macro_rules! plain_mac {\n    () => {};\n}\n",
        ),
        (
            "src/user.rs",
            "use crate::reexp::inner_mac as im;\nuse crate::plain::plain_mac as pm;\n\npub fn by_reexport() {\n    im!();\n}\n\npub fn by_module_path() {\n    pm!();\n}\n",
        ),
    ]);
    assert!(trace(
        &db,
        "sym:src/user.rs#by_reexport",
        "sym:src/reexp.rs#inner_mac"
    ));
    // Without the `use` of itself, a `macro_rules!` has no module path.
    assert!(!trace(
        &db,
        "sym:src/user.rs#by_module_path",
        "sym:src/plain.rs#plain_mac"
    ));
}

#[test]
fn a_lint_attributed_macro_reexported_by_itself_binds_at_its_module_path() {
    let (_root, db) = project(&[
        ("src/lib.rs", "pub mod host;\npub mod user;\n"),
        (
            "src/host.rs",
            "#[allow(unused_macros)]\nmacro_rules! attr_mac {\n    () => {};\n}\npub(crate) use attr_mac;\n\n#[cfg_attr(test, allow(unused_macros))]\nmacro_rules! cfg_mac {\n    () => {};\n}\npub(crate) use cfg_mac;\n",
        ),
        (
            "src/user.rs",
            "use crate::host::attr_mac as am;\n\npub fn u() {\n    crate::host::attr_mac!();\n    crate::host::cfg_mac!();\n    am!();\n}\n",
        ),
    ]);
    for target in ["sym:src/host.rs#attr_mac", "sym:src/host.rs#cfg_mac"] {
        assert!(trace(&db, "sym:src/user.rs#u", target), "{target}");
    }
    let rows = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: "sym:src/host.rs#attr_mac".into(),
            kind: None,
        },
    )
    .unwrap()
    .results;
    let mut bound: Vec<_> = rows
        .iter()
        .map(|row| (row.to_name.as_str(), row.kind, row.to.as_deref()))
        .collect();
    bound.sort_by_key(|(to_name, kind, _)| (*to_name, format!("{kind:?}")));
    let to = Some("sym:src/host.rs#attr_mac");
    assert_eq!(
        bound,
        [
            ("am", ReferenceKind::MacroCall, to),
            // The `use` of itself that re-exports it.
            ("attr_mac", ReferenceKind::Import, to),
            ("crate::host::attr_mac", ReferenceKind::Import, to),
            ("crate::host::attr_mac", ReferenceKind::MacroCall, to),
        ]
    );
}

#[test]
fn the_rename_screen_admits_the_same_spellings_on_every_call() {
    let mut files = vec![(
        "src/lib.rs".to_string(),
        "pub mod reexp;\npub fn base_fn() {}\n".to_string(),
    )];
    files.push((
        "src/reexp.rs".into(),
        "pub use crate::base_fn as x;\n".into(),
    ));
    for alias in ["mc", "ma", "mb"] {
        for index in 0..30 {
            let module = format!("{alias}{index:02}");
            files[0].1.push_str(&format!("pub mod {module};\n"));
            files.push((
                format!("src/{module}.rs"),
                format!("use crate::reexp as {alias};\n\npub fn f() {{\n    {alias}::x();\n}}\n"),
            ));
        }
    }
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(file, source)| (file.as_str(), source.as_str()))
        .collect();
    let (_root, db) = project(&borrowed);
    let bound_files = || {
        let out = find_references(
            &db,
            &FindReferencesRequest {
                symbol_name: "x".into(),
                kind: None,
            },
        )
        .unwrap();
        assert!(
            out.to_resolution
                .is_some_and(|resolution| resolution.capped)
        );
        let mut files: Vec<String> = out
            .results
            .into_iter()
            .filter(|row| row.to.as_deref() == Some("sym:src/lib.rs#base_fn"))
            .map(|row| row.from_file)
            .collect();
        files.sort();
        files
    };
    // 64 reads per request cover two 30-file spellings; sorted heads make
    // them `ma` and `mb` every time.
    let expected: Vec<String> = ["ma", "mb"]
        .iter()
        .flat_map(|alias| (0..30).map(move |index| format!("src/{alias}{index:02}.rs")))
        .collect();
    for _ in 0..6 {
        assert_eq!(bound_files(), expected);
    }
}

#[test]
fn importers_of_a_self_reexported_macro_bind_past_the_resolver_read_limit() {
    let mut files = vec![("src/lib.rs".to_string(), "pub mod host;\n".to_string())];
    files.push((
        "src/host.rs".into(),
        "#[allow(unused_macros)]\nmacro_rules! host_mac {\n    () => {};\n}\npub(crate) use host_mac;\n"
            .into(),
    ));
    // More importers than the external resolver's 64 source reads.
    for index in 0..70 {
        let module = format!("caller{index:02}");
        files[0].1.push_str(&format!("pub mod {module};\n"));
        files.push((
            format!("src/{module}.rs"),
            "use crate::host::host_mac;\n\npub fn f() {\n    host_mac!();\n}\n".into(),
        ));
    }
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(file, source)| (file.as_str(), source.as_str()))
        .collect();
    let (_root, db) = project(&borrowed);
    let imports = impact_reasons(&db, "sym:src/host.rs#host_mac")
        .into_iter()
        .filter(|(_, kind, _)| *kind == ReferenceKind::Import)
        .count();
    assert_eq!(imports, 70);
}
