use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use codesage_embed::config::EmbeddingConfig;
use codesage_embed::model::Embedder;
use codesage_graph::{
    index_files, list_dependencies, remove_files, semantic_index_files, semantic_remove_files,
};
use codesage_parser::detect::detect_language;
use codesage_parser::discover::content_hash;
use codesage_protocol::FileInfo;
use codesage_storage::Database;
use globset::{GlobSet, GlobSetBuilder};
use notify::{Event, EventKind, RecursiveMode, Watcher};

use crate::lockfile;

const DEFAULT_DEBOUNCE_MS: u64 = 1000;
const BATCH_THRESHOLD: usize = 10;
const BATCH_WINDOW: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub struct StateWatcherConfig {
    pub project_root: PathBuf,
    pub db_path: PathBuf,
    pub embed_config: EmbeddingConfig,
    pub exclude_patterns: Vec<String>,
    pub debounce_ms: u64,
    pub shutdown: Arc<AtomicBool>,
}

pub fn run_statewatcher(config: StateWatcherConfig) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let project_root = config.project_root.clone();

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
    })
    .context("creating filesystem watcher")?;

    watcher
        .watch(&project_root, RecursiveMode::Recursive)
        .context("starting recursive watch")?;

    let mut embedder = if config.embed_config.model.is_empty() {
        None
    } else {
        Some(Embedder::new(&config.embed_config)?)
    };

    let exclude_glob = build_globset(&config.exclude_patterns)?;
    let debounce = Duration::from_millis(config.debounce_ms);

    let mut pending: HashMap<PathBuf, Instant> = HashMap::new();
    let mut removed_paths: Vec<String> = Vec::new();
    let mut batch_event_times: Vec<Instant> = Vec::new();
    let mut currently_indexing: HashSet<PathBuf> = HashSet::new();
    let mut recheck_queue: HashSet<PathBuf> = HashSet::new();

    tracing::info!(
        root = %config.project_root.display(),
        debounce_ms = config.debounce_ms,
        "statewatcher started"
    );

    loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(event) => {
                for path in &event.paths {
                    let rel = match path.strip_prefix(&config.project_root) {
                        Ok(p) => p.to_path_buf(),
                        Err(_) => continue,
                    };

                    if !is_source_file(&rel) {
                        continue;
                    }

                    let rel_str = rel.to_string_lossy().to_string();
                    if exclude_glob.is_match(&rel_str) {
                        continue;
                    }

                    match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) => {
                            pending.insert(rel, Instant::now());
                            batch_event_times.push(Instant::now());
                        }
                        EventKind::Remove(_) => {
                            removed_paths.push(rel_str);
                        }
                        _ => {}
                    }
                }

                let now = Instant::now();
                batch_event_times.retain(|t| now - *t < BATCH_WINDOW);

                if batch_event_times.len() >= BATCH_THRESHOLD {
                    tracing::info!(
                        count = batch_event_times.len(),
                        "batch threshold reached, triggering bulk incremental index"
                    );
                    run_bulk_incremental(&config, &mut embedder);
                    pending.clear();
                    removed_paths.clear();
                    batch_event_times.clear();
                    continue;
                }

                if !removed_paths.is_empty() {
                    let paths = std::mem::take(&mut removed_paths);
                    handle_removals(&config, &paths);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if config.shutdown.load(Ordering::Relaxed) {
            tracing::info!("shutdown requested, draining pending work");
            drain_pending_force(
                &config,
                &mut pending,
                &mut currently_indexing,
                &mut recheck_queue,
                &exclude_glob,
                &mut embedder,
            );
            break;
        }

        drain_pending(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &exclude_glob,
            &mut embedder,
            debounce,
        );
    }

    // Drop the filesystem watcher first so its background thread joins
    // before we return. This avoids a FORTIFY warning from the notify
    // crate's internal mutex being destroyed while still in use.
    drop(watcher);

    tracing::info!("statewatcher stopped");
    Ok(())
}

fn drain_pending(
    config: &StateWatcherConfig,
    pending: &mut HashMap<PathBuf, Instant>,
    currently_indexing: &mut HashSet<PathBuf>,
    recheck_queue: &mut HashSet<PathBuf>,
    exclude_glob: &GlobSet,
    embedder: &mut Option<Embedder>,
    debounce: Duration,
) {
    let now = Instant::now();
    let ready: Vec<PathBuf> = pending
        .iter()
        .filter(|(_, t)| now - **t >= debounce)
        .map(|(p, _)| p.clone())
        .collect();

    process_ready(
        config,
        pending,
        currently_indexing,
        recheck_queue,
        exclude_glob,
        embedder,
        ready,
    );
}

fn drain_pending_force(
    config: &StateWatcherConfig,
    pending: &mut HashMap<PathBuf, Instant>,
    currently_indexing: &mut HashSet<PathBuf>,
    recheck_queue: &mut HashSet<PathBuf>,
    exclude_glob: &GlobSet,
    embedder: &mut Option<Embedder>,
) {
    let ready: Vec<PathBuf> = pending.keys().cloned().collect();
    process_ready(
        config,
        pending,
        currently_indexing,
        recheck_queue,
        exclude_glob,
        embedder,
        ready,
    );
}

fn process_ready(
    config: &StateWatcherConfig,
    pending: &mut HashMap<PathBuf, Instant>,
    currently_indexing: &mut HashSet<PathBuf>,
    recheck_queue: &mut HashSet<PathBuf>,
    exclude_glob: &GlobSet,
    embedder: &mut Option<Embedder>,
    ready: Vec<PathBuf>,
) {
    for path in ready {
        pending.remove(&path);

        let rel_str = path.to_string_lossy().to_string();

        if exclude_glob.is_match(&rel_str) {
            continue;
        }

        if currently_indexing.contains(&path) {
            recheck_queue.insert(path);
            continue;
        }

        currently_indexing.insert(path.clone());
        reindex_one(config, &path, embedder);
        currently_indexing.remove(&path);

        if recheck_queue.remove(&path) {
            let abs_path = config.project_root.join(&path);
            if abs_path.exists()
                && let Ok(bytes) = std::fs::read(&abs_path)
            {
                let new_hash = content_hash(&bytes);
                let needs_reindex = match Database::open(&config.db_path) {
                    Ok(db) => match db.get_file_hash(&rel_str) {
                        Ok(Some(stored_hash)) => new_hash != stored_hash,
                        _ => true,
                    },
                    Err(_) => true,
                };
                if needs_reindex {
                    pending.insert(path, Instant::now());
                }
            }
            continue;
        }

        // Cascade to direct importers.
        if embedder.is_some()
            && let Ok(db) = Database::open(&config.db_path)
            && let Ok(deps) = list_dependencies(&db, &rel_str)
        {
            for importer in &deps.imported_by {
                let importer_path = PathBuf::from(importer);
                if !pending.contains_key(&importer_path)
                    && !currently_indexing.contains(&importer_path)
                {
                    pending.insert(importer_path, Instant::now());
                }
            }
        }
    }
}

fn reindex_one(config: &StateWatcherConfig, rel: &Path, embedder: &mut Option<Embedder>) {
    let abs = config.project_root.join(rel);
    let rel_str = rel.to_string_lossy().to_string();

    let bytes = match std::fs::read(&abs) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(path = %rel_str, error = %e, "failed to read file for reindex");
            return;
        }
    };
    if bytes.is_empty() {
        return;
    }

    let hash = content_hash(&bytes);
    let Some(lang) = detect_language(rel) else {
        return;
    };

    if let Ok(db) = Database::open(&config.db_path)
        && let Ok(Some(stored)) = db.get_file_hash(&rel_str)
        && stored == hash
    {
        return;
    }

    let _lock = match lockfile::try_acquire(&config.project_root) {
        Ok(lockfile::LockOutcome::Acquired(lock)) => Some(lock),
        Ok(lockfile::LockOutcome::AlreadyHeld) => {
            tracing::debug!(
                path = %rel_str,
                "skipping reindex: index lock held by another process"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "acquiring index lock");
            return;
        }
    };

    let file_info = FileInfo {
        path: rel_str.clone(),
        language: lang,
        content_hash: hash,
    };

    match Database::open(&config.db_path) {
        Ok(db) => {
            match index_files(
                &config.project_root,
                &db,
                std::slice::from_ref(&file_info),
                false,
            ) {
                Ok(stats) => {
                    if stats.files_indexed > 0 {
                        tracing::info!(
                            path = %rel_str,
                            symbols = stats.symbols_found,
                            refs = stats.references_found,
                            "structural reindex"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %rel_str, error = %e, "structural reindex failed");
                }
            }
        }
        Err(e) => {
            tracing::warn!(path = %rel_str, error = %e, "opening DB for structural reindex");
            return;
        }
    }

    if let Some(emb) = embedder.as_mut() {
        let db = match Database::open_for_model(
            &config.db_path,
            &config.embed_config.model,
            emb.dim(),
        ) {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!(path = %rel_str, error = %e, "opening DB for semantic reindex");
                return;
            }
        };
        match semantic_index_files(
            &config.project_root,
            &db,
            emb,
            std::slice::from_ref(&file_info),
            false,
        ) {
            Ok(stats) => {
                if stats.files_processed > 0 {
                    tracing::info!(
                        path = %rel_str,
                        chunks = stats.chunks_created,
                        "semantic reindex"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(path = %rel_str, error = %e, "semantic reindex failed");
            }
        }
    }
}

fn handle_removals(config: &StateWatcherConfig, paths: &[String]) {
    if paths.is_empty() {
        return;
    }

    let _lock = match lockfile::try_acquire(&config.project_root) {
        Ok(lockfile::LockOutcome::Acquired(lock)) => Some(lock),
        _ => {
            tracing::debug!("skipping removal: index lock held");
            return;
        }
    };

    if let Ok(db) = Database::open(&config.db_path) {
        match remove_files(&db, paths) {
            Ok(n) => {
                if n > 0 {
                    tracing::info!(removed = n, paths = ?paths, "files removed from index");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "removing files from structural index");
            }
        }
    }

    for path in paths {
        if let Ok(db) = Database::open(&config.db_path) {
            let _ = semantic_remove_files(&db, std::slice::from_ref(path));
        }
    }
}

fn run_bulk_incremental(config: &StateWatcherConfig, embedder: &mut Option<Embedder>) {
    let _lock = match lockfile::try_acquire(&config.project_root) {
        Ok(lockfile::LockOutcome::Acquired(lock)) => Some(lock),
        _ => {
            tracing::debug!("skipping bulk incremental: index lock held");
            return;
        }
    };

    match Database::open(&config.db_path) {
        Ok(db) => {
            if let Err(e) = codesage_graph::incremental_index(
                &config.project_root,
                &db,
                &config.exclude_patterns,
                false,
            ) {
                tracing::warn!(error = %e, "bulk incremental structural reindex failed");
                return;
            }
            if let Some(emb) = embedder.as_mut() {
                let db = match Database::open_for_model(
                    &config.db_path,
                    &config.embed_config.model,
                    emb.dim(),
                ) {
                    Ok(db) => db,
                    Err(e) => {
                        tracing::warn!(error = %e, "opening DB for bulk semantic incremental");
                        return;
                    }
                };
                if let Err(e) = codesage_graph::semantic_incremental_index(
                    &config.project_root,
                    &db,
                    emb,
                    &config.exclude_patterns,
                    false,
                ) {
                    tracing::warn!(error = %e, "bulk incremental semantic reindex failed");
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "opening DB for bulk incremental");
        }
    }
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    if patterns.is_empty() {
        return Ok(GlobSet::empty());
    }
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        builder.add(globset::Glob::new(pat)?);
    }
    Ok(builder.build()?)
}

fn is_source_file(rel: &Path) -> bool {
    let Some(ext) = rel.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext,
        "rs" | "php"
            | "py"
            | "c"
            | "cpp"
            | "cc"
            | "cxx"
            | "h"
            | "hpp"
            | "java"
            | "js"
            | "ts"
            | "jsx"
            | "tsx"
            | "go"
            | "rb"
            | "swift"
            | "kt"
            | "kts"
            | "scala"
    )
}

pub fn resolve_debounce_ms() -> u64 {
    std::env::var("REINDEX_DEBOUNCE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_DEBOUNCE_MS)
}

static mut SHUTDOWN_PTR: Option<*const AtomicBool> = None;

extern "C" fn shutdown_signal_handler(_: libc::c_int) {
    unsafe {
        if let Some(ptr) = SHUTDOWN_PTR {
            (*ptr).store(true, Ordering::SeqCst);
        }
    }
}

pub fn register_shutdown_flag() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    unsafe {
        SHUTDOWN_PTR = Some(Arc::as_ptr(&flag));
        libc::signal(
            libc::SIGTERM,
            shutdown_signal_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            shutdown_signal_handler as *const () as libc::sighandler_t,
        );
    }
    flag
}
