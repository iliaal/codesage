//! Mapping orchestration: run every mapper, build [`FeatureRecord`]s from
//! seeds, attach nearby tests and trust boundaries, persist into the
//! `features` / `feature_files` / `feature_trust_boundaries` tables, and
//! garbage-collect features whose seed disappeared.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use codesage_parser::discover::build_exclude_set;
use codesage_protocol::{
    FeatureFileRef, FeatureFileRole, FeatureMapStats, FeatureRecord, Reference, ReferenceKind,
    TrustBoundary,
};
use codesage_storage::Database;

use crate::feature_id;
use crate::mappers::{
    c::CCppMapper,
    go::GoMapper,
    js::JsMapper,
    php::{PhpMapper, laravel_route_handler_refs},
    python::PythonMapper,
    rust::RustMapper,
    shared::walk_files,
    types::{FeatureMapper, FeatureSeed, MapperContext},
};
use crate::nearby_tests::nearby_tests;

/// Run every registered mapper, build `FeatureRecord`s from seeds, persist
/// them, and remove stale features. Returns stats for the caller.
///
/// `exclude_patterns` is the project's `[index].exclude_patterns` list. It
/// is compiled into a `GlobSet` once and threaded into every mapper through
/// `MapperContext` so feature output honors the same file-filter contract
/// as the structural indexer — no ghost features for files the rest of the
/// pipeline never sees.
///
/// When **any** mapper errors mid-collection, the orchestrator still
/// persists the seeds it did collect but **skips the garbage-collect
/// pass** — otherwise a single mapper failure (corrupted composer.json,
/// unreadable Cargo.toml, etc.) could silently delete every feature
/// owned by that language. Stale-feature debt is reconciled on the
/// next clean run.
pub fn map_features(
    root: &Path,
    db: &Database,
    exclude_patterns: &[String],
) -> Result<FeatureMapStats> {
    let excludes = if exclude_patterns.is_empty() {
        None
    } else {
        Some(build_exclude_set(exclude_patterns)?)
    };
    let ctx = MapperContext {
        root,
        excludes: excludes.as_ref(),
    };
    let collected = collect_seeds(&ctx)?;
    let seeds = collected.seeds;
    let any_mapper_errored = collected.any_errored;
    let mut keep_ids: Vec<String> = Vec::with_capacity(seeds.len());
    let mut created = 0usize;
    let mut updated = 0usize;
    // Snapshot of repo files for the nearby-test walker. Capped to keep
    // big repos under a couple seconds of wall-time.
    let all_files = walk_files(root, root, 50_000, ctx.excludes);
    db.execute_batch(|db| {
        for seed in &seeds {
            if !ctx.allowed(&seed.entry_path) {
                continue;
            }
            let mut record = build_record(db, seed, &all_files)?;
            // Final safety net: even when a mapper forgets to filter, no
            // FeatureFileRef should reference a path the structural
            // indexer excludes. Drop any leaked refs before persisting.
            record.files.retain(|f| ctx.allowed(&f.path));
            let exists = db.load_feature(&record.feature_id)?.is_some();
            db.upsert_feature(&record)?;
            keep_ids.push(record.feature_id.clone());
            if exists {
                updated += 1;
            } else {
                created += 1;
            }
        }
        Ok(())
    })?;
    // Framework route edges: synthesize `RouteHandler` references so
    // `impact_analysis` / `find_references` traverse Laravel routing.
    // Re-derived every run and rewritten wholesale (delete-of-kind then
    // insert) so edges from removed routes don't linger. Non-Laravel repos
    // produce an empty set; the delete then just clears any prior edges.
    let route_refs = laravel_route_handler_refs(root)?;
    db.execute_batch(|db| {
        db.delete_references_of_kind(ReferenceKind::RouteHandler)?;
        let mut by_file: BTreeMap<&str, Vec<Reference>> = BTreeMap::new();
        for r in &route_refs {
            by_file
                .entry(r.from_file.as_str())
                .or_default()
                .push(r.clone());
        }
        for (path, refs) in by_file {
            if let Some(file_id) = db.file_id_for_path(path)? {
                db.insert_references(file_id, &refs)?;
            }
        }
        Ok(())
    })?;

    let removed = if any_mapper_errored {
        tracing::warn!(
            "one or more mappers errored — skipping stale-feature GC to avoid deleting valid rows from an incomplete pass"
        );
        0
    } else {
        db.remove_features_not_in(&keep_ids)?
    };
    Ok(FeatureMapStats {
        created,
        updated,
        removed,
        total_features: db.feature_count()?,
    })
}

struct CollectedSeeds {
    seeds: Vec<FeatureSeed>,
    any_errored: bool,
}

/// Collect seeds from every mapper, deduped by `(kind, source, entry_path,
/// command|route|symbol)`. The `any_errored` flag lets the caller skip
/// destructive cleanup when a mapper failed mid-pass.
fn collect_seeds(ctx: &MapperContext) -> Result<CollectedSeeds> {
    let mappers: Vec<Box<dyn FeatureMapper>> = vec![
        Box::new(RustMapper),
        Box::new(PhpMapper),
        Box::new(CCppMapper),
        Box::new(PythonMapper),
        Box::new(JsMapper),
        Box::new(GoMapper),
    ];
    let mut all: Vec<FeatureSeed> = Vec::new();
    let mut any_errored = false;
    for m in mappers {
        match m.map(ctx) {
            Ok(s) => all.extend(s),
            Err(e) => {
                any_errored = true;
                tracing::warn!(mapper = m.name(), error = %e, "mapper failed");
            }
        }
    }
    let mut seen: BTreeSet<(String, String, String, String)> = BTreeSet::new();
    let mut out = Vec::with_capacity(all.len());
    for s in all {
        let disc = s.discriminator();
        let key = (
            s.kind.as_str().to_string(),
            s.source.to_string(),
            s.entry_path.clone(),
            disc,
        );
        if seen.insert(key) {
            out.push(s);
        }
    }
    Ok(CollectedSeeds {
        seeds: out,
        any_errored,
    })
}

fn build_record(db: &Database, seed: &FeatureSeed, all_files: &[String]) -> Result<FeatureRecord> {
    let disc = seed.discriminator();
    let feature_id = feature_id::build(seed.kind, seed.source, &seed.entry_path, &disc);

    // Build the file ref set: entry + owned + context + (seed tests union
    // nearby tests), deduped by PATH with role precedence Entry > Owned >
    // Context > Test (first insertion of a path wins). Keying by path alone is
    // load-bearing: several mappers — notably the C/C++ `filter_target_sources`
    // — return the entry file again inside `owned_files`, so a `(path, role)`
    // key would persist that file twice (once Entry, once Owned). The doc that
    // used to say "entry always wins over owned" now actually holds.
    let mut files_by_path: BTreeMap<String, FeatureFileRef> = BTreeMap::new();
    files_by_path.insert(
        seed.entry_path.clone(),
        FeatureFileRef {
            path: seed.entry_path.clone(),
            role: FeatureFileRole::Entry,
            reason: Some("entrypoint".to_string()),
        },
    );
    for f in &seed.owned_files {
        files_by_path
            .entry(f.path.clone())
            .or_insert(FeatureFileRef {
                path: f.path.clone(),
                role: FeatureFileRole::Owned,
                reason: Some(f.reason.clone()),
            });
    }
    for f in &seed.context_files {
        files_by_path
            .entry(f.path.clone())
            .or_insert(FeatureFileRef {
                path: f.path.clone(),
                role: FeatureFileRole::Context,
                reason: Some(f.reason.clone()),
            });
    }
    // Seed-attached tests.
    for t in &seed.tests {
        files_by_path
            .entry(t.path.clone())
            .or_insert(FeatureFileRef {
                path: t.path.clone(),
                role: FeatureFileRole::Test,
                reason: Some("seed test".to_string()),
            });
    }
    // Nearby-test discovery from filesystem conventions.
    let nearby = nearby_tests(seed, all_files);
    for path in nearby {
        files_by_path.entry(path.clone()).or_insert(FeatureFileRef {
            path,
            role: FeatureFileRole::Test,
            reason: Some("convention".to_string()),
        });
    }
    let files: Vec<FeatureFileRef> = files_by_path.into_values().collect();
    // Aggregate trust boundaries across owned + entry files (skip context
    // because context tends to be huge generic helpers and would dilute).
    let mut tb: BTreeSet<TrustBoundary> = BTreeSet::new();
    let boundary_inputs: Vec<&str> = files
        .iter()
        .filter(|f| matches!(f.role, FeatureFileRole::Entry | FeatureFileRole::Owned))
        .map(|f| f.path.as_str())
        .collect();
    for p in boundary_inputs {
        for b in db.trust_boundaries_for_file_path(p)? {
            tb.insert(b);
        }
    }
    let mut tags = seed.tags.clone();
    tags.sort();
    tags.dedup();
    Ok(FeatureRecord {
        feature_id,
        title: seed.title.clone(),
        summary: seed.summary.clone(),
        kind: seed.kind,
        source: seed.source.to_string(),
        confidence: seed.confidence,
        entry_path: seed.entry_path.clone(),
        entry_symbol: seed.entry_symbol.clone(),
        entry_route: seed.entry_route.clone(),
        entry_command: seed.entry_command.clone(),
        test_command: seed.test_command.clone(),
        language: seed.language,
        tags,
        trust_boundaries: tb.into_iter().collect(),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::FeatureKind;
    use tempfile::tempdir;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn entry_file_repeated_in_owned_is_deduped_to_entry_role() {
        use crate::mappers::types::SeedFile;
        use codesage_protocol::{FeatureConfidence, FeatureFileRole, Language};
        // The C/C++ mapper's filter_target_sources returns ALL target sources as
        // owned, including the file picked as the entry. build_record must
        // persist that file once, as Entry — not twice (Entry + Owned).
        let seed = FeatureSeed {
            title: "svc".into(),
            summary: String::new(),
            kind: FeatureKind::CliCommand,
            source: "cmake-target",
            confidence: FeatureConfidence::High,
            entry_path: "src/main.c".into(),
            entry_symbol: Some("main".into()),
            entry_route: None,
            entry_command: Some("svc".into()),
            test_command: None,
            language: Language::C,
            tags: Vec::new(),
            owned_files: vec![
                SeedFile {
                    path: "src/main.c".into(),
                    reason: "target source".into(),
                },
                SeedFile {
                    path: "src/helper.c".into(),
                    reason: "target source".into(),
                },
            ],
            context_files: Vec::new(),
            tests: Vec::new(),
            test_prefixes: Vec::new(),
        };
        let db = Database::open_in_memory().unwrap();
        let record = build_record(&db, &seed, &[]).unwrap();

        let main_refs: Vec<_> = record
            .files
            .iter()
            .filter(|f| f.path == "src/main.c")
            .collect();
        assert_eq!(
            main_refs.len(),
            1,
            "entry file must appear exactly once: {:?}",
            record.files
        );
        assert_eq!(main_refs[0].role, FeatureFileRole::Entry);
        assert!(
            record
                .files
                .iter()
                .any(|f| f.path == "src/helper.c" && f.role == FeatureFileRole::Owned),
            "non-entry owned file should remain: {:?}",
            record.files
        );
    }

    #[test]
    fn maps_simple_rust_repo_end_to_end() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
        );
        write(root, "src/main.rs", "fn main() {}");
        let db = Database::open_in_memory().unwrap();
        let stats = map_features(root, &db, &[]).unwrap();
        assert!(stats.created >= 1, "expected ≥1 created, got {stats:?}");
        let features = db
            .list_features(Some(FeatureKind::CliCommand), None, None, 100)
            .unwrap();
        assert!(features.iter().any(|f| f.entry_path == "src/main.rs"));
    }

    #[test]
    fn rerun_is_idempotent_and_ids_are_stable() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
        );
        write(root, "src/main.rs", "fn main() {}");
        let db = Database::open_in_memory().unwrap();
        let first = map_features(root, &db, &[]).unwrap();
        let ids_before: Vec<String> = db
            .list_features(None, None, None, 1000)
            .unwrap()
            .into_iter()
            .map(|f| f.feature_id)
            .collect();
        let second = map_features(root, &db, &[]).unwrap();
        let ids_after: Vec<String> = db
            .list_features(None, None, None, 1000)
            .unwrap()
            .into_iter()
            .map(|f| f.feature_id)
            .collect();
        assert_eq!(ids_before, ids_after, "feature ids must be stable");
        assert_eq!(first.total_features, second.total_features);
        assert_eq!(second.created, 0);
        assert!(second.updated >= 1);
    }

    #[test]
    fn laravel_route_handler_edge_emitted_and_idempotent() {
        use codesage_protocol::{FileInfo, Language, ReferenceKind};
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "routes/web.php",
            "<?php\nuse App\\Http\\Controllers\\UserController;\n\
             Route::get('/users', [UserController::class, 'index']);\n",
        );
        write(
            root,
            "app/Http/Controllers/UserController.php",
            "<?php\nnamespace App\\Http\\Controllers;\n\
             class UserController {\n  public function index() {}\n}\n",
        );
        let db = Database::open_in_memory().unwrap();
        // Structural index registers files before mapping in the real
        // pipeline; emulate it so `file_id_for_path` resolves the route file.
        db.upsert_file(&FileInfo {
            path: "routes/web.php".to_string(),
            language: Language::Php,
            content_hash: "h".to_string(),
        })
        .unwrap();

        map_features(root, &db, &[]).unwrap();

        // Qualified lookup (the impact_analysis path) resolves the edge.
        let qualified = db
            .find_references("App\\Http\\Controllers\\UserController\\index", None)
            .unwrap();
        assert_eq!(
            qualified.len(),
            1,
            "expected one route edge, got {qualified:?}"
        );
        assert_eq!(qualified[0].kind, ReferenceKind::RouteHandler);
        assert_eq!(qualified[0].from_file, "routes/web.php");

        // Unqualified (tail) lookup also resolves it.
        let by_tail = db.find_references("index", None).unwrap();
        assert!(
            by_tail
                .iter()
                .any(|r| r.kind == ReferenceKind::RouteHandler),
            "tail lookup missed route edge: {by_tail:?}"
        );

        // A second map run must not duplicate the edge.
        map_features(root, &db, &[]).unwrap();
        let after = db
            .find_references("App\\Http\\Controllers\\UserController\\index", None)
            .unwrap();
        assert_eq!(after.len(), 1, "remap duplicated the route edge: {after:?}");
    }

    #[test]
    fn map_features_respects_index_exclude_patterns() {
        // A Python script under `scripts/` would normally produce a
        // `python-main-guard` feature. With `scripts/**` in
        // `exclude_patterns`, the feature must be dropped — otherwise the
        // features pipeline emits rows whose entry_path is invisible to
        // the structural indexer (the "ghost feature" failure mode).
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "scripts/run.py",
            "if __name__ == '__main__':\n    pass\n",
        );
        write(
            root,
            "real_cli.py",
            "if __name__ == '__main__':\n    pass\n",
        );
        let db = Database::open_in_memory().unwrap();
        let stats = map_features(root, &db, &["scripts/**".to_string()]).unwrap();
        assert!(stats.created >= 1, "expected real_cli.py to map");
        let features = db.list_features(None, None, None, 100).unwrap();
        assert!(
            features.iter().any(|f| f.entry_path == "real_cli.py"),
            "expected real_cli.py feature, got {features:?}"
        );
        assert!(
            !features
                .iter()
                .any(|f| f.entry_path.starts_with("scripts/")),
            "scripts/** exclude not applied, got {features:?}"
        );
    }

    #[test]
    fn rust_bin_under_exclude_is_dropped() {
        // Regression: a `src/bin/<name>.rs` that matches
        // `[index].exclude_patterns` must not produce a cargo-bin
        // feature, otherwise the row references a path the structural
        // indexer ignored.
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
        );
        write(root, "src/main.rs", "fn main() {}");
        write(root, "src/bin/visible.rs", "fn main() {}");
        write(root, "src/bin/hidden.rs", "fn main() {}");
        let db = Database::open_in_memory().unwrap();
        map_features(root, &db, &["**/hidden.rs".to_string()]).unwrap();
        let features = db.list_features(None, None, None, 100).unwrap();
        let entries: Vec<&str> = features.iter().map(|f| f.entry_path.as_str()).collect();
        assert!(
            entries.iter().any(|e| e == &"src/bin/visible.rs"),
            "expected visible bin feature, got {entries:?}"
        );
        assert!(
            !entries.iter().any(|e| e == &"src/bin/hidden.rs"),
            "hidden.rs was emitted as a feature despite **/hidden.rs exclude, got {entries:?}"
        );
    }

    #[test]
    fn rust_integration_test_under_exclude_is_dropped() {
        // Integration tests under `tests/` must respect `**/tests/**`
        // excludes.
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
        );
        write(root, "src/main.rs", "fn main() {}");
        write(root, "tests/integration.rs", "#[test] fn t() {}");
        let db = Database::open_in_memory().unwrap();
        map_features(root, &db, &["**/tests/**".to_string()]).unwrap();
        let features = db.list_features(None, None, None, 100).unwrap();
        assert!(
            !features.iter().any(|f| f.entry_path.starts_with("tests/")),
            "tests/** exclude not applied to integration tests, got {features:?}"
        );
    }

    #[test]
    fn cmake_target_under_exclude_is_dropped() {
        // Regression: a CMake target whose entry resolves to an excluded
        // path must not emit a feature.
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "CMakeLists.txt",
            "add_executable(visible visible.c)\nadd_executable(hidden hidden.c)\n",
        );
        write(root, "visible.c", "int main(){return 0;}\n");
        write(root, "hidden.c", "int main(){return 0;}\n");
        let db = Database::open_in_memory().unwrap();
        map_features(root, &db, &["**/hidden.c".to_string()]).unwrap();
        let features = db.list_features(None, None, None, 100).unwrap();
        let entries: Vec<&str> = features.iter().map(|f| f.entry_path.as_str()).collect();
        assert!(
            entries.iter().any(|e| e == &"visible.c"),
            "expected visible.c feature, got {entries:?}"
        );
        assert!(
            !entries.iter().any(|e| e == &"hidden.c"),
            "hidden.c was emitted despite **/hidden.c exclude, got {entries:?}"
        );
    }

    #[test]
    fn cmake_owned_files_under_exclude_are_dropped() {
        // Even if the target's entry is allowed, sources listed under
        // `add_executable(target src1 src2)` that are themselves excluded
        // must not show up as owned/context refs on the emitted feature.
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "CMakeLists.txt",
            "add_executable(svc main.c helper_generated.c)\n",
        );
        write(root, "main.c", "int main(){return 0;}\n");
        write(root, "helper_generated.c", "void h(){}\n");
        let db = Database::open_in_memory().unwrap();
        map_features(root, &db, &["**/*_generated.c".to_string()]).unwrap();
        let features = db.list_features(None, None, None, 100).unwrap();
        let svc = features
            .iter()
            .find(|f| f.entry_path == "main.c")
            .expect("expected main.c feature");
        let owned_paths: Vec<&str> = svc.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            !owned_paths.iter().any(|p| p == &"helper_generated.c"),
            "generated source leaked as owned file despite exclude, got {owned_paths:?}"
        );
    }

    #[test]
    fn removed_seed_drops_feature_on_remap() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
        );
        write(root, "src/main.rs", "fn main() {}");
        write(root, "src/bin/extra.rs", "fn main() {}");
        let db = Database::open_in_memory().unwrap();
        map_features(root, &db, &[]).unwrap();
        let before = db.feature_count().unwrap();
        std::fs::remove_file(root.join("src/bin/extra.rs")).unwrap();
        let stats = map_features(root, &db, &[]).unwrap();
        assert!(stats.removed >= 1, "expected ≥1 removal, got {stats:?}");
        let after = db.feature_count().unwrap();
        assert!(after < before);
    }

    #[test]
    fn feature_id_stable_when_test_command_changes() {
        // Regression: feature_id hashes (kind, source, entry_path,
        // command|route|symbol). The test command (e.g. `pnpm --dir api
        // test`) must NOT contribute — projects edit their test script /
        // package manager often, and cross-session IDs should survive
        // that. Verifies by changing the package's lock file (npm →
        // pnpm) and asserting the feature_id is unchanged.
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "package.json",
            r#"{"name":"monorepo","workspaces":["packages/*"]}"#,
        );
        write(
            root,
            "packages/api/package.json",
            r#"{"name":"@acme/api","scripts":{"test":"jest"}}"#,
        );
        write(root, "packages/api/src/index.ts", "export const x = 1;\n");

        let db = Database::open_in_memory().unwrap();
        map_features(root, &db, &[]).unwrap();
        let before = db
            .features_for_file("packages/api/src/index.ts")
            .unwrap()
            .into_iter()
            .find(|f| f.source == "node-package")
            .expect("api package feature before lock swap");

        // Add a pnpm-lock.yaml so the detected package manager flips
        // from npm → pnpm, changing the inferred test command.
        write(root, "pnpm-lock.yaml", "lockfileVersion: '9'\n");
        map_features(root, &db, &[]).unwrap();
        let after = db
            .features_for_file("packages/api/src/index.ts")
            .unwrap()
            .into_iter()
            .find(|f| f.source == "node-package")
            .expect("api package feature after lock swap");

        assert_eq!(
            before.feature_id, after.feature_id,
            "feature_id must be stable when test_command changes"
        );
        assert_ne!(
            before.test_command, after.test_command,
            "sanity: test_command actually changed across the swap"
        );
        assert!(
            after.entry_command.is_none(),
            "library feature must not put the test command in entry_command"
        );
        assert_eq!(
            after.test_command.as_deref(),
            Some("pnpm --dir packages/api test")
        );
    }
}
