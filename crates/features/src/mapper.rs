//! Mapping orchestration: run every mapper, build [`FeatureRecord`]s from
//! seeds, attach nearby tests and trust boundaries, persist into the
//! `features` / `feature_files` / `feature_trust_boundaries` tables, and
//! garbage-collect features whose seed disappeared.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use codesage_protocol::{
    FeatureFileRef, FeatureFileRole, FeatureMapStats, FeatureRecord, TrustBoundary,
};
use codesage_storage::Database;

use crate::feature_id;
use crate::mappers::{
    c::CCppMapper, go::GoMapper, js::JsMapper, php::PhpMapper, python::PythonMapper,
    rust::RustMapper, shared::walk_files, types::FeatureMapper, types::FeatureSeed,
};
use crate::nearby_tests::nearby_tests;

/// Run every registered mapper, build `FeatureRecord`s from seeds, persist
/// them, and remove stale features. Returns stats for the caller.
///
/// When **any** mapper errors mid-collection, the orchestrator still
/// persists the seeds it did collect but **skips the garbage-collect
/// pass**. The earlier "warn-and-continue + GC anything not in
/// keep_ids" path could silently delete every PHP feature when the PHP
/// mapper happened to fail on a corrupt composer.json (CR-004). Skipping
/// GC trades a small amount of stale-feature debt for not nuking valid
/// rows; the next clean run reconciles.
pub fn map_features(root: &Path, db: &Database) -> Result<FeatureMapStats> {
    let collected = collect_seeds(root)?;
    let seeds = collected.seeds;
    let any_mapper_errored = collected.any_errored;
    let mut keep_ids: Vec<String> = Vec::with_capacity(seeds.len());
    let mut created = 0usize;
    let mut updated = 0usize;
    // Snapshot of repo files for the nearby-test walker. Capped to keep
    // big repos under a couple seconds of wall-time.
    let all_files = walk_files(root, root, 50_000);
    db.execute_batch(|db| {
        for seed in &seeds {
            let record = build_record(db, root, seed, &all_files)?;
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
fn collect_seeds(root: &Path) -> Result<CollectedSeeds> {
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
        match m.map(root) {
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
        let disc = s
            .entry_command
            .clone()
            .or_else(|| s.entry_route.clone())
            .or_else(|| s.entry_symbol.clone())
            .unwrap_or_default();
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

fn build_record(
    db: &Database,
    root: &Path,
    seed: &FeatureSeed,
    all_files: &[String],
) -> Result<FeatureRecord> {
    let disc = seed
        .entry_command
        .clone()
        .or_else(|| seed.entry_route.clone())
        .or_else(|| seed.entry_symbol.clone())
        .unwrap_or_default();
    let feature_id = feature_id::build(seed.kind, seed.source, &seed.entry_path, &disc);

    // Build the file ref set: entry + owned + context + (seed tests union
    // nearby tests). Deduped by (path, role); entry always wins over owned.
    let mut files_by_key: BTreeMap<(String, FeatureFileRole), FeatureFileRef> = BTreeMap::new();
    let entry_ref = FeatureFileRef {
        path: seed.entry_path.clone(),
        role: FeatureFileRole::Entry,
        reason: Some("entrypoint".to_string()),
    };
    files_by_key.insert((seed.entry_path.clone(), FeatureFileRole::Entry), entry_ref);
    for f in &seed.owned_files {
        files_by_key
            .entry((f.path.clone(), FeatureFileRole::Owned))
            .or_insert(FeatureFileRef {
                path: f.path.clone(),
                role: FeatureFileRole::Owned,
                reason: Some(f.reason.clone()),
            });
    }
    for f in &seed.context_files {
        files_by_key
            .entry((f.path.clone(), FeatureFileRole::Context))
            .or_insert(FeatureFileRef {
                path: f.path.clone(),
                role: FeatureFileRole::Context,
                reason: Some(f.reason.clone()),
            });
    }
    // Seed-attached tests.
    for t in &seed.tests {
        files_by_key
            .entry((t.path.clone(), FeatureFileRole::Test))
            .or_insert(FeatureFileRef {
                path: t.path.clone(),
                role: FeatureFileRole::Test,
                reason: Some("seed test".to_string()),
            });
    }
    // Nearby-test discovery from filesystem conventions.
    let nearby = nearby_tests(seed, all_files);
    for path in nearby {
        files_by_key
            .entry((path.clone(), FeatureFileRole::Test))
            .or_insert(FeatureFileRef {
                path,
                role: FeatureFileRole::Test,
                reason: Some("convention".to_string()),
            });
    }
    let files: Vec<FeatureFileRef> = files_by_key.into_values().collect();
    // Aggregate trust boundaries across owned + entry files (skip context
    // because context tends to be huge generic helpers and would dilute).
    let mut tb: BTreeSet<TrustBoundary> = BTreeSet::new();
    let boundary_inputs: Vec<&str> = files
        .iter()
        .filter(|f| matches!(f.role, FeatureFileRole::Entry | FeatureFileRole::Owned))
        .map(|f| f.path.as_str())
        .collect();
    for p in boundary_inputs {
        let _ = root; // root unused after walking files; keeps the api uniform
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
        let stats = map_features(root, &db).unwrap();
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
        let first = map_features(root, &db).unwrap();
        let ids_before: Vec<String> = db
            .list_features(None, None, None, 1000)
            .unwrap()
            .into_iter()
            .map(|f| f.feature_id)
            .collect();
        let second = map_features(root, &db).unwrap();
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
        map_features(root, &db).unwrap();
        let before = db.feature_count().unwrap();
        std::fs::remove_file(root.join("src/bin/extra.rs")).unwrap();
        let stats = map_features(root, &db).unwrap();
        assert!(stats.removed >= 1, "expected ≥1 removal, got {stats:?}");
        let after = db.feature_count().unwrap();
        assert!(after < before);
    }
}
