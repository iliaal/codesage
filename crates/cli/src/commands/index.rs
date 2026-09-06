//! `index` / `map` / `cleanup` / `status`: the indexing pipeline and its
//! read-side status reporting.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use codesage_embed::config::EmbeddingConfig;
use codesage_embed::model::Embedder;
use codesage_graph::{
    ArtifactLookup, LazyEmbedder, SemanticFingerprint, SemanticTableState, TextEmbedder,
    full_index, incremental_index, resolve_semantic_fingerprint, semantic_full_index,
    semantic_incremental_index, semantic_table_state,
};
use codesage_parser::discover::build_exclude_set;
use codesage_storage::Database;

use crate::util::format_bytes;
use crate::{
    DB_FILE, PROJECT_DIR, acquire_index_lock, db_path, find_project_root, get_exclude_patterns,
    load_project_config, open_context_db_for_existing_model, open_db, open_db_for_model,
    open_db_for_model_rebuild,
};

/// FNV-1a fold shared by the structural and manifest fingerprint passes.
/// Stable across processes and binary rebuilds, unlike `DefaultHasher`,
/// because the value is persisted in `.codesage/feature-map.state` and
/// compared on later runs.
fn fnv1a(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

const FNV1A_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// Order-independent fingerprint of the structural file set (path + content
/// hash per file), FNV-1a over the sorted pairs.
fn structural_state_fingerprint(hashes: &std::collections::HashMap<String, String>) -> u64 {
    let mut entries: Vec<(&String, &String)> = hashes.iter().collect();
    entries.sort();
    let mut h = FNV1A_SEED;
    for (path, content_hash) in entries {
        h = fnv1a(h, path.as_bytes());
        h = fnv1a(h, &[0]);
        h = fnv1a(h, content_hash.as_bytes());
        h = fnv1a(h, &[0]);
    }
    h
}

/// Build-manifest basenames the feature mappers read directly. Most are not
/// source files, so the structural indexer never hashes them and
/// `all_file_hashes` alone would miss a manifest-only edit (e.g. adding a
/// `bin` entry to package.json). Laravel `routes/*.php`, Next.js route files,
/// and `setup.py` are also structurally indexed, but membership here is
/// harmless — the stat fold just covers them twice.
const MAPPER_MANIFEST_BASENAMES: &[&str] = &[
    "CMakeLists.txt",
    "Cargo.toml",
    "Makefile.am",
    "composer.json",
    "config.m4",
    "config.w32",
    "configure.ac",
    "go.mod",
    "package.json",
    "pyproject.toml",
    "setup.py",
];

/// Stat every mapper-input manifest under `root` (honoring the same exclude
/// globs as the mapper) and fold path + mtime + size into `seed`.
///
/// Stat-based (mtime + size) rather than content-hashed on purpose: this runs
/// on every no-op incremental pass, and reading every manifest would erode
/// the very cost the skip gate exists to avoid. The trade-off is that a
/// same-size rewrite within the mtime granularity can go unnoticed until the
/// next real change — rare enough to accept for a cache-invalidation marker.
fn manifest_state_fingerprint(seed: u64, root: &Path, exclude_patterns: &[String]) -> u64 {
    let excludes = if exclude_patterns.is_empty() {
        None
    } else {
        build_exclude_set(exclude_patterns).ok()
    };

    let mut entries: Vec<(String, i64, u32, u64)> = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder.hidden(true).git_ignore(true);
    if let Some(excludes) = excludes {
        let root_for_filter = root.to_path_buf();
        builder.filter_entry(move |entry| {
            let Ok(rel) = entry.path().strip_prefix(&root_for_filter) else {
                return true;
            };
            if rel.as_os_str().is_empty() {
                return true;
            }
            let rel_path = rel.to_string_lossy();
            if excludes.is_match(rel_path.as_ref()) {
                return false;
            }
            // Directory patterns like `**/node_modules/**` match contents,
            // not the bare dir path; probe with the same suffix trick the
            // structural discovery filter uses so pruning stays consistent.
            if entry.file_type().is_some_and(|ft| ft.is_dir())
                && (excludes.is_match(format!("{rel_path}/"))
                    || excludes.is_match(format!("{rel_path}/_")))
            {
                return false;
            }
            true
        });
    }
    for entry in builder.build().flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if !MAPPER_MANIFEST_BASENAMES.contains(&name.as_ref()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let (mtime_secs, mtime_nanos) = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs() as i64, d.subsec_nanos()))
            .unwrap_or((0, 0));
        entries.push((
            rel.to_string_lossy().into_owned(),
            mtime_secs,
            mtime_nanos,
            meta.len(),
        ));
    }
    entries.sort();

    let mut h = seed;
    for (path, secs, nanos, size) in entries {
        h = fnv1a(h, path.as_bytes());
        h = fnv1a(h, &secs.to_le_bytes());
        h = fnv1a(h, &nanos.to_le_bytes());
        h = fnv1a(h, &size.to_le_bytes());
        h = fnv1a(h, &[0]);
    }
    h
}

/// Fingerprint of everything feature mapping consumes: the structural
/// file-hash set plus the build manifests the mappers read directly. `None`
/// when the DB can't produce the file-hash set (mapping then proceeds
/// unconditionally — the safe direction).
fn feature_map_fingerprint(db: &Database, root: &Path, exclude_patterns: &[String]) -> Option<u64> {
    let hashes = match db.all_file_hashes() {
        Ok(hashes) => hashes,
        Err(e) => {
            tracing::debug!(error = %e, "file-hash fingerprint unavailable; mapping features");
            return None;
        }
    };
    Some(manifest_state_fingerprint(
        structural_state_fingerprint(&hashes),
        root,
        exclude_patterns,
    ))
}

fn feature_map_state_path(root: &Path) -> PathBuf {
    root.join(PROJECT_DIR).join("feature-map.state")
}

fn read_feature_map_state(root: &Path) -> Option<u64> {
    crate::fsguard::read_state_to_string(&feature_map_state_path(root))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn write_feature_map_state(root: &Path, fingerprint: u64) {
    // `.codesage/feature-map.state` is repository-supplied like the rest of the
    // directory, so a planted symlink here would turn this marker write into an
    // arbitrary-path truncate on the documented `codesage index` path.
    let write = |path: &Path| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut f = crate::fsguard::create_no_follow(path)?;
        writeln!(f, "{fingerprint}")
    };
    if let Err(e) = write(&feature_map_state_path(root)) {
        tracing::warn!(error = %e, "failed to record feature-map state");
    }
}

/// Record the skip marker after a mapping run — but only when the run was
/// clean. A partial run (a mapper errored mid-collection) persisted what it
/// could and skipped garbage collection; stamping the marker would suppress
/// the retry that reconciles the debt on the next pass.
fn record_feature_map_state(
    root: &Path,
    db: &Database,
    exclude_patterns: &[String],
    mapper_errors: &[String],
) {
    if mapper_errors.is_empty() {
        if let Some(fp) = feature_map_fingerprint(db, root, exclude_patterns) {
            write_feature_map_state(root, fp);
        }
        return;
    }
    for err in mapper_errors {
        tracing::warn!(error = %err, "feature mapper failed; skip marker not advanced");
    }
}

/// An incremental `codesage index` may skip feature mapping only when this
/// process changed nothing (no files parsed or removed, no trust-boundary
/// backfill) AND the mapper-input fingerprint matches the one recorded at
/// the last successful map. The fingerprint check is what keeps the skip
/// safe against the daemon watcher, which advances structural state in the
/// same DB without ever running feature mapping. `current_fingerprint` is a
/// closure so the fingerprint (a DB scan plus a manifest walk) is only
/// computed once the cheap conditions already hold.
fn can_skip_feature_mapping(
    full: bool,
    files_indexed: usize,
    files_removed: usize,
    boundaries_backfilled: usize,
    last_mapped_fingerprint: Option<u64>,
    current_fingerprint: impl FnOnce() -> Option<u64>,
) -> bool {
    if full || files_indexed != 0 || files_removed != 0 || boundaries_backfilled != 0 {
        return false;
    }
    let Some(last) = last_mapped_fingerprint else {
        return false;
    };
    current_fingerprint().is_some_and(|cur| cur == last)
}

/// The fingerprint an index pass compares and attests against, resolved
/// through the table's recorded attestation so a no-change pass reads no
/// model file. Downloads on a cache miss, as the loader it stands in for.
pub(crate) fn resolved_fingerprint(
    db: &Database,
    emb_config: &EmbeddingConfig,
    dim: usize,
) -> Result<SemanticFingerprint> {
    resolve_semantic_fingerprint(db, emb_config, dim, ArtifactLookup::Resolve)?.ok_or_else(|| {
        anyhow::anyhow!(
            "model files for {:?} could not be resolved",
            emb_config.model
        )
    })
}

/// Dimension the existing index already records for `model`, or `None` when
/// this model has never indexed the project (or the lookup itself fails, in
/// which case the eager path reports the same error the old code did).
fn recorded_semantic_dim(root: &Path, model: &str) -> Option<usize> {
    let db = open_context_db_for_existing_model(root, model).ok()?;
    db.recorded_semantic_dim().ok().flatten()
}

/// Open the index for `emb_config.model` and produce the embedder the
/// semantic pass will use.
///
/// On an incremental run against a model that already has a recorded table,
/// the dimension comes from that record and the embedder is a
/// [`LazyEmbedder`]: no ONNX session, no CUDA context, until the semantic
/// pass has computed a non-empty file set. A full rebuild, or a model with no
/// recorded table yet, needs the model for its dimension and builds it
/// eagerly — it is about to embed every file anyway.
fn open_index_db_and_embedder(
    root: &Path,
    full: bool,
    emb_config: &EmbeddingConfig,
) -> Result<(Database, Box<dyn TextEmbedder>, SemanticFingerprint)> {
    // Surface a bad batch size now, as the eager constructor always did,
    // rather than only on the first run that has something to embed.
    emb_config.effective_batch_size()?;

    let recorded_dim = if full {
        None
    } else {
        recorded_semantic_dim(root, &emb_config.model)
    };

    let Some(dim) = recorded_dim else {
        let (embedder, dim) = match daemon_embedder(root, emb_config) {
            Some(pair) => pair,
            None => {
                let embedder = Embedder::new(emb_config)?;
                let dim = embedder.dim();
                (Box::new(embedder) as Box<dyn TextEmbedder>, dim)
            }
        };
        let db = if full {
            open_db_for_model_rebuild(root, &emb_config.model, dim)?
        } else {
            open_db_for_model(root, &emb_config.model, dim)?
        };
        let fingerprint = resolved_fingerprint(&db, emb_config, dim)?;
        return Ok((db, embedder, fingerprint));
    };

    let db = open_db_for_model(root, &emb_config.model, dim)?;
    let fingerprint = resolved_fingerprint(&db, emb_config, dim)?;
    let init_config = emb_config.clone();
    let root = root.to_path_buf();
    let lazy = LazyEmbedder::new(Box::new(move |_files_to_embed| {
        let (embedder, produced) = match daemon_embedder(&root, &init_config) {
            Some(pair) => pair,
            None => {
                let embedder = Embedder::new(&init_config)?;
                let dim = embedder.dim();
                (Box::new(embedder) as Box<dyn TextEmbedder>, dim)
            }
        };
        ensure!(
            produced == dim,
            "the embedder produces {produced}-dimensional vectors but the index records {dim} \
             for this model; run `codesage index --full` to rebuild the table"
        );
        Ok(embedder)
    }));
    Ok((db, Box::new(lazy), fingerprint))
}

/// The running daemon's resident session for `model`, with its dimension,
/// when a daemon spawned from this binary answers. `None` means embed
/// privately; the refusal has already been logged.
fn daemon_embedder(
    root: &Path,
    emb_config: &EmbeddingConfig,
) -> Option<(Box<dyn TextEmbedder>, usize)> {
    #[cfg(unix)]
    {
        let daemon = crate::daemon_embed::DaemonEmbedder::connect(root, emb_config)?;
        let dim = daemon.dim();
        Some((Box::new(daemon), dim))
    }
    #[cfg(not(unix))]
    {
        let _ = (root, emb_config);
        None
    }
}

pub(crate) fn cmd_index(
    full: bool,
    no_semantic: bool,
    no_features: bool,
    verbose: bool,
    batch_size_override: Option<NonZeroUsize>,
    lock_wait: Duration,
) -> Result<()> {
    let root = find_project_root()?;
    // Acquire the project-level indexing lock before loading embedders or
    // touching the DB. `--lock-wait` bounds a polling wait first: the
    // daemon's watcher debounce-indexes around commit time but never runs
    // feature mapping, so a skip here would leave feature slices stale.
    // Contention past the window exits EXIT_LOCK_HELD: nothing was indexed,
    // and the installed hook records its skip stamp only on exit 0.
    let _lock = acquire_index_lock(&root, "skipping", lock_wait)?;
    let config = load_project_config(&root)?;
    let excludes = get_exclude_patterns(&config);

    let mut emb_config = config.embedding.unwrap_or_default();
    if let Some(n) = batch_size_override {
        emb_config.set_batch_size_override(n);
    }

    if verbose {
        let batch_size = emb_config.effective_batch_size()?;
        tracing::info!(
            project = root.display().to_string(),
            full,
            no_semantic,
            model = %emb_config.model,
            device = %emb_config.device,
            batch_size = batch_size.get(),
            "starting index"
        );
    }

    let (db, mut embedder) = if no_semantic {
        (open_db(&root)?, None)
    } else {
        let (db, embedder, fingerprint) = open_index_db_and_embedder(&root, full, &emb_config)?;
        (db, Some((embedder, fingerprint)))
    };

    let stats = if full {
        full_index(&root, &db, &excludes, verbose)?
    } else {
        incremental_index(&root, &db, &excludes, verbose)?
    };

    if verbose {
        tracing::info!(
            files_indexed = stats.files_indexed,
            files_skipped = stats.files_skipped,
            files_failed = stats.files_failed,
            failed_paths = %codesage_graph::summarize_paths(&stats.failed_paths, 10),
            files_degraded = stats.files_degraded,
            files_removed = stats.files_removed,
            symbols = stats.symbols_found,
            refs = stats.references_found,
            "structural index complete"
        );
    } else {
        let mut line = format!(
            "Structural: {} files ({} skipped, {} failed, {} removed), {} symbols, {} references",
            stats.files_indexed,
            stats.files_skipped,
            stats.files_failed,
            stats.files_removed,
            stats.symbols_found,
            stats.references_found
        );
        if stats.files_degraded > 0 {
            line.push_str(&format!(
                ", {} parsed with syntax errors",
                stats.files_degraded
            ));
        }
        println!("{line}");
        if !stats.failed_paths.is_empty() {
            println!(
                "  failed (retried next pass): {}",
                codesage_graph::summarize_paths(&stats.failed_paths, 10)
            );
        }
    }

    // Targeted trust-boundary backfill. `files_pending_boundary_derivation`
    // returns files that have never been derived (or were indexed before
    // the marker column existed); the structural indexer only derives
    // inline for files it parses this pass, so the catch-up here picks
    // up the rest without reprocessing rule-clean files.
    let mut boundaries_backfilled = 0usize;
    match db.files_pending_boundary_derivation() {
        Ok(pending) if !pending.is_empty() => {
            let n_pending = pending.len();
            if verbose {
                tracing::info!(pending = n_pending, "backfilling trust boundaries");
            }
            match codesage_features::derive_for_files(&db, &pending) {
                Ok(n) => {
                    boundaries_backfilled = n;
                    if verbose {
                        tracing::info!(
                            backfilled = n,
                            total = n_pending,
                            "trust boundaries complete"
                        );
                    } else {
                        println!("Trust boundaries: backfilled {n}/{n_pending} pending files");
                    }
                }
                Err(e) => {
                    return Err(e.context("trust-boundary backfill failed during codesage index"));
                }
            }
        }
        Ok(_) => {}
        Err(e) => {
            return Err(e.context("trust-boundary pending-list query failed during codesage index"));
        }
    }

    // Feature mapping runs after structural (which populated `refs` and
    // `file_trust_boundaries`) and before semantic so per-feature
    // trust-boundary tags are fresh. Errors here are fatal: a mid-run
    // failure must surface as command failure, not a silent eprintln
    // with feature tables left in a partial state.
    //
    // An incremental pass may skip mapping, but only when the mapper-input
    // state (file hashes + build manifests) is byte-identical to what the
    // last successful map saw. This process's own stats are not enough: the
    // daemon's watcher indexes structurally around commit time without ever
    // mapping features, so a hook-invoked pass can report 0 parsed files
    // while the DB did change.
    if no_features {
        if verbose {
            tracing::info!("feature mapping skipped (--no-features)");
        } else {
            println!("Features:   skipped (--no-features)");
        }
    } else {
        let skip = can_skip_feature_mapping(
            full,
            stats.files_indexed,
            stats.files_removed,
            boundaries_backfilled,
            read_feature_map_state(&root),
            || feature_map_fingerprint(&db, &root, &excludes),
        );
        if skip {
            if verbose {
                tracing::info!("feature mapping skipped (no structural changes since last map)");
            } else {
                println!("Features:   unchanged (no structural changes since last map)");
            }
        } else {
            if verbose {
                tracing::info!("mapping features");
            }
            match codesage_features::map_features_detailed(&root, &db, &excludes) {
                Ok(outcome) => {
                    record_feature_map_state(&root, &db, &excludes, &outcome.mapper_errors);
                    let map_stats = outcome.stats;
                    if verbose {
                        tracing::info!(
                            created = map_stats.created,
                            updated = map_stats.updated,
                            removed = map_stats.removed,
                            total = map_stats.total_features,
                            "feature mapping complete"
                        );
                    } else {
                        println!(
                            "Features:   created={} updated={} removed={} total={}",
                            map_stats.created,
                            map_stats.updated,
                            map_stats.removed,
                            map_stats.total_features
                        );
                    }
                }
                Err(e) => return Err(e.context("feature mapping failed during `codesage index`")),
            }
        }
    }

    if let Some((embedder, fingerprint)) = embedder.as_mut() {
        let sem_stats = if full {
            semantic_full_index(
                &root,
                &db,
                embedder.as_mut(),
                &excludes,
                fingerprint,
                verbose,
            )?
        } else {
            semantic_incremental_index(
                &root,
                &db,
                embedder.as_mut(),
                &excludes,
                fingerprint,
                verbose,
            )?
        };
        if verbose {
            tracing::info!(
                files_processed = sem_stats.files_processed,
                files_skipped = sem_stats.files_skipped,
                files_failed = sem_stats.files_failed,
                failed_paths = %codesage_graph::summarize_paths(&sem_stats.failed_paths, 10),
                files_removed = sem_stats.files_removed,
                chunks = sem_stats.chunks_created,
                "semantic index complete"
            );
        } else {
            println!(
                "Semantic: {} files ({} skipped, {} failed, {} removed), {} chunks",
                sem_stats.files_processed,
                sem_stats.files_skipped,
                sem_stats.files_failed,
                sem_stats.files_removed,
                sem_stats.chunks_created
            );
            if !sem_stats.failed_paths.is_empty() {
                println!(
                    "  failed to read (retried next pass): {}",
                    codesage_graph::summarize_paths(&sem_stats.failed_paths, 10)
                );
            }
        }
    }

    // Stamp the HEAD SHA we just indexed against. Skipped in non-git dirs.
    // Failures here only degrade drift telemetry, so they warn rather than
    // propagate — the index itself is already durable on disk.
    if let Some(sha) = codesage_graph::drift::git_head_sha(&root)
        && let Err(e) = db.set_structural_index_state(&sha)
    {
        tracing::warn!(error = %e, "failed to stamp structural_index_state");
    }

    Ok(())
}

pub(crate) fn cmd_map(json: bool) -> Result<()> {
    let root = find_project_root()?;
    // `map_features` writes the feature tables in multiple transactions and runs
    // a GC pass; take the same project writer lock that cmd_index / cmd_git_index
    // / cmd_cleanup hold so a manual `codesage map` doesn't race the background
    // hook-driven indexer (which maps features itself) into SQLITE_BUSY or a
    // partial multi-transaction state. Skip if an indexer already holds it.
    let _lock = acquire_index_lock(&root, "skipping map", Duration::ZERO)?;
    let db = open_db(&root)?;
    let config = load_project_config(&root)?;
    let excludes = get_exclude_patterns(&config);
    let outcome = codesage_features::map_features_detailed(&root, &db, &excludes)?;
    record_feature_map_state(&root, &db, &excludes, &outcome.mapper_errors);
    for err in &outcome.mapper_errors {
        eprintln!("warning: feature mapper failed: {err}");
    }
    let stats = outcome.stats;
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!(
            "Features mapped: created={} updated={} removed={} total={}",
            stats.created, stats.updated, stats.removed, stats.total_features
        );
    }
    Ok(())
}

/// JSON envelope for `codesage status --json`. Composes existing serializable
/// state (the drift report serializes as-is) with the same counts the prose
/// output prints; `semantic` mirrors the prose semantic line because storage's
/// `SemanticFreshness` does not serialize.
#[derive(serde::Serialize)]
struct StatusReport {
    project_root: String,
    database: String,
    files: usize,
    symbols: usize,
    references: usize,
    chunks: usize,
    drift: codesage_graph::drift::DriftReport,
    drift_summary: String,
    semantic: SemanticStatus,
}

#[derive(serde::Serialize)]
struct SemanticStatus {
    model: String,
    /// One of `fresh`, `stale`, `missing` (no chunk table for the configured
    /// model), or `unavailable` (freshness query returned nothing, or the
    /// current fingerprint could not be derived). `fresh` requires every
    /// file's hash to match AND the table's fingerprint to be current.
    state: &'static str,
    /// How the table's recorded fingerprint relates to the configured setup:
    /// `current`, `unrecorded`, `mismatch`, `unknown` (the model files are
    /// not in the local cache; `status` never downloads them), `unresolved`
    /// (the model files could not be digested), or `absent` (no table).
    fingerprint: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    indexed_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stale_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    missing_files: Option<usize>,
}

/// The `state` label: file hashes alone cannot make a table fresh when its
/// vectors were produced under an unknown or different setup.
fn semantic_state_label(files_fresh: bool, table: &SemanticTableState) -> &'static str {
    if files_fresh && table.is_current() {
        "fresh"
    } else {
        "stale"
    }
}

fn fingerprint_label(table: &SemanticTableState) -> &'static str {
    match table {
        SemanticTableState::Current => "current",
        SemanticTableState::Unrecorded => "unrecorded",
        SemanticTableState::Mismatch { .. } => "mismatch",
    }
}

fn semantic_status(root: &Path) -> Result<SemanticStatus> {
    let config = load_project_config(root)?;
    let emb_config = config.embedding.unwrap_or_default();
    let model = emb_config.model.clone();
    let db = open_context_db_for_existing_model(root, &model)?;
    if db.chunk_table_name().is_empty() {
        return Ok(SemanticStatus {
            model,
            state: "missing",
            fingerprint: "absent",
            indexed_files: None,
            stale_files: None,
            missing_files: None,
        });
    }
    let unavailable = |model: String, fingerprint: &'static str| SemanticStatus {
        model,
        state: "unavailable",
        fingerprint,
        indexed_files: None,
        stale_files: None,
        missing_files: None,
    };
    let Some(freshness) = db.semantic_freshness()? else {
        return Ok(unavailable(model, "unresolved"));
    };
    // The recorded dimension names the table; the fingerprint needs the
    // model files on disk. Either missing leaves freshness undecidable,
    // which is never reported as fresh. A status line never downloads a
    // model: files absent from the local cache are `unknown`.
    let Some(dim) = db.recorded_semantic_dim()? else {
        return Ok(unavailable(model, "unresolved"));
    };
    let current =
        match resolve_semantic_fingerprint(&db, &emb_config, dim, ArtifactLookup::CachedOnly) {
            Ok(Some(fingerprint)) => fingerprint,
            Ok(None) => return Ok(unavailable(model, "unknown")),
            Err(e) => {
                eprintln!("warning: semantic fingerprint could not be derived: {e:#}");
                return Ok(unavailable(model, "unresolved"));
            }
        };
    let table = semantic_table_state(&db, &current)?;
    Ok(SemanticStatus {
        model,
        state: semantic_state_label(freshness.is_fresh(), &table),
        fingerprint: fingerprint_label(&table),
        indexed_files: Some(freshness.indexed_files),
        stale_files: Some(freshness.stale_files),
        missing_files: Some(freshness.missing_files),
    })
}

fn print_semantic_status(s: &SemanticStatus) {
    match s.state {
        "missing" => println!(
            "Semantic:   missing for model {} (run `codesage index`)",
            s.model
        ),
        "unavailable" => println!(
            "Semantic:   unavailable for model {} (fingerprint {})",
            s.model, s.fingerprint
        ),
        "fresh" => println!(
            "Semantic:   fresh for model {} ({} files)",
            s.model,
            s.indexed_files.unwrap_or(0)
        ),
        _ if s.fingerprint != "current" => println!(
            "Semantic:   stale for model {} (fingerprint {}; run `codesage index --full`)",
            s.model, s.fingerprint
        ),
        _ => println!(
            "Semantic:   stale for model {} ({} stale, {} missing; run `codesage index`)",
            s.model,
            s.stale_files.unwrap_or(0),
            s.missing_files.unwrap_or(0)
        ),
    }
}

pub(crate) fn cmd_status(json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;

    let drift = codesage_graph::drift::check_drift(&root, &db);
    let report = StatusReport {
        project_root: root.display().to_string(),
        database: db_path(&root).display().to_string(),
        files: db.file_count()?,
        symbols: db.symbol_count()?,
        references: db.reference_count()?,
        chunks: db.total_chunk_count()?,
        drift_summary: drift.summary(),
        drift,
        semantic: semantic_status(&root)?,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("Project root: {}", report.project_root);
    println!("Database: {}", report.database);
    println!("Files:      {}", report.files);
    println!("Symbols:    {}", report.symbols);
    println!("References: {}", report.references);
    println!("Chunks:     {}", report.chunks);
    println!("Drift:      {}", report.drift_summary);
    print_semantic_status(&report.semantic);
    Ok(())
}

pub(crate) fn cmd_cleanup(dry_run: bool) -> Result<()> {
    let root = find_project_root()?;
    // Cleanup drops orphan vec tables (from prior model switches) — also
    // a writer-style operation that races with in-flight indexers. Same
    // lock coordination.
    let _lock = acquire_index_lock(&root, "skipping cleanup", Duration::ZERO)?;
    let config = load_project_config(&root)?;
    let emb_config = config.embedding.unwrap_or_default();

    let db = open_context_db_for_existing_model(&root, &emb_config.model)?;
    let active_table = db.chunk_table_name().to_string();

    let db_path = root.join(PROJECT_DIR).join(DB_FILE);
    let size_before = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    let tables = db.list_vec_tables()?;
    if active_table.is_empty() && !tables.is_empty() {
        bail!(
            "no active semantic table for model {}; run `codesage index` for this model before \
             dropping orphan vec tables (found {} table(s) that would all be treated as orphans)",
            emb_config.model,
            tables.len()
        );
    }
    let mut dropped = 0;
    let mut failed = 0;

    println!("Active model:  {}", emb_config.model);
    println!(
        "Active table:  {}",
        if active_table.is_empty() {
            "(none)"
        } else {
            &active_table
        }
    );
    println!("DB size before: {}", format_bytes(size_before));
    println!();

    for table in &tables {
        if table == &active_table {
            println!("  keep: {table}");
            continue;
        }
        if dry_run {
            println!("  DRY-RUN drop: {table}");
        } else {
            match db.drop_vec_table(table) {
                Ok(()) => println!("  drop: {table}"),
                Err(e) => {
                    tracing::error!(%table, error = %e, "failed to drop orphan vec table");
                    eprintln!("  FAILED to drop: {table} ({e})");
                    failed += 1;
                    continue;
                }
            }
        }
        dropped += 1;
    }

    if dry_run {
        println!("\nWould drop {dropped} tables (dry-run, no changes made)");
        return Ok(());
    }

    if dropped > 0 {
        println!("\nVacuuming...");
        db.vacuum()?;
    }

    let size_after = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let saved = size_before.saturating_sub(size_after);

    println!("DB size after:  {}", format_bytes(size_after));
    println!("Saved:          {}", format_bytes(saved));
    println!("Dropped:        {dropped} tables");
    if failed > 0 {
        println!("Failed:         {failed} tables");
        // Non-zero exit so a scripted auto-clean (e.g. /codesage-reindex) can
        // detect that orphan tables were left behind.
        bail!("failed to drop {failed} orphan vec table(s); see errors above");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- feature-mapping skip gate ----------

    fn fp_of(v: u64) -> impl FnOnce() -> Option<u64> {
        move || Some(v)
    }

    #[test]
    fn feature_mapping_skips_only_on_true_noop_incremental_pass() {
        let fp = 42u64;
        assert!(can_skip_feature_mapping(
            false,
            0,
            0,
            0,
            Some(fp),
            fp_of(fp)
        ));
        // Full index always maps.
        assert!(!can_skip_feature_mapping(
            true,
            0,
            0,
            0,
            Some(fp),
            fp_of(fp)
        ));
        // Any structural change this pass maps.
        assert!(!can_skip_feature_mapping(
            false,
            1,
            0,
            0,
            Some(fp),
            fp_of(fp)
        ));
        assert!(!can_skip_feature_mapping(
            false,
            0,
            1,
            0,
            Some(fp),
            fp_of(fp)
        ));
        // Trust-boundary backfill changes per-feature aggregates.
        assert!(!can_skip_feature_mapping(
            false,
            0,
            0,
            3,
            Some(fp),
            fp_of(fp)
        ));
        // No recorded map state: never skip.
        assert!(!can_skip_feature_mapping(false, 0, 0, 0, None, fp_of(fp)));
        // DB advanced since the last map (e.g. the daemon watcher indexed
        // files without mapping): fingerprints differ, must map.
        assert!(!can_skip_feature_mapping(
            false,
            0,
            0,
            0,
            Some(fp + 1),
            fp_of(fp)
        ));
        // Unavailable fingerprint: never skip.
        assert!(!can_skip_feature_mapping(false, 0, 0, 0, Some(fp), || None));
    }

    #[test]
    fn fingerprint_is_not_computed_when_cheap_conditions_already_fail() {
        // The fingerprint costs a DB scan + a manifest walk; a pass that
        // already indexed files (or has no marker) must not pay it.
        let must_not_run = || -> Option<u64> { panic!("fingerprint must not be computed") };
        assert!(!can_skip_feature_mapping(
            true,
            0,
            0,
            0,
            Some(1),
            must_not_run
        ));
        assert!(!can_skip_feature_mapping(
            false,
            2,
            0,
            0,
            Some(1),
            must_not_run
        ));
        assert!(!can_skip_feature_mapping(
            false,
            0,
            0,
            0,
            None,
            must_not_run
        ));
    }

    #[test]
    fn structural_fingerprint_is_order_independent_and_content_sensitive() {
        let mut a = std::collections::HashMap::new();
        a.insert("src/a.rs".to_string(), "h1".to_string());
        a.insert("src/b.rs".to_string(), "h2".to_string());
        let mut b = std::collections::HashMap::new();
        b.insert("src/b.rs".to_string(), "h2".to_string());
        b.insert("src/a.rs".to_string(), "h1".to_string());
        assert_eq!(
            structural_state_fingerprint(&a),
            structural_state_fingerprint(&b)
        );

        let mut changed = a.clone();
        changed.insert("src/a.rs".to_string(), "h1'".to_string());
        assert_ne!(
            structural_state_fingerprint(&a),
            structural_state_fingerprint(&changed)
        );

        // Path/hash boundary must matter: ("ab","c") != ("a","bc").
        let mut x = std::collections::HashMap::new();
        x.insert("ab".to_string(), "c".to_string());
        let mut y = std::collections::HashMap::new();
        y.insert("a".to_string(), "bc".to_string());
        assert_ne!(
            structural_state_fingerprint(&x),
            structural_state_fingerprint(&y)
        );
    }

    #[test]
    fn manifest_only_change_defeats_feature_map_skip() {
        // package.json is a mapper input but not a structurally indexed
        // source file, so `all_file_hashes` never covers it. Editing only the
        // manifest must still change the combined fingerprint.
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        std::fs::write(dir.path().join("package.json"), "{\"name\":\"a\"}").unwrap();

        let before = feature_map_fingerprint(&db, dir.path(), &[]).unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\"name\":\"a\",\"bin\":{\"x\":\"cli.js\"}}",
        )
        .unwrap();
        let after = feature_map_fingerprint(&db, dir.path(), &[]).unwrap();

        assert_ne!(
            before, after,
            "a manifest-only edit must defeat the feature-map skip"
        );
    }

    #[test]
    fn nested_manifest_change_defeats_feature_map_skip() {
        // Mapper inputs live in subdirectories too (crates/*/Cargo.toml,
        // ext/*/config.m4); the walk must not stop at the root level.
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let nested = dir.path().join("crates/foo");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Cargo.toml"), "[package]\nname=\"foo\"\n").unwrap();

        let before = feature_map_fingerprint(&db, dir.path(), &[]).unwrap();
        std::fs::write(
            nested.join("Cargo.toml"),
            "[package]\nname=\"foo\"\n[[bin]]\nname=\"foo-cli\"\n",
        )
        .unwrap();
        let after = feature_map_fingerprint(&db, dir.path(), &[]).unwrap();

        assert_ne!(before, after);
    }

    #[test]
    fn status_never_reports_fresh_over_a_stale_or_unrecorded_fingerprint() {
        assert_eq!(
            semantic_state_label(true, &SemanticTableState::Current),
            "fresh"
        );
        assert_eq!(
            semantic_state_label(false, &SemanticTableState::Current),
            "stale"
        );
        assert_eq!(
            semantic_state_label(true, &SemanticTableState::Unrecorded),
            "stale",
            "matching file hashes over unattested vectors are not fresh"
        );
        assert_eq!(
            semantic_state_label(
                true,
                &SemanticTableState::Mismatch {
                    stored: "v2;other".into()
                }
            ),
            "stale"
        );
        assert_eq!(
            fingerprint_label(&SemanticTableState::Unrecorded),
            "unrecorded"
        );
        assert_eq!(
            fingerprint_label(&SemanticTableState::Mismatch { stored: "x".into() }),
            "mismatch"
        );
    }

    #[test]
    fn excluded_manifest_does_not_affect_fingerprint() {
        // The mapper honors [index].exclude_patterns; a vendored manifest the
        // mapper never reads must not defeat the skip either.
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let vendored = dir.path().join("node_modules/dep");
        std::fs::create_dir_all(&vendored).unwrap();
        let excludes = vec!["**/node_modules/**".to_string()];
        std::fs::write(vendored.join("package.json"), "{}").unwrap();

        let before = feature_map_fingerprint(&db, dir.path(), &excludes).unwrap();
        std::fs::write(vendored.join("package.json"), "{\"name\":\"changed\"}").unwrap();
        let after = feature_map_fingerprint(&db, dir.path(), &excludes).unwrap();

        assert_eq!(
            before, after,
            "excluded manifests are not mapper inputs and must not churn the fingerprint"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_feature_map_state_refuses_a_symlinked_marker() {
        // Sink-level: reverting `write_feature_map_state` to `fs::write` must
        // turn this red, so it exercises the real function, not the helper.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(PROJECT_DIR)).unwrap();
        let victim = root.join("victim.rc");
        std::fs::write(&victim, b"# victim\n").unwrap();
        std::os::unix::fs::symlink(&victim, feature_map_state_path(root)).unwrap();

        write_feature_map_state(root, 0xDEAD_BEEF);

        assert_eq!(std::fs::read(&victim).unwrap(), b"# victim\n");
    }

    #[cfg(unix)]
    #[test]
    fn read_feature_map_state_refuses_a_symlinked_source() {
        // Points at an ordinary file rather than /dev/zero so that reverting
        // the guard fails this test instead of hanging the suite.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(PROJECT_DIR)).unwrap();
        let victim = root.join("victim");
        std::fs::write(&victim, b"12345\n").unwrap();
        std::os::unix::fs::symlink(&victim, feature_map_state_path(root)).unwrap();

        assert_eq!(read_feature_map_state(root), None);
    }

    #[test]
    fn feature_map_state_round_trips_through_marker_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(PROJECT_DIR)).unwrap();
        assert_eq!(read_feature_map_state(dir.path()), None);
        write_feature_map_state(dir.path(), 0xDEAD_BEEF);
        assert_eq!(read_feature_map_state(dir.path()), Some(0xDEAD_BEEF));
        // Corrupt marker reads as absent (maps on next run).
        std::fs::write(feature_map_state_path(dir.path()), "not a number\n").unwrap();
        assert_eq!(read_feature_map_state(dir.path()), None);
    }

    #[test]
    fn record_feature_map_state_skips_marker_on_mapper_failure() {
        // A partial mapping run (mapper_errors non-empty) must not advance
        // the marker — the next pass has to re-map to reconcile the debt.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(PROJECT_DIR)).unwrap();
        let db = Database::open_in_memory().unwrap();

        record_feature_map_state(dir.path(), &db, &[], &["composer.json: broken".to_string()]);
        assert_eq!(
            read_feature_map_state(dir.path()),
            None,
            "marker must not advance on a partial run"
        );

        record_feature_map_state(dir.path(), &db, &[], &[]);
        assert!(
            read_feature_map_state(dir.path()).is_some(),
            "clean run must advance the marker"
        );
    }

    // ---------- status --json ----------

    #[test]
    fn status_semantic_state_serializes_prose_equivalent_fields() {
        let s = SemanticStatus {
            model: "m".to_string(),
            state: "stale",
            fingerprint: "mismatch",
            indexed_files: Some(10),
            stale_files: Some(2),
            missing_files: Some(1),
        };
        let v: serde_json::Value = serde_json::to_value(&s).unwrap();
        assert_eq!(v["state"], "stale");
        assert_eq!(v["stale_files"], 2);
        assert_eq!(v["fingerprint"], "mismatch");

        let missing = SemanticStatus {
            model: "m".to_string(),
            state: "missing",
            fingerprint: "absent",
            indexed_files: None,
            stale_files: None,
            missing_files: None,
        };
        let v: serde_json::Value = serde_json::to_value(&missing).unwrap();
        assert!(
            v.get("indexed_files").is_none(),
            "absent counts must be omitted, not null: {v}"
        );
    }
}
