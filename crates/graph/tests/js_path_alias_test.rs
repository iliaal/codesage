//! `tsconfig`/`jsconfig` `paths` and `baseUrl` aliases resolve like relative
//! specifiers, and renamed JS/TS imports bind their uses to the imported item.

use codesage_graph::{
    file_import_pairs, full_index, impact_analysis, incremental_index, index_files,
    list_dependencies, trace_call_path,
};
use codesage_parser::discover::content_hash;
use codesage_protocol::{
    CallPathRequest, FileInfo, ImpactRequest, ImpactTarget, Language, ReferenceKind,
};
use codesage_storage::Database;
use std::path::Path;

fn write(root: &Path, files: &[(&str, &str)]) {
    for (path, source) in files {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, source).unwrap();
    }
}

fn index_tree(files: &[(&str, &str)]) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), files);
    let db = Database::open_in_memory().unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    (dir, db)
}

fn imports(db: &Database, file: &str) -> Vec<String> {
    list_dependencies(db, file).unwrap().imports
}

fn unresolved(db: &Database, file: &str) -> Vec<String> {
    list_dependencies(db, file)
        .unwrap()
        .unresolved_imports
        .into_iter()
        .map(|import| import.target)
        .collect()
}

fn impacted(db: &Database, target: ImpactTarget) -> Vec<String> {
    impact_analysis(
        db,
        &ImpactRequest {
            target,
            depth: 1,
            source_only: false,
        },
    )
    .unwrap()
    .into_iter()
    .map(|entry| entry.file_path)
    .collect()
}

/// The common Vite monorepo layout: a solution `tsconfig.json` with
/// `files: []` references `tsconfig.app.json`, which governs `src/` and
/// declares the aliases, while the solution config extends a root base.
const MONOREPO: &[(&str, &str)] = &[
    (
        "tsconfig.base.json",
        "{ \"compilerOptions\": { \"strict\": true } }\n",
    ),
    (
        "apps/web/tsconfig.json",
        r#"{
  // Solution-style config.
  "extends": "../../tsconfig.base.json",
  "files": [],
  "references": [{ "path": "./tsconfig.app.json" }, { "path": "./tsconfig.node.json" }],
  "compilerOptions": { "paths": { "@/*": ["./src/*"] } },
}
"#,
    ),
    (
        "apps/web/tsconfig.app.json",
        r#"{
  "compilerOptions": {
    /* Aliases */
    "paths": {
      "@/*": ["./src/*"],
      "@pages/*": ["./src/pages/*"],
    },
  },
  "include": ["src"],
  "exclude": ["src/**/*.test.ts"],
}
"#,
    ),
    (
        "apps/web/tsconfig.node.json",
        "{ \"include\": [\"vite.config.ts\"] }\n",
    ),
    (
        "apps/web/src/router.tsx",
        "import { routeTree } from '@/routeTree';\n\
         import { NotFound } from '@pages/NotFound/NotFound';\n\
         import { missing } from '@pages/Missing';\n\
         import { useQuery } from '@tanstack/react-query';\n\
         export function router() { return [routeTree, NotFound(), missing, useQuery]; }\n",
    ),
    (
        "apps/web/src/routeTree.ts",
        "import { router } from '@/router';\nexport const routeTree = router;\n",
    ),
    (
        "apps/web/src/pages/NotFound/NotFound.tsx",
        "export function NotFound() { return null; }\n",
    ),
    (
        "apps/web/src/pages/NotFound/NotFound.test.ts",
        "import { NotFound } from '@pages/NotFound/NotFound';\nNotFound();\n",
    ),
    (
        "apps/web/scripts/gen.ts",
        "import { routeTree } from '@/routeTree';\nexport const g = routeTree;\n",
    ),
    (
        "apps/other/src/main.ts",
        "import { routeTree } from '@/routeTree';\nexport const x = routeTree;\n",
    ),
];

#[test]
fn solution_config_references_supply_the_governing_paths() {
    let (_dir, db) = index_tree(MONOREPO);
    assert_eq!(
        imports(&db, "apps/web/src/router.tsx"),
        [
            "file:apps/web/src/pages/NotFound/NotFound.tsx",
            "file:apps/web/src/routeTree.ts",
        ]
    );
    assert_eq!(
        unresolved(&db, "apps/web/src/router.tsx"),
        ["@pages/Missing", "@tanstack/react-query"]
    );
    // No config governs `apps/other`, so its `@/` stays a package name.
    assert_eq!(unresolved(&db, "apps/other/src/main.ts"), ["@/routeTree"]);
    // The test file matches `tsconfig.app.json`'s include but its exclude
    // removes it, so that config's `@pages/*` still applies; a script no
    // include matches gets no aliases, as in TypeScript.
    assert_eq!(unresolved(&db, "apps/web/scripts/gen.ts"), ["@/routeTree"]);
    assert_eq!(
        list_dependencies(&db, "apps/web/src/pages/NotFound/NotFound.test.ts")
            .unwrap()
            .test_imports,
        ["file:apps/web/src/pages/NotFound/NotFound.tsx"]
    );
    assert_eq!(
        list_dependencies(&db, "apps/web/src/routeTree.ts")
            .unwrap()
            .imported_by,
        ["apps/web/src/router.tsx"]
    );
    let pairs = file_import_pairs(&db).unwrap();
    for pair in [
        ("apps/web/src/router.tsx", "apps/web/src/routeTree.ts"),
        ("apps/web/src/routeTree.ts", "apps/web/src/router.tsx"),
    ] {
        assert!(
            pairs.eager.contains(&(pair.0.into(), pair.1.into())),
            "{pair:?} in {pairs:?}"
        );
    }
    let affected = impacted(
        &db,
        ImpactTarget::File {
            path: "apps/web/src/pages/NotFound/NotFound.tsx".into(),
        },
    );
    assert!(
        affected.contains(&"apps/web/src/router.tsx".to_string()),
        "{affected:?}"
    );
}

#[test]
fn extended_paths_resolve_against_the_declaring_config_and_base_url() {
    let (_dir, db) = index_tree(&[
        (
            "config/tsconfig.shared.json",
            r##"{ "compilerOptions": { "paths": { "#lib/*": ["../packages/lib/src/*"] } } }"##,
        ),
        (
            "app/tsconfig.json",
            r#"{ "extends": "../config/tsconfig.shared", "compilerOptions": {} }"#,
        ),
        (
            "app/main.ts",
            "import { a } from '#lib/a';\nexport const m = a;\n",
        ),
        ("packages/lib/src/a.ts", "export const a = 1;\n"),
        (
            "web/jsconfig.json",
            r#"{ "compilerOptions": { "baseUrl": "src", "paths": { "exact": ["special/exact.js"], "*": ["missing/*", "vendor/*"] } } }"#,
        ),
        (
            "web/src/main.js",
            "import a from 'exact';\nimport b from 'tool';\nimport c from 'util/strings';\nexport default [a, b, c];\n",
        ),
        ("web/src/special/exact.js", "export default 1;\n"),
        ("web/src/vendor/tool/index.js", "export default 2;\n"),
        ("web/src/util/strings.js", "export default 3;\n"),
    ]);
    assert_eq!(imports(&db, "app/main.ts"), ["file:packages/lib/src/a.ts"]);
    // Exact pattern first; the wildcard's second substitution when the first
    // names nothing; `baseUrl` when no substitution resolves.
    assert_eq!(
        imports(&db, "web/src/main.js"),
        [
            "file:web/src/special/exact.js",
            "file:web/src/util/strings.js",
            "file:web/src/vendor/tool/index.js",
        ]
    );
}

#[test]
fn config_dir_bom_and_array_extends_follow_typescript() {
    let (_dir, db) = index_tree(&[
        (
            "cfg/tsconfig.shared.json",
            r#"{ "compilerOptions": { "paths": { "~/*": ["${configDir}/src/*"] } } }"#,
        ),
        (
            "pkg/tsconfig.json",
            r#"{ "extends": "../cfg/tsconfig.shared.json" }"#,
        ),
        ("pkg/src/a.ts", "export const a = 1;\n"),
        (
            "pkg/src/main.ts",
            "import { a } from '~/a';\nexport const m = a;\n",
        ),
        (
            "bom/tsconfig.json",
            "\u{feff}{ \"compilerOptions\": { \"paths\": { \"@/*\": [\"src/*\"] } } }\n",
        ),
        (
            "bom/main.ts",
            "import { a } from '@/a';\nexport const m = a;\n",
        ),
        ("bom/src/a.ts", "export const a = 1;\n"),
        (
            "arr/tsconfig.one.json",
            r##"{ "compilerOptions": { "paths": { "#x/*": ["./src/a/*"] } } }"##,
        ),
        (
            "arr/tsconfig.two.json",
            r##"{ "compilerOptions": { "paths": { "#x/*": ["./src/b/*"] } } }"##,
        ),
        (
            "arr/tsconfig.json",
            r#"{ "extends": ["./tsconfig.one.json", "./tsconfig.two.json"] }"#,
        ),
        (
            "arr/src/main.ts",
            "import { v } from '#x/v';\nexport const w = v;\n",
        ),
        ("arr/src/a/v.ts", "export const v = 1;\n"),
        ("arr/src/b/v.ts", "export const v = 2;\n"),
    ]);
    assert_eq!(imports(&db, "pkg/src/main.ts"), ["file:pkg/src/a.ts"]);
    assert_eq!(imports(&db, "bom/main.ts"), ["file:bom/src/a.ts"]);
    assert_eq!(imports(&db, "arr/src/main.ts"), ["file:arr/src/b/v.ts"]);
}

#[test]
fn inherited_paths_are_stored_once_and_queries_do_constant_config_work() {
    let mut files = vec![(
        "tsconfig.base.json".to_string(),
        format!(
            r#"{{ "compilerOptions": {{ "paths": {{ {} }} }} }}"#,
            (0..200)
                .map(|i| format!(r#""@org/lib{i}": ["libs/lib{i}/src/index.ts"]"#))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )];
    for i in 0..200 {
        files.push((
            format!("libs/lib{i}/tsconfig.json"),
            r#"{ "extends": "../../tsconfig.base.json", "files": [], "references": [{ "path": "./tsconfig.lib.json" }] }"#.into(),
        ));
        files.push((
            format!("libs/lib{i}/tsconfig.lib.json"),
            r#"{ "extends": "./tsconfig.json", "include": ["src/**/*.ts"] }"#.into(),
        ));
        files.push((
            format!("libs/lib{i}/src/index.ts"),
            format!(
                "import {{ v }} from '@org/lib{}';\nimport React from 'react';\nexport const x{i} = v;\n",
                (i + 1) % 200
            ),
        ));
    }
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, source)| (path.as_str(), source.as_str()))
        .collect();
    let (_dir, db) = index_tree(&borrowed);
    let configs = db.js_path_configs().unwrap();
    assert_eq!(configs.configs().len(), 401);
    assert_eq!(
        configs
            .configs()
            .iter()
            .filter(|config| !config.declared_paths.is_empty())
            .count(),
        1,
        "inherited paths live only on the declaring config"
    );
    assert_eq!(db.js_import_mappings().unwrap().len(), 200);
    let loads = db.js_import_mapping_loads();
    let config_loads = db.js_path_config_loads();
    let work = configs.work();
    for _ in 0..3 {
        assert_eq!(
            imports(&db, "libs/lib5/src/index.ts"),
            ["file:libs/lib6/src/index.ts"]
        );
        impacted(
            &db,
            ImpactTarget::File {
                path: "libs/lib6/src/index.ts".into(),
            },
        );
        file_import_pairs(&db).unwrap();
    }
    // Queries read the stored mappings once per database generation and
    // never walk configs or patterns, however many exist.
    assert_eq!(db.js_import_mapping_loads(), loads);
    assert_eq!(db.js_path_config_loads(), config_loads);
    assert_eq!(configs.work(), work);
}

#[test]
fn editing_a_config_changes_resolution_on_the_next_incremental_index() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "paths": { "~/*": ["src/a/*"] } } }"#,
            ),
            ("main.ts", "import { x } from '~/x';\nexport const y = x;\n"),
            ("src/a/x.ts", "export const x = 1;\n"),
            ("src/b/x.ts", "export const x = 2;\n"),
        ],
    );
    let db = Database::open_in_memory().unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert_eq!(imports(&db, "main.ts"), ["file:src/a/x.ts"]);
    write(
        dir.path(),
        &[(
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "~/*": ["src/b/*"] } } }"#,
        )],
    );
    incremental_index(dir.path(), &db, &[], false).unwrap();
    assert_eq!(imports(&db, "main.ts"), ["file:src/b/x.ts"]);
    std::fs::remove_file(dir.path().join("tsconfig.json")).unwrap();
    incremental_index(dir.path(), &db, &[], false).unwrap();
    assert!(imports(&db, "main.ts").is_empty());
}

#[test]
fn a_no_op_pass_restores_mappings_an_interrupted_pass_left_stale() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "paths": { "@/*": ["src/*"] } } }"#,
            ),
            ("main.ts", "import { a } from '@/a';\nexport const m = a;\n"),
            ("src/a.ts", "export const a = 1;\n"),
            ("tool.py", "x = 1\n"),
        ],
    );
    let db = Database::open_in_memory().unwrap();
    full_index(dir.path(), &db, &[], false).unwrap();
    assert_eq!(imports(&db, "main.ts"), ["file:src/a.ts"]);
    let info = |path: &str, language| FileInfo {
        path: path.into(),
        language,
        content_hash: content_hash(&std::fs::read(dir.path().join(path)).unwrap()),
        is_test: false,
    };

    // Rows lost without the stale flag: a pass with nothing to do keeps
    // them, and so does a single-file pass over another language.
    db.replace_js_import_mappings(&[]).unwrap();
    incremental_index(dir.path(), &db, &[], false).unwrap();
    assert!(imports(&db, "main.ts").is_empty());
    index_files(dir.path(), &db, &[info("tool.py", Language::Python)], false).unwrap();
    assert!(imports(&db, "main.ts").is_empty());
    // A single-file pass over a JS/TS file remaps.
    index_files(
        dir.path(),
        &db,
        &[info("main.ts", Language::TypeScript)],
        false,
    )
    .unwrap();
    assert_eq!(imports(&db, "main.ts"), ["file:src/a.ts"]);

    // An interrupted pass leaves the flag set: the next no-op pass remaps.
    db.replace_js_import_mappings(&[]).unwrap();
    db.mark_js_aliases_stale().unwrap();
    incremental_index(dir.path(), &db, &[], false).unwrap();
    assert_eq!(imports(&db, "main.ts"), ["file:src/a.ts"]);
    assert!(!db.js_aliases_stale().unwrap());

    // So does a single-file pass over another language.
    db.replace_js_import_mappings(&[]).unwrap();
    db.mark_js_aliases_stale().unwrap();
    index_files(dir.path(), &db, &[info("tool.py", Language::Python)], false).unwrap();
    assert_eq!(imports(&db, "main.ts"), ["file:src/a.ts"]);
}

#[test]
fn a_single_file_pass_reads_a_config_new_to_its_directory() {
    let (dir, db) = index_tree(&[("other/main.ts", "export const o = 1;\n")]);
    write(
        dir.path(),
        &[
            (
                "pkg/tsconfig.json",
                r#"{ "compilerOptions": { "paths": { "@/*": ["src/*"] } } }"#,
            ),
            ("pkg/src/a.ts", "export const a = 1;\n"),
            (
                "pkg/main.ts",
                "import { a } from '@/a';\nexport const m = a;\n",
            ),
        ],
    );
    let infos: Vec<FileInfo> = ["pkg/src/a.ts", "pkg/main.ts"]
        .into_iter()
        .map(|path| FileInfo {
            path: path.into(),
            language: Language::TypeScript,
            content_hash: content_hash(&std::fs::read(dir.path().join(path)).unwrap()),
            is_test: false,
        })
        .collect();
    index_files(dir.path(), &db, &infos, false).unwrap();
    assert_eq!(imports(&db, "pkg/main.ts"), ["file:pkg/src/a.ts"]);
}

#[test]
fn cyclic_extends_and_configs_outside_the_project_are_ignored() {
    let outside = tempfile::tempdir().unwrap();
    write(
        outside.path(),
        &[(
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@x/*": ["./*"] } } }"#,
        )],
    );
    let (dir, db) = index_tree(&[
        (
            "a/tsconfig.json",
            r#"{ "extends": "./tsconfig.b.json", "compilerOptions": { "paths": { "@a/*": ["./*"] } } }"#,
        ),
        ("a/tsconfig.b.json", r#"{ "extends": "./tsconfig.json" }"#),
        (
            "a/main.ts",
            "import { v } from '@a/v';\nexport const w = v;\n",
        ),
        ("a/v.ts", "export const v = 1;\n"),
        (
            "b/main.ts",
            "import { v } from '@x/v';\nexport const w = v;\n",
        ),
        ("b/v.ts", "export const v = 1;\n"),
    ]);
    assert_eq!(imports(&db, "a/main.ts"), ["file:a/v.ts"]);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            outside.path().join("tsconfig.json"),
            dir.path().join("b/tsconfig.json"),
        )
        .unwrap();
        incremental_index(dir.path(), &db, &[], false).unwrap();
        assert_eq!(unresolved(&db, "b/main.ts"), ["@x/v"]);
    }
}

#[test]
fn a_renamed_import_never_binds_to_a_same_file_declaration_of_its_name() {
    let (_dir, db) = index_tree(&[
        (
            "src/base.ts",
            "export function render(): number { return 1; }\n",
        ),
        (
            "src/wrap2.ts",
            "import { render as baseRender } from './base';\n\
             export function render(): number { return 7; }\n\
             export function other(): number { return baseRender(); }\n",
        ),
    ]);
    let report = trace_call_path(
        &db,
        &CallPathRequest {
            from: "sym:src/wrap2.ts#other".into(),
            to: "sym:src/wrap2.ts#render".into(),
            max_depth: 3,
        },
    )
    .unwrap();
    assert!(!report.found, "{report:?}");
}

#[test]
fn renamed_imports_bind_calls_to_the_imported_item() {
    let (_dir, db) = index_tree(&[
        (
            "src/jwt/jwt.ts",
            "export function verifyToken(t: string) { return t; }\n",
        ),
        (
            "src/a.ts",
            "import { verifyToken as vt } from './jwt/jwt';\nexport function check(t: string) { return vt(t); }\n",
        ),
        (
            "src/r.js",
            "import { other as renamed } from './x.js';\nexport function run() { return renamed(); }\n",
        ),
        ("src/x.js", "export function other() { return 1; }\n"),
    ]);
    for (from, to) in [
        ("check", "sym:src/jwt/jwt.ts#verifyToken"),
        ("run", "sym:src/x.js#other"),
    ] {
        let report = trace_call_path(
            &db,
            &CallPathRequest {
                from: from.into(),
                to: to.into(),
                max_depth: 3,
            },
        )
        .unwrap();
        assert!(report.found, "{from} -> {to}: {report:?}");
    }
    let entries = impact_analysis(
        &db,
        &ImpactRequest {
            target: ImpactTarget::Symbol {
                name: "sym:src/jwt/jwt.ts#verifyToken".into(),
            },
            depth: 1,
            source_only: false,
        },
    )
    .unwrap();
    let caller = entries
        .iter()
        .find(|entry| entry.file_path == "src/a.ts")
        .expect("the aliasing file is impacted");
    assert!(
        caller
            .reasons
            .iter()
            .any(|reason| reason.kind == ReferenceKind::Call && reason.line == 2),
        "{:?}",
        caller.reasons
    );
}
