use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use codesage_embed::config::EmbeddingConfig;
use codesage_embed::model::Embedder;
use codesage_graph::{index_files, remove_files, semantic_index_files, semantic_remove_files};
use codesage_parser::detect::{
    detect_language, detect_language_with_dialect, is_unambiguous_cpp_extension,
};
use codesage_parser::discover::{WatchFilter, content_hash};
use codesage_protocol::FileInfo;
use codesage_storage::Database;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::lockfile;

const DEFAULT_DEBOUNCE_MS: u64 = 1000;
const BATCH_THRESHOLD: usize = 10;
const BATCH_WINDOW: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// After a completed bulk pass, suppress threshold-triggered bulk passes for
/// this window. The burst's events keep arriving from the channel while the
/// pass runs and would re-cross the threshold every `BATCH_THRESHOLD` replayed
/// events — one checkout would otherwise trigger ~N/10 full-repo passes.
const BULK_COOLDOWN: Duration = Duration::from_secs(3);
const DEFAULT_IDLE_SECS: u64 = 1800;
/// Give up on a set of removals after this many consecutive hard failures.
/// A persistent failure (disk full, EACCES, corrupt table) will never clear,
/// and retaining the paths keeps the loop reporting activity — which would
/// pin the pooled embedder and its VRAM against the idle timeout forever.
/// Transient lock contention (`Skipped`) does not count toward this ceiling.
const MAX_REMOVAL_FAILURES: u32 = 10;

/// Lazily yields the embedder the watcher should use for semantic reindex.
/// In the daemon this hands back the pooled `Arc<Mutex<Embedder>>` shared with
/// live searches (no extra model load); the standalone `watch run` path builds
/// a private one. `None` means semantic indexing is disabled (no model
/// configured) and the watcher does structural-only updates.
pub type EmbedderProvider = Arc<dyn Fn() -> Result<Arc<Mutex<Embedder>>> + Send + Sync>;

/// Where the watcher is hosted. Recorded in the status file so `codesage watch
/// stop` knows whether the `pid` is safe to signal (a foreground process) or
/// must be asked to exit via the disabled marker (a daemon-owned thread).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatcherMode {
    Daemon,
    Foreground,
}

pub struct StateWatcherConfig {
    pub project_root: PathBuf,
    pub db_path: PathBuf,
    pub embed_config: EmbeddingConfig,
    pub exclude_patterns: Vec<String>,
    pub debounce_ms: u64,
    pub idle_timeout: Duration,
    pub mode: WatcherMode,
    pub embedder: Option<EmbedderProvider>,
    pub shutdown: Arc<AtomicBool>,
}

/// Resolves the embedder through the provider on every use, holding no ref
/// between uses. The provider is a pool lookup (map-lock + slot-lock) that
/// restamps the pooled entry's `last_used`, so a reindex keeps the model
/// marked in-use and the daemon's idle eviction won't drop a model the
/// watcher touched seconds ago. Holding no strong ref across idle periods
/// also means an idle watcher never pins the model's VRAM/host RSS against
/// eviction; an idle watcher never calls `get`, so it never forces a load.
struct EmbedderHandle {
    provider: Option<EmbedderProvider>,
}

impl EmbedderHandle {
    fn new(provider: Option<EmbedderProvider>) -> Self {
        Self { provider }
    }

    fn enabled(&self) -> bool {
        self.provider.is_some()
    }

    fn get(&mut self) -> Option<Arc<Mutex<Embedder>>> {
        let provider = self.provider.as_ref()?;
        match provider() {
            Ok(emb) => Some(emb),
            Err(e) => {
                tracing::warn!(error = %e, "loading embedder for watcher");
                None
            }
        }
    }
}

/// Outcome of a lock-guarded indexing pass. Callers must only discard
/// accumulated work (`pending` / `removed_paths`) on `Done`: a `Skipped`
/// pass did nothing, and dropping the state would silently lose the
/// changes it represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkOutcome {
    /// The pass ran; accumulated state it covered can be cleared.
    Done,
    /// The index lock was held by another process. Nothing was indexed;
    /// keep the state and retry after a debounce.
    Skipped,
    /// Hard error (I/O, DB). Logged; retrying the same pass is unlikely
    /// to help.
    Failed,
}

/// Removes the status file when the watcher loop exits by any path
/// (shutdown, idle, disabled marker, channel disconnect, or error).
struct StatusGuard(PathBuf);

impl Drop for StatusGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn run_statewatcher(config: StateWatcherConfig) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let project_root = config.project_root.clone();

    // Errors are forwarded, not dropped: an Err from the backend (inotify
    // queue overflow, rescan-needed) means events were lost, and the loop
    // must schedule a reconciliation pass or those edits stay unindexed.
    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        let _ = tx.send(res);
    })
    .context("creating filesystem watcher")?;

    let filter = WatchFilter::new(&config.project_root, &config.exclude_patterns)?;
    watch_tree(&mut watcher, &project_root, &filter);

    let mut embedder = EmbedderHandle::new(config.embedder.clone());

    let debounce = Duration::from_millis(config.debounce_ms);
    let disabled_marker = watch_disabled_path(&config.project_root);

    write_status(&config.project_root, config.mode)?;
    let _status_guard = StatusGuard(watch_status_path(&config.project_root));

    let mut pending: HashMap<PathBuf, Instant> = HashMap::new();
    let mut removed_paths: Vec<String> = Vec::new();
    let mut batch_event_times: Vec<Instant> = Vec::new();
    let mut currently_indexing: HashSet<PathBuf> = HashSet::new();
    let mut recheck_queue: HashSet<PathBuf> = HashSet::new();
    let mut bulk_retry_at: Option<Instant> = None;
    let mut bulk_cooldown_until: Option<Instant> = None;
    let mut removal_retry_at: Option<Instant> = None;
    let mut removal_fail_count: u32 = 0;
    let mut last_activity = Instant::now();
    let mut header_is_cpp = header_dialect_is_cpp(&config.db_path);

    tracing::info!(
        root = %config.project_root.display(),
        debounce_ms = config.debounce_ms,
        mode = ?config.mode,
        "statewatcher started"
    );

    let exit_reason = loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(Ok(event)) => {
                for path in &event.paths {
                    // A newly created top-level directory needs its own watch:
                    // the root is watched non-recursively, so new top-level
                    // trees aren't covered until we recurse into them.
                    if matches!(event.kind, EventKind::Create(_)) && path.is_dir() {
                        maybe_watch_new_dir(&mut watcher, &config.project_root, path, &filter);
                        continue;
                    }

                    let rel = match path.strip_prefix(&config.project_root) {
                        Ok(p) => p.to_path_buf(),
                        Err(_) => continue,
                    };

                    if !is_source_file(&rel) {
                        continue;
                    }

                    if filter.is_ignored(path, false) {
                        continue;
                    }

                    let rel_str = rel.to_string_lossy().to_string();
                    match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) => {
                            // First C++ file of the session: from here on,
                            // edited `.h` headers parse as C++, matching what
                            // the discovery layer would derive for this tree.
                            if !header_is_cpp
                                && rel
                                    .extension()
                                    .and_then(|e| e.to_str())
                                    .is_some_and(is_unambiguous_cpp_extension)
                            {
                                header_is_cpp = true;
                            }
                            // A re-creation cancels a still-queued removal for
                            // the same path; otherwise a removal deferred on
                            // lock contention would fire later and delete the
                            // live file's symbols/chunks.
                            removed_paths.retain(|p| *p != rel_str);
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
                    let burst = batch_event_times.len();
                    batch_event_times.clear();
                    if in_bulk_cooldown(bulk_cooldown_until, now) {
                        // Backlog replay: these events queued while the
                        // just-finished bulk pass ran, and that pass already
                        // observed the post-burst tree. Defer to one catch-up
                        // pass at cooldown expiry instead of re-running a full
                        // pass per BATCH_THRESHOLD replayed events. The Skipped
                        // disposition keeps every event queued, so an edit
                        // the pass raced past is never lost.
                        bulk_retry_at = apply_bulk_outcome(
                            WorkOutcome::Skipped,
                            &mut pending,
                            &mut removed_paths,
                            Instant::now(),
                            debounce,
                            bulk_cooldown_until,
                        );
                        continue;
                    }
                    tracing::info!(
                        count = burst,
                        "batch threshold reached, triggering bulk incremental index"
                    );
                    let outcome = run_bulk_incremental(&config, &mut embedder);
                    bulk_cooldown_until = bulk_cooldown_after(outcome, Instant::now());
                    if outcome == WorkOutcome::Done {
                        // Unconditional: a bulk pass can also delete the last
                        // C++ file, which must un-flip header parsing without
                        // a watcher restart.
                        header_is_cpp = header_dialect_is_cpp(&config.db_path);
                    }
                    bulk_retry_at = apply_bulk_outcome(
                        outcome,
                        &mut pending,
                        &mut removed_paths,
                        Instant::now(),
                        debounce,
                        bulk_cooldown_until,
                    );
                    continue;
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    "filesystem watcher reported an error; scheduling bulk reconciliation pass"
                );
                bulk_retry_at = schedule_watch_error_catchup(
                    &mut pending,
                    &mut removed_paths,
                    Instant::now(),
                    debounce,
                    bulk_cooldown_until,
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break "watcher channel closed",
        }

        if config.shutdown.load(Ordering::Relaxed) {
            tracing::info!("shutdown requested, draining pending work");
            if !removed_paths.is_empty() {
                let _ = handle_removals(&config, &removed_paths);
            }
            drain_pending_force(
                &config,
                &mut pending,
                &mut currently_indexing,
                &mut recheck_queue,
                &filter,
                &mut embedder,
                header_is_cpp,
            );
            break "shutdown";
        }

        if disabled_marker.exists() {
            break "disabled marker present";
        }

        // A lock-skipped bulk pass retries here, ahead of the per-file
        // drain, so a success clears `pending` wholesale instead of
        // re-walking it file by file.
        if let Some(at) = bulk_retry_at
            && Instant::now() >= at
        {
            let outcome = run_bulk_incremental(&config, &mut embedder);
            bulk_cooldown_until = bulk_cooldown_after(outcome, Instant::now());
            if outcome == WorkOutcome::Done {
                header_is_cpp = header_dialect_is_cpp(&config.db_path);
            }
            bulk_retry_at = apply_bulk_outcome(
                outcome,
                &mut pending,
                &mut removed_paths,
                Instant::now(),
                debounce,
                bulk_cooldown_until,
            );
        }

        // Deletions have no natural retrigger: a removal dropped on lock
        // contention would leave ghost symbols/chunks for the whole
        // session, so keep the paths queued and retry a debounce later.
        if removed_paths.is_empty() {
            removal_retry_at = None;
            removal_fail_count = 0;
        } else if removal_retry_at.is_none_or(|at| Instant::now() >= at) {
            let may_unflip = removed_paths_may_unflip_header(header_is_cpp, &removed_paths);
            let outcome = handle_removals(&config, &removed_paths);
            if outcome == WorkOutcome::Done && may_unflip {
                header_is_cpp = header_dialect_is_cpp(&config.db_path);
            }
            removal_retry_at = apply_removal_outcome(
                outcome,
                &mut removed_paths,
                &mut removal_fail_count,
                Instant::now(),
                debounce,
            );
        }

        if drain_pending(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &filter,
            &mut embedder,
            header_is_cpp,
            debounce,
        ) {
            header_is_cpp = header_dialect_is_cpp(&config.db_path);
        }

        // Idle clock: only queued or in-flight work counts as activity, so
        // raw FS events that fail the source/ignore filters can't keep the
        // watcher (and its pooled model) alive.
        if !pending.is_empty()
            || !currently_indexing.is_empty()
            || !recheck_queue.is_empty()
            || !removed_paths.is_empty()
            || bulk_retry_at.is_some()
        {
            last_activity = Instant::now();
        } else if is_idle(last_activity, config.idle_timeout) {
            break "idle timeout";
        }
    };

    // Drop the filesystem watcher first so its background thread joins
    // before we return. This avoids a FORTIFY warning from the notify
    // crate's internal mutex being destroyed while still in use.
    drop(watcher);

    tracing::info!(reason = exit_reason, "statewatcher stopped");
    Ok(())
}

/// Idle when a non-zero timeout has elapsed since the last activity.
/// A zero timeout disables self-exit (watcher runs until shutdown).
fn is_idle(last_activity: Instant, idle_timeout: Duration) -> bool {
    !idle_timeout.is_zero() && last_activity.elapsed() >= idle_timeout
}

#[allow(clippy::too_many_arguments)]
fn drain_pending(
    config: &StateWatcherConfig,
    pending: &mut HashMap<PathBuf, Instant>,
    currently_indexing: &mut HashSet<PathBuf>,
    recheck_queue: &mut HashSet<PathBuf>,
    filter: &WatchFilter,
    embedder: &mut EmbedderHandle,
    header_is_cpp: bool,
    debounce: Duration,
) -> bool {
    let ready = compute_ready(pending, Instant::now(), debounce);
    process_ready(
        config,
        pending,
        currently_indexing,
        recheck_queue,
        filter,
        embedder,
        header_is_cpp,
        ready,
    )
}

fn drain_pending_force(
    config: &StateWatcherConfig,
    pending: &mut HashMap<PathBuf, Instant>,
    currently_indexing: &mut HashSet<PathBuf>,
    recheck_queue: &mut HashSet<PathBuf>,
    filter: &WatchFilter,
    embedder: &mut EmbedderHandle,
    header_is_cpp: bool,
) {
    let ready: Vec<PathBuf> = pending.keys().cloned().collect();
    process_ready(
        config,
        pending,
        currently_indexing,
        recheck_queue,
        filter,
        embedder,
        header_is_cpp,
        ready,
    );
}

/// Paths whose debounce window has elapsed as of `now`.
fn compute_ready(
    pending: &HashMap<PathBuf, Instant>,
    now: Instant,
    debounce: Duration,
) -> Vec<PathBuf> {
    pending
        .iter()
        .filter(|(_, t)| now.duration_since(**t) >= debounce)
        .map(|(p, _)| p.clone())
        .collect()
}

/// Returns `true` when a completed reindex purged an unambiguous C++ file
/// (vanished or emptied on disk) while headers were parsing as C++ — the
/// caller must re-derive the header dialect, since that purge may have
/// removed the last C++ file in the index.
#[allow(clippy::too_many_arguments)]
fn process_ready(
    config: &StateWatcherConfig,
    pending: &mut HashMap<PathBuf, Instant>,
    currently_indexing: &mut HashSet<PathBuf>,
    recheck_queue: &mut HashSet<PathBuf>,
    filter: &WatchFilter,
    embedder: &mut EmbedderHandle,
    header_is_cpp: bool,
    ready: Vec<PathBuf>,
) -> bool {
    let mut rederive_header = false;
    for path in ready {
        pending.remove(&path);

        let rel_str = path.to_string_lossy().to_string();

        if filter.is_ignored(&config.project_root.join(&path), false) {
            continue;
        }

        if currently_indexing.contains(&path) {
            recheck_queue.insert(path);
            continue;
        }

        currently_indexing.insert(path.clone());
        let outcome = reindex_one(config, &path, embedder, header_is_cpp);
        currently_indexing.remove(&path);

        // Lock contention: re-queue with a fresh stamp so the retry waits
        // out a full debounce window instead of spinning on the held lock.
        if outcome == WorkOutcome::Skipped {
            pending.insert(path, Instant::now());
            continue;
        }

        if outcome == WorkOutcome::Done
            && header_is_cpp
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(is_unambiguous_cpp_extension)
            && !file_has_content(&config.project_root.join(&path))
        {
            rederive_header = true;
        }

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
        }
    }
    rederive_header
}

/// Present on disk with at least one byte. A vanished or emptied path had
/// its index rows purged by `reindex_one`, which is what the header-dialect
/// re-derivation gate cares about.
fn file_has_content(abs: &Path) -> bool {
    std::fs::metadata(abs).is_ok_and(|m| m.len() > 0)
}

fn reindex_one(
    config: &StateWatcherConfig,
    rel: &Path,
    embedder: &mut EmbedderHandle,
    header_is_cpp: bool,
) -> WorkOutcome {
    let abs = config.project_root.join(rel);
    let rel_str = rel.to_string_lossy().to_string();

    let bytes = match std::fs::read(&abs) {
        Ok(b) => b,
        // A vanished path is a removal, not a failure: notify's inotify
        // backend reports a rename's old path as a Modify (MOVED_FROM), and
        // editor swap patterns delete between event and read. Purge the rows
        // or ghost symbols/chunks persist for the session.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return purge_one_locked(config, &rel_str);
        }
        Err(e) => {
            tracing::warn!(path = %rel_str, error = %e, "failed to read file for reindex");
            return WorkOutcome::Failed;
        }
    };
    if bytes.is_empty() {
        return purge_one_locked(config, &rel_str);
    }

    let hash = content_hash(&bytes);
    let Some(lang) = detect_language_with_dialect(rel, header_is_cpp) else {
        return WorkOutcome::Done;
    };

    if indexed_hashes_are_fresh(config, &rel_str, &hash, embedder.enabled()) {
        return WorkOutcome::Done;
    }

    let _lock = match lockfile::try_acquire(&config.project_root) {
        Ok(lockfile::LockOutcome::Acquired(lock)) => Some(lock),
        Ok(lockfile::LockOutcome::AlreadyHeld) => {
            tracing::debug!(
                path = %rel_str,
                "deferring reindex: index lock held by another process"
            );
            return WorkOutcome::Skipped;
        }
        Err(e) => {
            tracing::warn!(error = %e, "acquiring index lock");
            return WorkOutcome::Failed;
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
                Err(e) if is_retryable_db_error(&e) => {
                    tracing::debug!(path = %rel_str, error = %e, "structural reindex deferred: database busy, will retry");
                    return WorkOutcome::Skipped;
                }
                Err(e) => {
                    tracing::warn!(path = %rel_str, error = %e, "structural reindex failed");
                    return WorkOutcome::Failed;
                }
            }
        }
        Err(e) => {
            tracing::warn!(path = %rel_str, error = %e, "opening DB for structural reindex");
            return WorkOutcome::Failed;
        }
    }

    if let Some(emb_arc) = embedder.get() {
        let mut emb = emb_arc.lock();
        let db = match Database::open_for_model(
            &config.db_path,
            &config.embed_config.model,
            emb.dim(),
        ) {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!(path = %rel_str, error = %e, "opening DB for semantic reindex");
                return WorkOutcome::Failed;
            }
        };
        match semantic_index_files(
            &config.project_root,
            &db,
            &mut emb,
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
            Err(e) if is_retryable_db_error(&e) => {
                tracing::debug!(path = %rel_str, error = %e, "semantic reindex deferred: database busy, will retry");
                return WorkOutcome::Skipped;
            }
            Err(e) => {
                tracing::warn!(path = %rel_str, error = %e, "semantic reindex failed");
                return WorkOutcome::Failed;
            }
        }
    }

    WorkOutcome::Done
}

fn indexed_hashes_are_fresh(
    config: &StateWatcherConfig,
    rel_str: &str,
    content_hash: &str,
    semantic_enabled: bool,
) -> bool {
    let Ok(db) = Database::open(&config.db_path) else {
        return false;
    };
    let structural_fresh =
        matches!(db.get_file_hash(rel_str), Ok(Some(stored)) if stored == content_hash);
    if !structural_fresh {
        return false;
    }
    if !semantic_enabled {
        return true;
    }

    let semantic_db =
        match Database::open_for_existing_model(&config.db_path, &config.embed_config.model) {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!(
                    path = %rel_str,
                    error = %e,
                    "opening DB for semantic freshness check"
                );
                return false;
            }
        };
    matches!(
        semantic_db.get_semantic_file_hash(rel_str),
        Ok(Some(stored)) if stored == content_hash
    )
}

fn handle_removals(config: &StateWatcherConfig, paths: &[String]) -> WorkOutcome {
    if paths.is_empty() {
        return WorkOutcome::Done;
    }

    // A path that exists on disk was re-created after its removal was queued
    // (git checkout back-and-forth, atomic-save editors). Deleting its rows
    // would silently drop a live file's symbols/chunks for the session, so
    // only purge paths that are genuinely gone.
    let to_remove: Vec<String> = paths
        .iter()
        .filter(|p| !config.project_root.join(p).exists())
        .cloned()
        .collect();
    if to_remove.is_empty() {
        return WorkOutcome::Done;
    }

    let _lock = match lockfile::try_acquire(&config.project_root) {
        Ok(lockfile::LockOutcome::Acquired(lock)) => Some(lock),
        Ok(lockfile::LockOutcome::AlreadyHeld) => {
            tracing::debug!("deferring removal: index lock held");
            return WorkOutcome::Skipped;
        }
        Err(e) => {
            tracing::warn!(error = %e, "acquiring index lock for removal");
            return WorkOutcome::Failed;
        }
    };

    purge_index_rows(config, &to_remove)
}

fn purge_index_rows(config: &StateWatcherConfig, paths: &[String]) -> WorkOutcome {
    let db = match Database::open(&config.db_path) {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!(error = %e, "opening DB for removal");
            return WorkOutcome::Failed;
        }
    };

    match remove_files(&db, paths) {
        Ok(n) => {
            if n > 0 {
                tracing::info!(removed = n, paths = ?paths, "files removed from index");
            }
        }
        // A busy/locked DB is transient (a concurrent daemon reader or the
        // lock holder). Retain the paths and retry rather than clear them,
        // which would leave ghost symbols/chunks for the session.
        Err(e) if is_retryable_db_error(&e) => {
            tracing::debug!(error = %e, "removal deferred: database busy, will retry");
            return WorkOutcome::Skipped;
        }
        Err(e) => {
            tracing::warn!(error = %e, "removing files from structural index");
            return WorkOutcome::Failed;
        }
    }

    let semantic_db =
        match Database::open_for_existing_model(&config.db_path, &config.embed_config.model) {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!(error = %e, "opening DB for semantic removal");
                return WorkOutcome::Failed;
            }
        };

    if let Err(e) = semantic_remove_files(&semantic_db, paths) {
        if is_retryable_db_error(&e) {
            tracing::debug!(error = %e, "semantic removal deferred: database busy, will retry");
            return WorkOutcome::Skipped;
        }
        tracing::warn!(error = %e, "removing files from semantic index");
        return WorkOutcome::Failed;
    }
    WorkOutcome::Done
}

/// Acquire the index lock and purge one path's structural + semantic rows.
/// For paths whose on-disk state no longer warrants index rows: emptied
/// files, and paths that vanished between the event and the read.
fn purge_one_locked(config: &StateWatcherConfig, rel_str: &str) -> WorkOutcome {
    let _lock = match lockfile::try_acquire(&config.project_root) {
        Ok(lockfile::LockOutcome::Acquired(lock)) => Some(lock),
        Ok(lockfile::LockOutcome::AlreadyHeld) => {
            tracing::debug!(
                path = %rel_str,
                "deferring index purge: index lock held by another process"
            );
            return WorkOutcome::Skipped;
        }
        Err(e) => {
            tracing::warn!(error = %e, "acquiring index lock for purge");
            return WorkOutcome::Failed;
        }
    };
    purge_index_rows(config, &[rel_str.to_string()])
}

/// Whether this project's bare `.h` headers should be parsed as C++. Mirrors
/// the discovery layer's rule — an unambiguous C++ extension anywhere in the
/// file set flips headers — against the indexed `files` table, so a watcher
/// reindex of an edited header stores the same language a full index would.
/// Keyed on path extension rather than the stored language column because
/// `.cu`/`.cuh` files are stored as C++ without implying the header flip.
///
/// Cost shape: one scan of the `files` path column (no hashes, no HashMap).
/// It runs at watcher startup, after every completed bulk pass, and after a
/// deletion that could have removed the last C++ file — the deletion sites
/// are gated so pure-C repos never pay it on the event path. The open must
/// not create a missing database: a reset deletes `index.db`, and a probe
/// that recreates it empty would make warm-state existence checks pass again.
fn header_dialect_is_cpp(db_path: &Path) -> bool {
    let Ok(db) = Database::open_existing(db_path) else {
        return false;
    };
    let Ok(paths) = db.all_file_paths() else {
        return false;
    };
    paths.iter().any(|p| {
        Path::new(p)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(is_unambiguous_cpp_extension)
    })
}

/// A completed removal pass can only lower the header dialect when the flag
/// is currently set and one of the removed paths was itself an unambiguous
/// C++ file — anything else leaves the indexed C++ set intact, so the
/// re-derivation scan is skipped.
fn removed_paths_may_unflip_header(header_is_cpp: bool, paths: &[String]) -> bool {
    header_is_cpp
        && paths.iter().any(|p| {
            Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(is_unambiguous_cpp_extension)
        })
}

/// A notify backend error (inotify queue overflow, rescan-needed) means
/// events were lost: the accumulated queues no longer describe everything
/// that changed on disk. Schedule a full reconciliation through the bulk
/// catch-up path — the `Skipped` disposition retains all queued state and
/// arms `bulk_retry_at` one debounce out, deferred to the end of any active
/// bulk cooldown. The rescan is never lost, only delayed: the pass that
/// armed the cooldown already observed the tree, and the catch-up at
/// cooldown expiry reconciles anything the overflow dropped after that.
fn schedule_watch_error_catchup(
    pending: &mut HashMap<PathBuf, Instant>,
    removed_paths: &mut Vec<String>,
    now: Instant,
    debounce: Duration,
    cooldown_until: Option<Instant>,
) -> Option<Instant> {
    apply_bulk_outcome(
        WorkOutcome::Skipped,
        pending,
        removed_paths,
        now,
        debounce,
        cooldown_until,
    )
}

/// Cooldown window armed by a completed bulk pass. `Skipped`/`Failed` arm
/// nothing: their state survives for the retry / per-file fallback, so a
/// follow-up burst should still be allowed to trigger a fresh pass.
fn bulk_cooldown_after(outcome: WorkOutcome, now: Instant) -> Option<Instant> {
    (outcome == WorkOutcome::Done).then(|| now + BULK_COOLDOWN)
}

fn in_bulk_cooldown(cooldown_until: Option<Instant>, now: Instant) -> bool {
    cooldown_until.is_some_and(|until| now < until)
}

/// SQLITE_BUSY / SQLITE_LOCKED are transient — a concurrent daemon reader or
/// the index-lock holder had the DB write-locked. Retrying after a debounce
/// clears them, so the caller must retain the work instead of dropping the
/// queued paths. Classified by rendered
/// message because `rusqlite` is not a direct dependency of this crate; SQLite
/// renders the whole busy/locked family as "... is locked".
fn is_retryable_db_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("is locked") || msg.contains("busy")
}

/// Apply a bulk-incremental outcome to the accumulated watch state,
/// returning when (if at all) the bulk pass should be retried. `Done`
/// clears the state the pass covered. `Skipped` keeps it, restamps
/// `pending` so the per-file drain doesn't race the bulk retry, and
/// schedules that retry one debounce out — never sooner, so lock
/// contention can't turn into a tight respin, and never inside an active
/// bulk cooldown: a threshold crossing during the cooldown would otherwise
/// re-arm a catch-up one debounce later and defeat the cooldown one retry
/// at a time. `Failed` keeps the state with its original stamps: the
/// per-file drain becomes the fallback, with per-file error logging.
fn apply_bulk_outcome(
    outcome: WorkOutcome,
    pending: &mut HashMap<PathBuf, Instant>,
    removed_paths: &mut Vec<String>,
    now: Instant,
    debounce: Duration,
    cooldown_until: Option<Instant>,
) -> Option<Instant> {
    match outcome {
        WorkOutcome::Done => {
            pending.clear();
            removed_paths.clear();
            None
        }
        WorkOutcome::Skipped => {
            let retry_at = cooldown_until.map_or(now + debounce, |cd| (now + debounce).max(cd));
            // Stamp so nothing becomes drain-ready before the retry fires;
            // retry_at >= now + debounce keeps this stamp at or after `now`.
            let stamp = retry_at - debounce;
            for s in pending.values_mut() {
                *s = stamp;
            }
            Some(retry_at)
        }
        WorkOutcome::Failed => None,
    }
}

/// Apply a removal pass outcome to the accumulated removal state, returning
/// when (if at all) the removal should be retried. `Done` clears the paths and
/// resets the failure counter. `Skipped` (transient lock contention) keeps the
/// paths and reschedules without counting toward the give-up ceiling. `Failed`
/// counts toward the ceiling; after `MAX_REMOVAL_FAILURES` consecutive hard
/// failures it gives up — clearing the paths so the loop stops reporting
/// activity and the watcher can idle out instead of pinning the pooled model.
fn apply_removal_outcome(
    outcome: WorkOutcome,
    removed_paths: &mut Vec<String>,
    fail_count: &mut u32,
    now: Instant,
    debounce: Duration,
) -> Option<Instant> {
    match outcome {
        WorkOutcome::Done => {
            removed_paths.clear();
            *fail_count = 0;
            None
        }
        WorkOutcome::Skipped => Some(now + debounce),
        WorkOutcome::Failed => {
            *fail_count += 1;
            if *fail_count >= MAX_REMOVAL_FAILURES {
                tracing::error!(
                    failures = *fail_count,
                    paths = ?removed_paths,
                    "giving up on index removals after repeated hard failures; \
                     index may retain stale rows until the next full reindex"
                );
                removed_paths.clear();
                *fail_count = 0;
                None
            } else {
                Some(now + debounce)
            }
        }
    }
}

fn run_bulk_incremental(config: &StateWatcherConfig, embedder: &mut EmbedderHandle) -> WorkOutcome {
    let _lock = match lockfile::try_acquire(&config.project_root) {
        Ok(lockfile::LockOutcome::Acquired(lock)) => Some(lock),
        Ok(lockfile::LockOutcome::AlreadyHeld) => {
            tracing::debug!("deferring bulk incremental: index lock held");
            return WorkOutcome::Skipped;
        }
        Err(e) => {
            tracing::warn!(error = %e, "acquiring index lock for bulk incremental");
            return WorkOutcome::Failed;
        }
    };

    let db = match Database::open(&config.db_path) {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!(error = %e, "opening DB for bulk incremental");
            return WorkOutcome::Failed;
        }
    };

    if let Err(e) = codesage_graph::incremental_index(
        &config.project_root,
        &db,
        &config.exclude_patterns,
        false,
    ) {
        tracing::warn!(error = %e, "bulk incremental structural reindex failed");
        return WorkOutcome::Failed;
    }

    if let Some(emb_arc) = embedder.get() {
        let mut emb = emb_arc.lock();
        let db = match Database::open_for_model(
            &config.db_path,
            &config.embed_config.model,
            emb.dim(),
        ) {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!(error = %e, "opening DB for bulk semantic incremental");
                return WorkOutcome::Failed;
            }
        };
        if let Err(e) = codesage_graph::semantic_incremental_index(
            &config.project_root,
            &db,
            &mut emb,
            &config.exclude_patterns,
            false,
        ) {
            tracing::warn!(error = %e, "bulk incremental semantic reindex failed");
            return WorkOutcome::Failed;
        }
    }

    WorkOutcome::Done
}

/// Set up the inotify watch set: the root non-recursively (top-level files +
/// detecting new top-level directories), plus a recursive watch on each
/// non-ignored top-level directory. This keeps `target/`, `.git/`,
/// `node_modules/`, and gitignored top-level trees out of the watch set, so the
/// watcher doesn't burn inotify descriptors or wake on build/VCS churn.
fn watch_tree(watcher: &mut RecommendedWatcher, root: &Path, filter: &WatchFilter) {
    if let Err(e) = watcher.watch(root, RecursiveMode::NonRecursive) {
        tracing::warn!(error = %e, "watching project root");
        return;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "reading project root for watch set");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir
            && !filter.is_ignored(&path, true)
            && let Err(e) = watcher.watch(&path, RecursiveMode::Recursive)
        {
            tracing::warn!(path = %path.display(), error = %e, "watching subtree");
        }
    }
}

/// Extend the watch set when a new top-level directory appears. Deeper new
/// directories are already covered by the recursive watch on their top-level
/// ancestor, so only immediate children of the root need an explicit watch.
fn maybe_watch_new_dir(
    watcher: &mut RecommendedWatcher,
    root: &Path,
    path: &Path,
    filter: &WatchFilter,
) {
    if path.parent() == Some(root)
        && !filter.is_ignored(path, true)
        && let Err(e) = watcher.watch(path, RecursiveMode::Recursive)
    {
        tracing::warn!(path = %path.display(), error = %e, "watching new top-level dir");
    }
}

/// A watch candidate: a known source extension we have a tree-sitter grammar
/// for. `detect_language` is the source of truth downstream; this is the cheap
/// pre-filter so we don't even queue files we can't parse.
fn is_source_file(rel: &Path) -> bool {
    detect_language(rel).is_some()
}

pub fn resolve_debounce_ms() -> u64 {
    std::env::var("REINDEX_DEBOUNCE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_DEBOUNCE_MS)
}

/// Idle window before a watcher self-exits, from `CODESAGE_WATCH_IDLE_SECS`.
/// `0` disables self-exit. Defaults to 30 minutes.
pub fn resolve_idle_timeout() -> Duration {
    let secs = std::env::var("CODESAGE_WATCH_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_IDLE_SECS);
    Duration::from_secs(secs)
}

// ---- status + control files -------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchStatus {
    pub mode: WatcherMode,
    pub pid: u32,
    pub started_at_unix: u64,
}

pub fn watch_status_path(root: &Path) -> PathBuf {
    root.join(".codesage").join("watch.status")
}

pub fn watch_disabled_path(root: &Path) -> PathBuf {
    root.join(".codesage").join("watch.disabled")
}

fn write_status(root: &Path, mode: WatcherMode) -> Result<()> {
    let started_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let status = WatchStatus {
        mode,
        pid: std::process::id(),
        started_at_unix,
    };
    let path = watch_status_path(root);
    let json = serde_json::to_string(&status)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn read_status(root: &Path) -> Option<WatchStatus> {
    let path = watch_status_path(root);
    let raw = std::fs::read_to_string(&path).ok()?;
    let status: WatchStatus = serde_json::from_str(&raw).ok()?;
    // An abrupt daemon/process death leaves the status file behind without the
    // owning thread running its cleanup. Treat a status whose recorded pid is
    // gone as inactive, and prune the stale file.
    if !process_alive(status.pid) {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(status)
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // kill(pid, 0): 0 => alive; EPERM => alive but not ours; ESRCH => gone.
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true
}

/// Whether the watcher is enabled for this project: not opted out via
/// `[index] watch = false`, not globally disabled via `CODESAGE_WATCH=0`,
/// and no `watch.disabled` marker present.
pub fn watch_enabled(root: &Path, config_watch: Option<bool>) -> bool {
    if config_watch == Some(false) {
        return false;
    }
    if let Ok(v) = std::env::var("CODESAGE_WATCH")
        && matches!(v.as_str(), "0" | "false" | "off" | "no")
    {
        return false;
    }
    !watch_disabled_path(root).exists()
}

// ---- standalone signal handling ---------------------------------------------

/// Holds a leaked pointer to the standalone watcher's shutdown flag so the
/// async-signal-safe handler can flip it. Using `AtomicPtr` + a leaked `Arc`
/// (rather than `static mut`) keeps the access sound: the flag lives for the
/// whole process and the load/store are atomic.
static STANDALONE_SHUTDOWN_PTR: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());

extern "C" fn shutdown_signal_handler(_: libc::c_int) {
    let ptr = STANDALONE_SHUTDOWN_PTR.load(Ordering::Acquire);
    if !ptr.is_null() {
        // Safety: `ptr` came from a leaked Arc that is never freed, so it
        // stays valid for the process lifetime; `store` is a single atomic
        // write, which is async-signal-safe.
        unsafe { (*ptr).store(true, Ordering::SeqCst) };
    }
}

pub fn register_shutdown_flag() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    // Leak one strong reference so the pointed-to AtomicBool outlives every
    // signal that may arrive before the process exits.
    let raw = Arc::into_raw(flag.clone()) as *mut AtomicBool;
    STANDALONE_SHUTDOWN_PTR.store(raw, Ordering::Release);
    unsafe {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_source_file_accepts_known_langs() {
        assert!(is_source_file(Path::new("src/main.rs")));
        assert!(is_source_file(Path::new("app/Foo.php")));
        assert!(is_source_file(Path::new("pkg/x.go")));
        assert!(is_source_file(Path::new("a/b/c.ts")));
    }

    #[test]
    fn is_source_file_rejects_non_source() {
        assert!(!is_source_file(Path::new("README.md")));
        assert!(!is_source_file(Path::new("data.json")));
        assert!(!is_source_file(Path::new("noext")));
        assert!(!is_source_file(Path::new(".codesage/index.db")));
    }

    #[test]
    fn compute_ready_respects_debounce() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let mut pending = HashMap::new();
        // One stale (ready), one fresh (not yet).
        pending.insert(PathBuf::from("ready.rs"), now - Duration::from_millis(600));
        pending.insert(PathBuf::from("fresh.rs"), now - Duration::from_millis(100));

        let ready = compute_ready(&pending, now, debounce);
        assert_eq!(ready, vec![PathBuf::from("ready.rs")]);
    }

    #[test]
    fn compute_ready_empty_when_all_fresh() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("a.rs"), now);
        assert!(compute_ready(&pending, now, debounce).is_empty());
    }

    #[test]
    fn batch_window_retains_only_recent() {
        let now = Instant::now();
        let mut times = vec![
            now - Duration::from_secs(5), // outside 3s window
            now - Duration::from_secs(1), // inside
            now,                          // inside
        ];
        times.retain(|t| now.duration_since(*t) < BATCH_WINDOW);
        assert_eq!(times.len(), 2);
    }

    #[test]
    fn idle_zero_timeout_never_idle() {
        assert!(!is_idle(Instant::now(), Duration::ZERO));
    }

    #[test]
    fn idle_elapsed_triggers() {
        let past = Instant::now() - Duration::from_secs(10);
        assert!(is_idle(past, Duration::from_secs(1)));
        assert!(!is_idle(Instant::now(), Duration::from_secs(60)));
    }

    fn test_config(root: &Path) -> StateWatcherConfig {
        StateWatcherConfig {
            project_root: root.to_path_buf(),
            db_path: root.join(".codesage").join("index.db"),
            embed_config: EmbeddingConfig::default(),
            exclude_patterns: vec![],
            debounce_ms: 100,
            idle_timeout: Duration::ZERO,
            mode: WatcherMode::Foreground,
            embedder: None,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    fn hold_lock(root: &Path) -> lockfile::IndexLock {
        match lockfile::try_acquire(root).unwrap() {
            lockfile::LockOutcome::Acquired(l) => l,
            lockfile::LockOutcome::AlreadyHeld => panic!("fresh tmpdir must lock"),
        }
    }

    #[test]
    fn apply_bulk_outcome_done_clears_accumulated_state() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("a.rs"), now);
        let mut removed = vec!["gone.rs".to_string()];

        let retry = apply_bulk_outcome(
            WorkOutcome::Done,
            &mut pending,
            &mut removed,
            now,
            debounce,
            None,
        );

        assert_eq!(retry, None);
        assert!(pending.is_empty());
        assert!(removed.is_empty());
    }

    #[test]
    fn apply_bulk_outcome_skipped_retains_and_rearms_after_debounce() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let stale = now - Duration::from_secs(10);
        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("a.rs"), stale);
        pending.insert(PathBuf::from("b.rs"), stale);
        let mut removed = vec!["gone.rs".to_string()];

        let retry = apply_bulk_outcome(
            WorkOutcome::Skipped,
            &mut pending,
            &mut removed,
            now,
            debounce,
            None,
        );

        // Everything retained, bulk retry scheduled a full debounce out.
        assert_eq!(retry, Some(now + debounce));
        assert_eq!(pending.len(), 2);
        assert_eq!(removed, vec!["gone.rs".to_string()]);
        // Stamps were refreshed: nothing is drain-ready before the retry
        // fires, so the skipped batch can't respin through the per-file path.
        assert!(compute_ready(&pending, now, debounce).is_empty());
        assert_eq!(compute_ready(&pending, now + debounce, debounce).len(), 2);
    }

    #[test]
    fn apply_bulk_outcome_failed_retains_for_per_file_fallback() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let stale = now - Duration::from_secs(10);
        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("a.rs"), stale);
        let mut removed = vec!["gone.rs".to_string()];

        let retry = apply_bulk_outcome(
            WorkOutcome::Failed,
            &mut pending,
            &mut removed,
            now,
            debounce,
            None,
        );

        // No bulk retry, but the state survives with its original stamps so
        // the per-file drain picks it up immediately.
        assert_eq!(retry, None);
        assert_eq!(compute_ready(&pending, now, debounce).len(), 1);
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn apply_removal_outcome_skipped_retains_without_counting_failures() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let mut removed = vec!["gone.rs".to_string()];
        let mut fails = 0;

        let retry = apply_removal_outcome(
            WorkOutcome::Skipped,
            &mut removed,
            &mut fails,
            now,
            debounce,
        );

        assert_eq!(retry, Some(now + debounce));
        assert_eq!(removed, vec!["gone.rs".to_string()]);
        // Lock contention is not a hard failure — the give-up ceiling stays put.
        assert_eq!(fails, 0);
    }

    #[test]
    fn apply_removal_outcome_gives_up_after_ceiling() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let mut removed = vec!["gone.rs".to_string()];
        let mut fails = 0;

        // Every hard failure short of the ceiling retains the paths and reschedules.
        for _ in 0..(MAX_REMOVAL_FAILURES - 1) {
            let retry =
                apply_removal_outcome(WorkOutcome::Failed, &mut removed, &mut fails, now, debounce);
            assert_eq!(retry, Some(now + debounce));
            assert_eq!(removed.len(), 1);
        }

        // The ceiling hit: give up, drop the paths so the loop stops reporting
        // activity and the watcher can idle out instead of pinning the model.
        let retry =
            apply_removal_outcome(WorkOutcome::Failed, &mut removed, &mut fails, now, debounce);
        assert_eq!(retry, None);
        assert!(removed.is_empty());
        assert_eq!(fails, 0);
    }

    #[test]
    fn apply_removal_outcome_done_resets_failure_streak() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let mut removed = vec!["gone.rs".to_string()];
        let mut fails = 3;

        let retry =
            apply_removal_outcome(WorkOutcome::Done, &mut removed, &mut fails, now, debounce);

        assert_eq!(retry, None);
        assert!(removed.is_empty());
        assert_eq!(fails, 0);
    }

    #[test]
    fn handle_removals_skipped_when_lock_held() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let config = test_config(root);

        let _held = hold_lock(root);
        let paths = vec!["gone.rs".to_string()];
        assert_eq!(handle_removals(&config, &paths), WorkOutcome::Skipped);
    }

    #[test]
    fn handle_removals_done_when_lock_free() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let config = test_config(root);

        // `Database::open` creates the schema on a fresh path; removing
        // paths absent from the index is a no-op success.
        let paths = vec!["gone.rs".to_string()];
        assert_eq!(handle_removals(&config, &paths), WorkOutcome::Done);
    }

    #[test]
    fn handle_removals_skips_recreated_file() {
        // A removal queued while the path still exists on disk (a re-creation
        // raced ahead of the retry) must not delete the live file's rows.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let src = "fn main() {}\n";
        std::fs::write(root.join("foo.rs"), src).unwrap();
        let config = test_config(root);

        let db = Database::open(&config.db_path).unwrap();
        let file_info = FileInfo {
            path: "foo.rs".to_string(),
            language: codesage_protocol::Language::Rust,
            content_hash: content_hash(src.as_bytes()),
        };
        index_files(root, &db, std::slice::from_ref(&file_info), false).unwrap();
        assert!(db.get_file_hash("foo.rs").unwrap().is_some());
        drop(db);

        // foo.rs is present on disk, so the removal is a no-op and the rows
        // survive.
        assert_eq!(
            handle_removals(&config, &["foo.rs".to_string()]),
            WorkOutcome::Done
        );
        let db = Database::open(&config.db_path).unwrap();
        assert!(
            db.get_file_hash("foo.rs").unwrap().is_some(),
            "re-created file must keep its indexed rows"
        );
    }

    #[test]
    fn is_retryable_db_error_classifies_busy_vs_permanent() {
        assert!(is_retryable_db_error(&anyhow::anyhow!(
            "database is locked"
        )));
        assert!(is_retryable_db_error(&anyhow::anyhow!(
            "database table is locked"
        )));
        // The classifier walks the anyhow context chain, not just the top.
        assert!(is_retryable_db_error(
            &anyhow::anyhow!("database is locked").context("removing files")
        ));
        assert!(!is_retryable_db_error(&anyhow::anyhow!(
            "no such table: files"
        )));
        assert!(!is_retryable_db_error(&anyhow::anyhow!("disk I/O error")));
    }

    #[test]
    fn reindex_one_skipped_when_lock_held() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::write(root.join("foo.rs"), "fn main() {}\n").unwrap();
        let config = test_config(root);
        let mut embedder = EmbedderHandle::new(None);

        let _held = hold_lock(root);
        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), &mut embedder, false),
            WorkOutcome::Skipped
        );
    }

    #[test]
    fn reindex_one_empty_file_removes_existing_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let src = "fn foo() {}\n";
        std::fs::write(root.join("foo.rs"), src).unwrap();
        let config = test_config(root);
        let hash = content_hash(src.as_bytes());

        let db = Database::open(&config.db_path).unwrap();
        let file_info = FileInfo {
            path: "foo.rs".to_string(),
            language: codesage_protocol::Language::Rust,
            content_hash: hash.clone(),
        };
        index_files(root, &db, std::slice::from_ref(&file_info), false).unwrap();
        assert!(db.get_file_hash("foo.rs").unwrap().is_some());
        drop(db);

        let semantic_db = Database::open_for_model(
            &config.db_path,
            &config.embed_config.model,
            codesage_storage::db::DEFAULT_EMBEDDING_DIM,
        )
        .unwrap();
        let embedding = vec![0.0; codesage_storage::db::DEFAULT_EMBEDDING_DIM];
        semantic_db
            .insert_chunks("foo.rs", "rust", &[("fn foo() {}", 1, 1, &embedding)])
            .unwrap();
        semantic_db
            .upsert_semantic_file_hash("foo.rs", &hash)
            .unwrap();
        assert_eq!(semantic_db.chunks_for_file("foo.rs").unwrap().len(), 1);
        drop(semantic_db);

        std::fs::write(root.join("foo.rs"), "").unwrap();
        let mut embedder = EmbedderHandle::new(None);

        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), &mut embedder, false),
            WorkOutcome::Done
        );

        let db = Database::open(&config.db_path).unwrap();
        assert!(db.get_file_hash("foo.rs").unwrap().is_none());
        drop(db);
        let semantic_db =
            Database::open_for_existing_model(&config.db_path, &config.embed_config.model).unwrap();
        assert!(semantic_db.chunks_for_file("foo.rs").unwrap().is_empty());
        assert!(
            semantic_db
                .get_semantic_file_hash("foo.rs")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reindex_one_empty_file_removes_structural_only_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let src = "fn foo() {}\n";
        std::fs::write(root.join("foo.rs"), src).unwrap();
        let config = test_config(root);

        let db = Database::open(&config.db_path).unwrap();
        index_files(
            root,
            &db,
            &[FileInfo {
                path: "foo.rs".to_string(),
                language: codesage_protocol::Language::Rust,
                content_hash: content_hash(src.as_bytes()),
            }],
            false,
        )
        .unwrap();
        assert!(db.get_file_hash("foo.rs").unwrap().is_some());
        drop(db);
        assert!(
            Database::open_for_existing_model(&config.db_path, &config.embed_config.model)
                .unwrap()
                .chunk_table_name()
                .is_empty(),
            "fixture must remain structural-only"
        );

        std::fs::write(root.join("foo.rs"), "").unwrap();
        let mut embedder = EmbedderHandle::new(None);

        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), &mut embedder, false),
            WorkOutcome::Done
        );

        let db = Database::open(&config.db_path).unwrap();
        assert!(db.get_file_hash("foo.rs").unwrap().is_none());
    }

    #[test]
    fn indexed_hashes_are_fresh_requires_semantic_hash_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let src = "fn foo() {}\n";
        std::fs::write(root.join("foo.rs"), src).unwrap();
        let config = test_config(root);
        let hash = content_hash(src.as_bytes());

        let db = Database::open(&config.db_path).unwrap();
        index_files(
            root,
            &db,
            &[FileInfo {
                path: "foo.rs".to_string(),
                language: codesage_protocol::Language::Rust,
                content_hash: hash.clone(),
            }],
            false,
        )
        .unwrap();
        drop(db);

        assert!(indexed_hashes_are_fresh(&config, "foo.rs", &hash, false));
        assert!(!indexed_hashes_are_fresh(&config, "foo.rs", &hash, true));
    }

    #[test]
    fn process_ready_requeues_on_lock_contention() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::write(root.join("foo.rs"), "fn main() {}\n").unwrap();
        let config = test_config(root);
        let filter = WatchFilter::new(root, &config.exclude_patterns).unwrap();
        let mut embedder = EmbedderHandle::new(None);

        let debounce = Duration::from_millis(100);
        let stale = Instant::now() - Duration::from_secs(10);
        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("foo.rs"), stale);
        let mut currently_indexing = HashSet::new();
        let mut recheck_queue = HashSet::new();

        let _held = hold_lock(root);
        process_ready(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &filter,
            &mut embedder,
            false,
            vec![PathBuf::from("foo.rs")],
        );

        // Re-queued with a fresh stamp: still pending, but not drain-ready
        // until another debounce window elapses.
        let stamp = pending
            .get(Path::new("foo.rs"))
            .copied()
            .expect("path re-queued after lock skip");
        assert!(stamp > stale);
        assert!(compute_ready(&pending, Instant::now(), debounce).is_empty());
        assert!(currently_indexing.is_empty());
        assert!(recheck_queue.is_empty());
    }

    #[test]
    fn reindex_one_missing_file_purges_index_rows() {
        // notify's inotify backend reports a rename's old path as a Modify
        // event, so a vanished path reaches reindex_one; it must purge the
        // stale rows rather than fail and strand them for the session.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let src = "fn foo() {}\n";
        std::fs::write(root.join("foo.rs"), src).unwrap();
        let config = test_config(root);

        let db = Database::open(&config.db_path).unwrap();
        index_files(
            root,
            &db,
            &[FileInfo {
                path: "foo.rs".to_string(),
                language: codesage_protocol::Language::Rust,
                content_hash: content_hash(src.as_bytes()),
            }],
            false,
        )
        .unwrap();
        assert!(db.get_file_hash("foo.rs").unwrap().is_some());
        drop(db);

        std::fs::remove_file(root.join("foo.rs")).unwrap();
        let mut embedder = EmbedderHandle::new(None);

        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), &mut embedder, false),
            WorkOutcome::Done
        );
        let db = Database::open(&config.db_path).unwrap();
        assert!(
            db.get_file_hash("foo.rs").unwrap().is_none(),
            "vanished file must have its index rows purged"
        );
    }

    #[test]
    fn reindex_one_missing_file_defers_when_lock_held() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let src = "fn foo() {}\n";
        std::fs::write(root.join("foo.rs"), src).unwrap();
        let config = test_config(root);

        let db = Database::open(&config.db_path).unwrap();
        index_files(
            root,
            &db,
            &[FileInfo {
                path: "foo.rs".to_string(),
                language: codesage_protocol::Language::Rust,
                content_hash: content_hash(src.as_bytes()),
            }],
            false,
        )
        .unwrap();
        drop(db);

        std::fs::remove_file(root.join("foo.rs")).unwrap();
        let mut embedder = EmbedderHandle::new(None);

        let _held = hold_lock(root);
        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), &mut embedder, false),
            WorkOutcome::Skipped
        );
        let db = Database::open(&config.db_path).unwrap();
        assert!(
            db.get_file_hash("foo.rs").unwrap().is_some(),
            "deferred purge must retain the rows for the retry"
        );
    }

    #[test]
    fn reindex_one_stores_header_as_cpp_in_cpp_project() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::write(root.join("api.h"), "struct foo { int x; };\n").unwrap();
        let config = test_config(root);
        let mut embedder = EmbedderHandle::new(None);

        assert_eq!(
            reindex_one(&config, Path::new("api.h"), &mut embedder, true),
            WorkOutcome::Done
        );

        let db = Database::open(&config.db_path).unwrap();
        let lang = db
            .all_files_with_id_and_language()
            .unwrap()
            .into_iter()
            .find(|(_, p, _)| p == "api.h")
            .map(|(_, _, l)| l);
        assert_eq!(
            lang,
            Some(codesage_protocol::Language::Cpp),
            "an edited .h in a C++ project must keep its C++ language row"
        );
    }

    #[test]
    fn reindex_one_stores_header_as_c_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::write(root.join("api.h"), "struct foo { int x; };\n").unwrap();
        let config = test_config(root);
        let mut embedder = EmbedderHandle::new(None);

        assert_eq!(
            reindex_one(&config, Path::new("api.h"), &mut embedder, false),
            WorkOutcome::Done
        );

        let db = Database::open(&config.db_path).unwrap();
        let lang = db
            .all_files_with_id_and_language()
            .unwrap()
            .into_iter()
            .find(|(_, p, _)| p == "api.h")
            .map(|(_, _, l)| l);
        assert_eq!(lang, Some(codesage_protocol::Language::C));
    }

    #[test]
    fn header_dialect_true_when_indexed_set_has_cpp_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let config = test_config(root);

        let db = Database::open(&config.db_path).unwrap();
        db.upsert_file(&FileInfo {
            path: "src/main.cpp".to_string(),
            language: codesage_protocol::Language::Cpp,
            content_hash: "x".to_string(),
        })
        .unwrap();
        db.upsert_file(&FileInfo {
            path: "src/util.h".to_string(),
            language: codesage_protocol::Language::Cpp,
            content_hash: "x".to_string(),
        })
        .unwrap();
        drop(db);

        assert!(header_dialect_is_cpp(&config.db_path));
    }

    #[test]
    fn header_dialect_false_for_c_and_cuda_only_sets() {
        // kernel.cu is stored with language cpp but must not flip headers —
        // mirrors the discovery layer keeping .cu out of the unambiguous set.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let config = test_config(root);

        let db = Database::open(&config.db_path).unwrap();
        db.upsert_file(&FileInfo {
            path: "src/main.c".to_string(),
            language: codesage_protocol::Language::C,
            content_hash: "x".to_string(),
        })
        .unwrap();
        db.upsert_file(&FileInfo {
            path: "src/kernel.cu".to_string(),
            language: codesage_protocol::Language::Cpp,
            content_hash: "x".to_string(),
        })
        .unwrap();
        db.upsert_file(&FileInfo {
            path: "src/api.h".to_string(),
            language: codesage_protocol::Language::C,
            content_hash: "x".to_string(),
        })
        .unwrap();
        drop(db);

        assert!(!header_dialect_is_cpp(&config.db_path));
    }

    #[test]
    fn header_dialect_unflips_after_last_cpp_file_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let config = test_config(root);

        let db = Database::open(&config.db_path).unwrap();
        db.upsert_file(&FileInfo {
            path: "src/main.cpp".to_string(),
            language: codesage_protocol::Language::Cpp,
            content_hash: "x".to_string(),
        })
        .unwrap();
        db.upsert_file(&FileInfo {
            path: "src/util.h".to_string(),
            language: codesage_protocol::Language::Cpp,
            content_hash: "x".to_string(),
        })
        .unwrap();
        drop(db);
        assert!(header_dialect_is_cpp(&config.db_path));

        // main.cpp is not on disk, so the removal pass purges its row; the
        // remaining set (one bare header) no longer proves C++.
        assert_eq!(
            handle_removals(&config, &["src/main.cpp".to_string()]),
            WorkOutcome::Done
        );
        assert!(
            !header_dialect_is_cpp(&config.db_path),
            "deleting the last C++ file must un-flip the header dialect"
        );
    }

    #[test]
    fn removal_unflip_gate_requires_flag_and_cpp_path() {
        let cpp = vec!["src/a.cpp".to_string()];
        let mixed = vec!["x.c".to_string(), "y.hpp".to_string()];
        let c_and_cuda = vec!["src/a.c".to_string(), "k.cu".to_string()];

        assert!(removed_paths_may_unflip_header(true, &cpp));
        assert!(removed_paths_may_unflip_header(true, &mixed));
        // .cu never flipped the dialect, so removing it can't un-flip it.
        assert!(!removed_paths_may_unflip_header(true, &c_and_cuda));
        assert!(!removed_paths_may_unflip_header(false, &cpp));
        assert!(!removed_paths_may_unflip_header(true, &[]));
    }

    #[test]
    fn header_dialect_probe_never_creates_a_missing_db() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let db_path = root.join(".codesage").join("index.db");

        assert!(!header_dialect_is_cpp(&db_path));
        assert!(
            !db_path.exists(),
            "the dialect probe must not resurrect a reset index"
        );
    }

    #[test]
    fn process_ready_requests_header_rederive_when_cpp_file_vanishes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let src = "int main() { return 0; }\n";
        std::fs::write(root.join("main.cpp"), src).unwrap();
        let config = test_config(root);
        let filter = WatchFilter::new(root, &config.exclude_patterns).unwrap();
        let mut embedder = EmbedderHandle::new(None);

        let db = Database::open(&config.db_path).unwrap();
        index_files(
            root,
            &db,
            &[FileInfo {
                path: "main.cpp".to_string(),
                language: codesage_protocol::Language::Cpp,
                content_hash: content_hash(src.as_bytes()),
            }],
            false,
        )
        .unwrap();
        drop(db);

        let mut pending = HashMap::new();
        let mut currently_indexing = HashSet::new();
        let mut recheck_queue = HashSet::new();

        // Still present and non-empty: a plain reindex must not request a
        // re-derivation.
        let rederive = process_ready(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &filter,
            &mut embedder,
            true,
            vec![PathBuf::from("main.cpp")],
        );
        assert!(!rederive);

        // Vanished (rename reported as Modify): the Done pass purged the
        // rows, so the caller must re-derive the header dialect.
        std::fs::remove_file(root.join("main.cpp")).unwrap();
        let rederive = process_ready(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &filter,
            &mut embedder,
            true,
            vec![PathBuf::from("main.cpp")],
        );
        assert!(rederive, "a purged C++ file must trigger re-derivation");
        assert!(!header_dialect_is_cpp(&config.db_path));
    }

    #[test]
    fn bulk_cooldown_arms_on_done_only() {
        let now = Instant::now();
        assert_eq!(
            bulk_cooldown_after(WorkOutcome::Done, now),
            Some(now + BULK_COOLDOWN)
        );
        assert_eq!(bulk_cooldown_after(WorkOutcome::Skipped, now), None);
        assert_eq!(bulk_cooldown_after(WorkOutcome::Failed, now), None);
    }

    #[test]
    fn bulk_cooldown_window_boundaries() {
        let now = Instant::now();
        let cd = bulk_cooldown_after(WorkOutcome::Done, now);
        assert!(in_bulk_cooldown(cd, now));
        assert!(in_bulk_cooldown(
            cd,
            now + BULK_COOLDOWN - Duration::from_millis(1)
        ));
        assert!(!in_bulk_cooldown(cd, now + BULK_COOLDOWN));
        assert!(!in_bulk_cooldown(None, now));
    }

    #[test]
    fn backlog_replay_during_cooldown_defers_bulk_and_keeps_events() {
        // Sequence: a Done bulk pass arms the cooldown; the burst's queued
        // events replay and re-cross the threshold during it. The Skipped
        // disposition must keep every replayed event pending (nothing lost)
        // and schedule exactly one catch-up pass at cooldown expiry — not a
        // debounce out, which would land inside the cooldown and defeat it.
        let debounce = Duration::from_millis(500);
        let done_at = Instant::now();
        let cd = bulk_cooldown_after(WorkOutcome::Done, done_at);

        let replay_at = done_at + Duration::from_millis(10);
        assert!(in_bulk_cooldown(cd, replay_at));

        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("late-edit.rs"), replay_at);
        let mut removed = vec!["gone.rs".to_string()];

        let retry = apply_bulk_outcome(
            WorkOutcome::Skipped,
            &mut pending,
            &mut removed,
            replay_at,
            debounce,
            cd,
        );

        assert_eq!(retry, Some(done_at + BULK_COOLDOWN));
        assert!(pending.contains_key(Path::new("late-edit.rs")));
        assert_eq!(removed, vec!["gone.rs".to_string()]);
    }

    #[test]
    fn threshold_crossing_during_cooldown_never_arms_a_bulk_before_expiry() {
        // A Done bulk pass at t=0 arms the 3s cooldown; a threshold crossing
        // at t=0.1 must not produce a bulk pass before t=3 — the catch-up is
        // clamped to cooldown expiry, and the restamped pending set stays
        // drain-unready until then so the per-file path can't race it either.
        let debounce = Duration::from_millis(1000);
        let done_at = Instant::now();
        let cd = bulk_cooldown_after(WorkOutcome::Done, done_at);

        let cross_at = done_at + Duration::from_millis(100);
        assert!(in_bulk_cooldown(cd, cross_at));

        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("a.rs"), cross_at);
        let mut removed: Vec<String> = Vec::new();

        let retry = apply_bulk_outcome(
            WorkOutcome::Skipped,
            &mut pending,
            &mut removed,
            cross_at,
            debounce,
            cd,
        )
        .expect("catch-up must be scheduled");

        assert_eq!(retry, done_at + BULK_COOLDOWN);
        assert!(
            !in_bulk_cooldown(cd, retry),
            "the catch-up must fire only once the cooldown has expired"
        );
        assert!(
            compute_ready(&pending, retry - Duration::from_millis(1), debounce).is_empty(),
            "pending must stay drain-unready until the catch-up fires"
        );
        assert_eq!(compute_ready(&pending, retry, debounce).len(), 1);
    }

    #[test]
    fn watch_error_schedules_bulk_catchup_and_retains_state() {
        // A forwarded notify error must arm bulk_retry_at (the loop's
        // catch-up path) while losing none of the queued work.
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let stale = now - Duration::from_secs(10);
        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("a.rs"), stale);
        let mut removed = vec!["gone.rs".to_string()];

        let retry = schedule_watch_error_catchup(&mut pending, &mut removed, now, debounce, None);

        assert_eq!(retry, Some(now + debounce));
        assert!(pending.contains_key(Path::new("a.rs")));
        assert_eq!(removed, vec!["gone.rs".to_string()]);
        // Stamps refreshed: the per-file drain won't race the catch-up pass.
        assert!(compute_ready(&pending, now, debounce).is_empty());
    }

    #[test]
    fn watch_error_catchup_schedules_even_with_empty_queues() {
        // An overflow can lose the only events describing a change, so the
        // rescan must be scheduled even when nothing is queued locally.
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let mut pending = HashMap::new();
        let mut removed: Vec<String> = Vec::new();

        let retry = schedule_watch_error_catchup(&mut pending, &mut removed, now, debounce, None);

        assert_eq!(retry, Some(now + debounce));
    }

    #[test]
    fn watch_error_catchup_during_cooldown_defers_to_expiry() {
        // Sequence: a Done bulk pass arms the cooldown; a notify error
        // (queue overflow) arrives during it. The correctness rescan is
        // never dropped — it's armed on bulk_retry_at — but it's clamped to
        // cooldown expiry like any other catch-up, so an overflow can't be
        // used to defeat the cooldown either.
        let debounce = Duration::from_millis(500);
        let done_at = Instant::now();
        let cd = bulk_cooldown_after(WorkOutcome::Done, done_at);

        let err_at = done_at + Duration::from_millis(10);
        assert!(in_bulk_cooldown(cd, err_at));

        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("late-edit.rs"), err_at);
        let mut removed: Vec<String> = Vec::new();

        let retry = schedule_watch_error_catchup(&mut pending, &mut removed, err_at, debounce, cd);

        let retry_at = retry.expect("catch-up must be scheduled during cooldown");
        assert_eq!(retry_at, done_at + BULK_COOLDOWN);
        assert!(
            !in_bulk_cooldown(cd, retry_at),
            "the rescan fires at cooldown expiry, not inside the window"
        );
        assert!(pending.contains_key(Path::new("late-edit.rs")));
    }

    #[test]
    fn watch_enabled_honors_config_and_marker() {
        let dir = std::env::temp_dir().join(format!("cs-watch-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join(".codesage"));

        // CODESAGE_WATCH is process-global; only assert the config + marker
        // paths here so the test stays independent of the ambient env.
        if std::env::var("CODESAGE_WATCH").is_err() {
            assert!(watch_enabled(&dir, None));
            assert!(watch_enabled(&dir, Some(true)));
        }
        assert!(!watch_enabled(&dir, Some(false)));

        std::fs::write(watch_disabled_path(&dir), "").unwrap();
        assert!(!watch_enabled(&dir, None));
        std::fs::remove_file(watch_disabled_path(&dir)).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
