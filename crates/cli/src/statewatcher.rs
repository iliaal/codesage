use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use codesage_embed::config::EmbeddingConfig;
use codesage_embed::model::Embedder;
use codesage_graph::{
    SemanticFingerprint, index_files, remove_files, semantic_index_files, semantic_remove_files,
};
use codesage_parser::detect::{
    detect_language, detect_language_with_dialect, is_unambiguous_cpp_extension,
};
use codesage_parser::discover::{WatchFilter, content_hash};
use codesage_protocol::FileInfo;
use codesage_storage::Database;
use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::lockfile;

/// Per-path quiet window before a saved file is re-indexed. Thirty seconds,
/// not one: an editor saves the same file every few seconds during active
/// work, and each save used to cost a full structural parse plus a GPU
/// embedding pass — 714 re-embeds of whole files (154,900 chunks) in one
/// day of editing one project. The window restarts on every event, so a
/// file is indexed once the author pauses, and every path that fell quiet in
/// the same poll is embedded in one batched call.
const DEFAULT_DEBOUNCE_MS: u64 = 30_000;
/// Longest a ready batch stays deferred under backpressure before it runs
/// anyway. Load that never drops (a machine that is simply busy) must not
/// turn into an index that never updates.
const BACKPRESSURE_MAX_DEFER: Duration = Duration::from_secs(15 * 60);
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
    /// Defer indexing while the host is busy (see [`backpressure_reason`]).
    /// Tests turn it off so a loaded CI runner cannot make a drain look like
    /// a lock skip.
    pub backpressure: bool,
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
        let _ = crate::fsguard::remove_state_file(&self.0);
    }
}

pub fn run_statewatcher(mut config: StateWatcherConfig) -> Result<()> {
    // macOS FSEvents reports canonical paths (symlinks resolved, /var =>
    // /private/var); a symlink-spelled root would fail every strip_prefix
    // below and silently drop all events. Resolve once so registration,
    // filtering, and event mapping agree on one spelling.
    config.project_root = canonical_root(&config.project_root);

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
    // Root-relative prefixes of vanished non-source paths (directory moves
    // and deletes). A directory moved out of the tree emits one event for
    // the directory itself and none for the files under it; the affected
    // rows are resolved from the index at removal time.
    let mut removed_prefixes: Vec<String> = Vec::new();
    let mut batch_event_times: Vec<Instant> = Vec::new();
    let mut currently_indexing: HashSet<PathBuf> = HashSet::new();
    let mut recheck_queue: HashSet<PathBuf> = HashSet::new();
    let mut bulk_retry_at: Option<Instant> = None;
    let mut bulk_cooldown_until: Option<Instant> = None;
    let mut removal_retry_at: Option<Instant> = None;
    let mut removal_fail_count: u32 = 0;
    let mut last_activity = Instant::now();
    let mut header_is_cpp = header_dialect_is_cpp(&config.db_path);
    let mut deferred_since: Option<Instant> = None;

    tracing::info!(
        root = %config.project_root.display(),
        debounce_ms = config.debounce_ms,
        mode = ?config.mode,
        "statewatcher started"
    );

    let exit_reason = loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(Ok(event)) => {
                // Dropped-event signals arrive as an Ok event flagged Rescan
                // (kind Other; inotify attaches no path, FSEvents does),
                // never through the Err arm: inotify surfaces queue overflow
                // this way, FSEvents its must-scan-subdirs flag. Same
                // disposition as a backend error — reconcile, or the dropped
                // edits stay unindexed.
                if event.need_rescan() {
                    tracing::warn!(
                        "watch backend requested rescan (events dropped); \
                         scheduling bulk reconciliation pass"
                    );
                    bulk_retry_at = schedule_watch_error_catchup(
                        &mut pending,
                        &mut removed_paths,
                        Instant::now(),
                        debounce,
                        bulk_cooldown_until,
                    );
                    continue;
                }

                for path in &event.paths {
                    // A directory that appeared needs adoption: the root is
                    // watched non-recursively, so new top-level trees aren't
                    // covered until we recurse into them. A rename-in arrives
                    // as Modify(Name), not Create, and no backend replays
                    // files that landed before the watch registered — scan
                    // the tree and queue what's already there.
                    if path.is_dir() && is_dir_adoption_kind(&event.kind) {
                        maybe_watch_new_dir(&mut watcher, &config.project_root, path, &filter);
                        match scan_dir_source_files(
                            &config.project_root,
                            path,
                            &filter,
                            BATCH_THRESHOLD,
                        ) {
                            DirScan::Files(files) => {
                                let now = Instant::now();
                                for rel in files {
                                    let rel_str = rel.to_string_lossy().to_string();
                                    removed_paths.retain(|p| *p != rel_str);
                                    if let std::collections::hash_map::Entry::Vacant(e) =
                                        pending.entry(rel)
                                    {
                                        e.insert(now);
                                        batch_event_times.push(now);
                                    }
                                }
                            }
                            // Too large to enqueue file by file — one bulk
                            // reconciliation pass covers the whole tree.
                            DirScan::OverThreshold => {
                                bulk_retry_at = schedule_watch_error_catchup(
                                    &mut pending,
                                    &mut removed_paths,
                                    Instant::now(),
                                    debounce,
                                    bulk_cooldown_until,
                                );
                            }
                        }
                        continue;
                    }

                    let rel = match path.strip_prefix(&config.project_root) {
                        Ok(p) => p.to_path_buf(),
                        Err(_) => continue,
                    };

                    // Any vanished path may have been a directory holding
                    // indexed files: a rename-out is one Modify(Name(From))
                    // for the directory, nothing for its contents, and a
                    // directory can be named like a source file. Queue it as
                    // a prefix; the removal pass resolves affected rows from
                    // the index, and a plain file prefix expands to nothing
                    // extra.
                    if matches!(
                        event.kind,
                        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
                    ) && !rel.as_os_str().is_empty()
                        && !path.exists()
                        && !filter.is_ignored(path, true)
                    {
                        let rel_str = rel.to_string_lossy().to_string();
                        if !removed_prefixes.contains(&rel_str) {
                            removed_prefixes.push(rel_str);
                        }
                    }

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
                    let outcome = run_bulk_guarded(&config, &mut embedder, &mut deferred_since);
                    bulk_cooldown_until = bulk_cooldown_after(outcome, Instant::now());
                    if outcome == WorkOutcome::Done {
                        // Unconditional: a bulk pass can also delete the last
                        // C++ file, which must un-flip header parsing without
                        // a watcher restart.
                        header_is_cpp = header_dialect_is_cpp(&config.db_path);
                        // The bulk pass purged every orphaned row, which
                        // covers any queued directory prefixes.
                        removed_prefixes.clear();
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
            if !removed_paths.is_empty() || !removed_prefixes.is_empty() {
                let _ = handle_removals(&config, &removed_paths, &removed_prefixes);
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
            let outcome = run_bulk_guarded(&config, &mut embedder, &mut deferred_since);
            bulk_cooldown_until = bulk_cooldown_after(outcome, Instant::now());
            if outcome == WorkOutcome::Done {
                header_is_cpp = header_dialect_is_cpp(&config.db_path);
                removed_prefixes.clear();
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
        if removed_paths.is_empty() && removed_prefixes.is_empty() {
            removal_retry_at = None;
            removal_fail_count = 0;
        } else if removal_retry_at.is_none_or(|at| Instant::now() >= at) {
            // A prefix purge has unknown contents, so it must conservatively
            // re-derive the header dialect on success.
            let may_unflip = removed_paths_may_unflip_header(header_is_cpp, &removed_paths)
                || (header_is_cpp && !removed_prefixes.is_empty());
            let outcome = handle_removals(&config, &removed_paths, &removed_prefixes);
            if outcome == WorkOutcome::Done && may_unflip {
                header_is_cpp = header_dialect_is_cpp(&config.db_path);
            }
            removal_retry_at = apply_removal_outcome(
                outcome,
                &mut removed_paths,
                &mut removed_prefixes,
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
            &mut deferred_since,
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
            || !removed_prefixes.is_empty()
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
    deferred_since: &mut Option<Instant>,
) -> bool {
    let ready = compute_ready(pending, Instant::now(), debounce);
    if ready.is_empty() {
        return false;
    }
    // Backpressure: a ready batch waits while the host is busy, re-armed for
    // one more debounce window, until the pressure lifts or the deferral cap
    // says a stale index is now the worse outcome.
    if config.backpressure {
        let reason = backpressure_reason(&config.project_root);
        if should_defer(reason.as_deref(), deferred_since, Instant::now()) {
            let now = Instant::now();
            for path in ready {
                pending.insert(path, now);
            }
            return false;
        }
    }
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
    // Files whose structural pass landed and whose semantic rows are stale.
    // They are embedded together below: one lock, one DB handle, and one
    // model call per commit batch instead of one per file.
    let mut semantic_todo: Vec<(PathBuf, FileInfo)> = Vec::new();
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
        let mut stale = None;
        let outcome = reindex_one(config, &path, embedder.enabled(), header_is_cpp, &mut stale);
        currently_indexing.remove(&path);
        if let Some(info) = stale {
            semantic_todo.push((path.clone(), info));
        }

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

    if !semantic_todo.is_empty() {
        let files: Vec<FileInfo> = semantic_todo.iter().map(|(_, f)| f.clone()).collect();
        if semantic_reindex_batch(config, embedder, &files) == WorkOutcome::Skipped {
            // Lock contention or a busy database: the structural rows landed,
            // the semantic rows did not. Re-queue the paths; the next drain
            // finds the structural hash fresh and only the semantic one stale.
            let now = Instant::now();
            for (path, _) in semantic_todo {
                pending.insert(path, now);
            }
        }
    }
    rederive_header
}

/// Embed every file in `files` in one pass under one lock. Chunks whose text
/// is unchanged keep their stored vectors (see `codesage_graph::semantic`),
/// so a batch of saved files costs one model call per fifty files for the
/// chunks that actually changed.
fn semantic_reindex_batch(
    config: &StateWatcherConfig,
    embedder: &mut EmbedderHandle,
    files: &[FileInfo],
) -> WorkOutcome {
    let Some(emb_arc) = embedder.get() else {
        return WorkOutcome::Failed;
    };
    let _lock = match lockfile::try_acquire(&config.project_root) {
        Ok(lockfile::LockOutcome::Acquired(lock)) => Some(lock),
        Ok(lockfile::LockOutcome::AlreadyHeld) => {
            tracing::debug!(
                files = files.len(),
                "deferring semantic reindex: index lock held by another process"
            );
            return WorkOutcome::Skipped;
        }
        Err(e) => {
            tracing::warn!(error = %e, "acquiring index lock for semantic reindex");
            return WorkOutcome::Failed;
        }
    };
    let mut emb = emb_arc.lock();
    let db = match Database::open_for_model(&config.db_path, &config.embed_config.model, emb.dim())
    {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!(error = %e, "opening DB for semantic reindex");
            return WorkOutcome::Failed;
        }
    };
    let fingerprint = SemanticFingerprint::compute(&config.embed_config, emb.dim());
    match semantic_index_files(
        &config.project_root,
        &db,
        &mut *emb,
        files,
        &fingerprint,
        false,
    ) {
        Ok(stats) => {
            if stats.files_processed > 0 {
                tracing::info!(
                    files = stats.files_processed,
                    chunks = stats.chunks_created,
                    embedded = stats.chunks_created.saturating_sub(stats.chunks_reused),
                    reused = stats.chunks_reused,
                    "semantic reindex"
                );
            }
            WorkOutcome::Done
        }
        Err(e) if is_retryable_db_error(&e) => {
            tracing::debug!(error = %e, "semantic reindex deferred: database busy, will retry");
            WorkOutcome::Skipped
        }
        Err(e) => {
            tracing::warn!(error = %e, "semantic reindex failed");
            WorkOutcome::Failed
        }
    }
}

/// Present on disk with at least one byte. A vanished or emptied path had
/// its index rows purged by `reindex_one`, which is what the header-dialect
/// re-derivation gate cares about.
fn file_has_content(abs: &Path) -> bool {
    std::fs::metadata(abs).is_ok_and(|m| m.len() > 0)
}

/// Structural re-index of one saved file. When `semantic_enabled` and the
/// file's semantic rows are stale, `stale_semantic` receives its `FileInfo`
/// so the caller can embed it together with the rest of the ready batch.
fn reindex_one(
    config: &StateWatcherConfig,
    rel: &Path,
    semantic_enabled: bool,
    header_is_cpp: bool,
    stale_semantic: &mut Option<FileInfo>,
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

    let file_info = FileInfo {
        path: rel_str.clone(),
        language: lang,
        content_hash: hash.clone(),
    };

    if semantic_enabled && !semantic_hash_is_fresh(config, &rel_str, &hash) {
        *stale_semantic = Some(file_info.clone());
    }

    if structural_hash_is_fresh(config, &rel_str, &hash) {
        return WorkOutcome::Done;
    }

    let _lock = match lockfile::try_acquire(&config.project_root) {
        Ok(lockfile::LockOutcome::Acquired(lock)) => Some(lock),
        Ok(lockfile::LockOutcome::AlreadyHeld) => {
            tracing::debug!(
                path = %rel_str,
                "deferring reindex: index lock held by another process"
            );
            *stale_semantic = None;
            return WorkOutcome::Skipped;
        }
        Err(e) => {
            tracing::warn!(error = %e, "acquiring index lock");
            *stale_semantic = None;
            return WorkOutcome::Failed;
        }
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
                    *stale_semantic = None;
                    return WorkOutcome::Skipped;
                }
                Err(e) => {
                    tracing::warn!(path = %rel_str, error = %e, "structural reindex failed");
                    *stale_semantic = None;
                    return WorkOutcome::Failed;
                }
            }
        }
        Err(e) => {
            tracing::warn!(path = %rel_str, error = %e, "opening DB for structural reindex");
            *stale_semantic = None;
            return WorkOutcome::Failed;
        }
    }

    WorkOutcome::Done
}

fn structural_hash_is_fresh(
    config: &StateWatcherConfig,
    rel_str: &str,
    content_hash: &str,
) -> bool {
    let Ok(db) = Database::open(&config.db_path) else {
        return false;
    };
    matches!(db.get_file_hash(rel_str), Ok(Some(stored)) if stored == content_hash)
}

fn semantic_hash_is_fresh(config: &StateWatcherConfig, rel_str: &str, content_hash: &str) -> bool {
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

fn handle_removals(
    config: &StateWatcherConfig,
    paths: &[String],
    prefixes: &[String],
) -> WorkOutcome {
    if paths.is_empty() && prefixes.is_empty() {
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

    // Resolve queued directory prefixes into concrete indexed paths under
    // the lock: a concurrent indexer can commit new rows under a prefix
    // right up until the lock is ours, and a pre-lock snapshot would miss
    // them while the caller clears the prefix.
    let mut candidates: Vec<String> = paths.to_vec();
    if !prefixes.is_empty() {
        let Some(expanded) = expand_removed_prefixes(&config.db_path, prefixes) else {
            return WorkOutcome::Failed;
        };
        for p in expanded {
            if !candidates.contains(&p) {
                candidates.push(p);
            }
        }
    }

    // A path that exists on disk was re-created after its removal was queued
    // (git checkout back-and-forth, atomic-save editors). Deleting its rows
    // would silently drop a live file's symbols/chunks for the session, so
    // only purge paths that are genuinely gone.
    let to_remove: Vec<String> = candidates
        .into_iter()
        .filter(|p| !config.project_root.join(p).exists())
        .collect();
    if to_remove.is_empty() {
        return WorkOutcome::Done;
    }

    purge_index_rows(config, &to_remove)
}

fn purge_index_rows(config: &StateWatcherConfig, paths: &[String]) -> WorkOutcome {
    // A reset deleted index.db: there are no rows to purge, and the open
    // must not recreate the database (warm-state existence checks would
    // pass again).
    if !config.db_path.exists() {
        return WorkOutcome::Done;
    }
    let db = match Database::open_existing(&config.db_path) {
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
/// when (if at all) the removal should be retried. `Done` clears the paths
/// and prefixes and resets the failure counter. `Skipped` (transient lock
/// contention) keeps the state and reschedules without counting toward the
/// give-up ceiling. `Failed` counts toward the ceiling; after
/// `MAX_REMOVAL_FAILURES` consecutive hard failures it gives up — clearing
/// the state so the loop stops reporting activity and the watcher can idle
/// out instead of pinning the pooled model.
fn apply_removal_outcome(
    outcome: WorkOutcome,
    removed_paths: &mut Vec<String>,
    removed_prefixes: &mut Vec<String>,
    fail_count: &mut u32,
    now: Instant,
    debounce: Duration,
) -> Option<Instant> {
    match outcome {
        WorkOutcome::Done => {
            removed_paths.clear();
            removed_prefixes.clear();
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
                    prefixes = ?removed_prefixes,
                    "giving up on index removals after repeated hard failures; \
                     index may retain stale rows until the next full reindex"
                );
                removed_paths.clear();
                removed_prefixes.clear();
                *fail_count = 0;
                None
            } else {
                Some(now + debounce)
            }
        }
    }
}

/// [`run_bulk_incremental`] behind the same backpressure gate as the per-file
/// drain: a deferred pass reports `Skipped`, which re-arms the retry.
fn run_bulk_guarded(
    config: &StateWatcherConfig,
    embedder: &mut EmbedderHandle,
    deferred_since: &mut Option<Instant>,
) -> WorkOutcome {
    if config.backpressure {
        let reason = backpressure_reason(&config.project_root);
        if should_defer(reason.as_deref(), deferred_since, Instant::now()) {
            return WorkOutcome::Skipped;
        }
    }
    run_bulk_incremental(config, embedder)
}

/// Decide whether a ready batch waits. `reason` is `Some` while the host is
/// under pressure; `deferred_since` tracks the first deferral of the current
/// streak so the wait is bounded by [`BACKPRESSURE_MAX_DEFER`]. Returns
/// `true` to defer.
fn should_defer(reason: Option<&str>, deferred_since: &mut Option<Instant>, now: Instant) -> bool {
    let Some(reason) = reason else {
        *deferred_since = None;
        return false;
    };
    let since = *deferred_since.get_or_insert(now);
    if now.duration_since(since) >= BACKPRESSURE_MAX_DEFER {
        tracing::info!(
            reason,
            deferred_for = ?now.duration_since(since),
            "backpressure cap reached; indexing despite load"
        );
        *deferred_since = None;
        return false;
    }
    tracing::debug!(reason, "deferring reindex under backpressure");
    true
}

/// Why indexing should wait right now, or `None`. Three signals, each cheap:
/// a git operation in flight (`index.lock` in the repository's git dir — a
/// checkout or rebase is about to rewrite the files we would index), a
/// one-minute load average above the CPU count, or a `cargo`/`rustc`/`pytest`
/// process running with its working directory under the project root. A
/// stale embedding for a few minutes costs less than contending with any of
/// them.
fn backpressure_reason(root: &Path) -> Option<String> {
    if git_index_lock_present(root) {
        return Some("git index.lock present".to_string());
    }
    let ncpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if let Some(load) = load_average_1m()
        && load > ncpu as f64
    {
        return Some(format!("load average {load:.1} exceeds {ncpu} cpus"));
    }
    build_process_under(root).map(|name| format!("{name} running under the project root"))
}

/// Whether `<git dir>/index.lock` exists for `root`. `.git` may be a
/// directory or, in a linked worktree, a `gitdir: <path>` pointer file.
fn git_index_lock_present(root: &Path) -> bool {
    let dot_git = root.join(".git");
    let git_dir = match std::fs::metadata(&dot_git) {
        Ok(meta) if meta.is_dir() => dot_git,
        Ok(_) => {
            let Ok(pointer) = std::fs::read_to_string(&dot_git) else {
                return false;
            };
            let Some(rest) = pointer.trim().strip_prefix("gitdir:") else {
                return false;
            };
            let target = PathBuf::from(rest.trim());
            if target.is_absolute() {
                target
            } else {
                root.join(target)
            }
        }
        Err(_) => return false,
    };
    git_dir.join("index.lock").exists()
}

/// One-minute load average from `/proc/loadavg`; `None` off Linux or when
/// unreadable.
fn load_average_1m() -> Option<f64> {
    let raw = std::fs::read_to_string("/proc/loadavg").ok()?;
    raw.split_whitespace().next()?.parse().ok()
}

/// Names a build or test process whose working directory is `root` or below
/// it, when one exists. Scans `/proc` by `comm`, so it costs one directory
/// walk per ready batch and nothing between batches. Only the process names
/// that saturate a machine for minutes are recognised; an `ls` under the
/// root is not pressure.
fn build_process_under(root: &Path) -> Option<String> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let root = canonical_root(root);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let proc_dir = entry.path();
        let Ok(comm) = std::fs::read_to_string(proc_dir.join("comm")) else {
            continue;
        };
        let comm = comm.trim();
        if !is_build_process_name(comm) {
            continue;
        }
        let Ok(cwd) = std::fs::read_link(proc_dir.join("cwd")) else {
            continue;
        };
        if cwd.starts_with(&root) {
            return Some(comm.to_string());
        }
    }
    None
}

fn is_build_process_name(comm: &str) -> bool {
    matches!(
        comm,
        "cargo" | "rustc" | "pytest" | "py.test" | "pytest-xdist"
    ) || comm.starts_with("cargo-")
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
        let fingerprint = SemanticFingerprint::compute(&config.embed_config, emb.dim());
        if let Err(e) = codesage_graph::semantic_incremental_index(
            &config.project_root,
            &db,
            &mut *emb,
            &config.exclude_patterns,
            &fingerprint,
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

/// Event kinds that can introduce a directory needing adoption. A rename
/// into the tree arrives as `Modify(Name(To))` (inotify MOVED_TO), never
/// as `Create` — matching `Create` alone would leave a `mv dir project/`
/// tree unwatched forever.
fn is_dir_adoption_kind(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

/// Cap on directory entries examined during an adoption scan. The scan runs
/// synchronously on the event loop and a renamed-in tree can be arbitrarily
/// large; past this, one bulk reconciliation pass is cheaper than walking.
const ADOPTION_SCAN_ENTRY_CAP: usize = 2048;

/// Outcome of an adoption scan over a newly appeared directory.
#[derive(Debug, PartialEq, Eq)]
enum DirScan {
    /// Every non-ignored source file under the directory, root-relative.
    Files(Vec<PathBuf>),
    /// The tree holds at least `file_threshold` source files (or blew the
    /// entry cap): enqueueing per file would cross the bulk threshold
    /// anyway, so the caller should schedule one bulk pass instead.
    OverThreshold,
}

/// Non-ignored source files under a newly appeared directory, as
/// root-relative paths. Watch registration races population: no backend
/// replays files that landed before the watch existed, and a directory
/// renamed in arrives fully populated with no per-file events at all.
fn scan_dir_source_files(
    root: &Path,
    dir: &Path,
    filter: &WatchFilter,
    file_threshold: usize,
) -> DirScan {
    let mut found = Vec::new();
    if dir.strip_prefix(root).is_err() || filter.is_ignored(dir, true) {
        return DirScan::Files(found);
    }
    let mut examined = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            examined += 1;
            if examined > ADOPTION_SCAN_ENTRY_CAP {
                return DirScan::OverThreshold;
            }
            let path = entry.path();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if !filter.is_ignored(&path, true) {
                    stack.push(path);
                }
            } else if let Ok(rel) = path.strip_prefix(root)
                && is_source_file(rel)
                && !filter.is_ignored(&path, false)
            {
                found.push(rel.to_path_buf());
                if found.len() >= file_threshold {
                    return DirScan::OverThreshold;
                }
            }
        }
    }
    DirScan::Files(found)
}

/// Indexed paths at or under any of the given root-relative prefixes.
/// `None` when the index exists but can't be read — the caller retains the
/// prefixes and retries. A missing database has nothing to purge, and the
/// probe must not recreate it (a reset deletes `index.db`).
fn expand_removed_prefixes(db_path: &Path, prefixes: &[String]) -> Option<Vec<String>> {
    if !db_path.exists() {
        return Some(Vec::new());
    }
    let db = Database::open_existing(db_path).ok()?;
    let all = db.all_file_paths().ok()?;
    Some(
        all.into_iter()
            .filter(|p| {
                prefixes.iter().any(|pre| {
                    p == pre || (p.starts_with(pre.as_str()) && p[pre.len()..].starts_with('/'))
                })
            })
            .collect(),
    )
}

/// The root's canonical spelling, or the given one when resolution fails
/// (the caller's spelling still works for a root that exists but can't be
/// canonicalized, e.g. permission-restricted ancestors).
fn canonical_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
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
    // `.codesage/` is repository-supplied content in a freshly cloned tree, so
    // `watch.status` may be a planted symlink pointing anywhere on the host.
    // Open with O_NOFOLLOW instead of `fs::write`: it fails with ELOOP rather
    // than truncating the link's target, and unlike an lstat-then-write check
    // it leaves no TOCTOU window.
    let mut file = crate::fsguard::create_no_follow(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    std::io::Write::write_all(&mut file, json.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn read_status(root: &Path) -> Option<WatchStatus> {
    let path = watch_status_path(root);
    let raw = crate::fsguard::read_state_to_string(&path).ok()?;
    let status: WatchStatus = serde_json::from_str(&raw).ok()?;
    // An abrupt daemon/process death leaves the status file behind without the
    // owning thread running its cleanup. Treat a status whose recorded pid is
    // gone as inactive, and prune the stale file.
    if !process_alive(status.pid) {
        let _ = crate::fsguard::remove_state_file(&path);
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
    !watch_disabled_marker_present(root)
}

/// Is a `watch.disabled` marker present at all?
///
/// lstat, not `exists()`: a dangling marker symlink reads as absent to
/// `exists()`, so the marker would look cleared to some callers while every
/// attempt to rewrite it fails. One helper keeps `watch_enabled` and
/// `watch status` reporting the same thing.
pub fn watch_disabled_marker_present(root: &Path) -> bool {
    // lstat the `.codesage` parent too: lstat on the full path still resolves
    // every component *before* the last, so a symlinked project dir would have
    // this report on an entry outside the tree.
    let dir = root.join(".codesage");
    if !std::fs::symlink_metadata(&dir)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return false;
    }
    std::fs::symlink_metadata(watch_disabled_path(root)).is_ok()
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

    #[cfg(unix)]
    #[test]
    fn marker_presence_ignores_a_symlinked_project_dir() {
        // lstat on the full path still resolves every component before the
        // last, so the `.codesage` parent needs its own check.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("watch.disabled"), b"").unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".codesage")).unwrap();

        assert!(
            !watch_disabled_marker_present(&root),
            "reported a marker that lives outside the project"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_status_refuses_a_symlinked_status_file() {
        // `.codesage/watch.status` ships with a cloned repo; `fs::write`
        // through a planted symlink truncates the target.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let victim = root.join("victim.key");
        std::fs::write(&victim, b"PRIVATE KEY").unwrap();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::os::unix::fs::symlink(&victim, watch_status_path(root)).unwrap();

        assert!(write_status(root, WatcherMode::Foreground).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"PRIVATE KEY");
    }

    #[cfg(unix)]
    #[test]
    fn write_status_writes_an_ordinary_status_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();

        write_status(root, WatcherMode::Foreground).unwrap();

        let status = read_status(root).expect("status must round-trip");
        assert_eq!(status.pid, std::process::id());
    }

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
            backpressure: false,
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
        let mut prefixes = vec!["gonedir".to_string()];
        let mut fails = 0;

        let retry = apply_removal_outcome(
            WorkOutcome::Skipped,
            &mut removed,
            &mut prefixes,
            &mut fails,
            now,
            debounce,
        );

        assert_eq!(retry, Some(now + debounce));
        assert_eq!(removed, vec!["gone.rs".to_string()]);
        assert_eq!(prefixes, vec!["gonedir".to_string()]);
        // Lock contention is not a hard failure — the give-up ceiling stays put.
        assert_eq!(fails, 0);
    }

    #[test]
    fn apply_removal_outcome_gives_up_after_ceiling() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let mut removed = vec!["gone.rs".to_string()];
        let mut prefixes = vec!["gonedir".to_string()];
        let mut fails = 0;

        // Every hard failure short of the ceiling retains the state and reschedules.
        for _ in 0..(MAX_REMOVAL_FAILURES - 1) {
            let retry = apply_removal_outcome(
                WorkOutcome::Failed,
                &mut removed,
                &mut prefixes,
                &mut fails,
                now,
                debounce,
            );
            assert_eq!(retry, Some(now + debounce));
            assert_eq!(removed.len(), 1);
            assert_eq!(prefixes.len(), 1);
        }

        // The ceiling hit: give up, drop the state so the loop stops reporting
        // activity and the watcher can idle out instead of pinning the model.
        let retry = apply_removal_outcome(
            WorkOutcome::Failed,
            &mut removed,
            &mut prefixes,
            &mut fails,
            now,
            debounce,
        );
        assert_eq!(retry, None);
        assert!(removed.is_empty());
        assert!(prefixes.is_empty());
        assert_eq!(fails, 0);
    }

    #[test]
    fn apply_removal_outcome_done_resets_failure_streak() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let mut removed = vec!["gone.rs".to_string()];
        let mut prefixes = vec!["gonedir".to_string()];
        let mut fails = 3;

        let retry = apply_removal_outcome(
            WorkOutcome::Done,
            &mut removed,
            &mut prefixes,
            &mut fails,
            now,
            debounce,
        );

        assert_eq!(retry, None);
        assert!(removed.is_empty());
        assert!(prefixes.is_empty());
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
        assert_eq!(handle_removals(&config, &paths, &[]), WorkOutcome::Skipped);
    }

    #[test]
    fn handle_removals_done_when_lock_free() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let config = test_config(root);

        // No index.db on a fresh path: nothing to purge, no-op success.
        let paths = vec!["gone.rs".to_string()];
        assert_eq!(handle_removals(&config, &paths, &[]), WorkOutcome::Done);
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
            handle_removals(&config, &["foo.rs".to_string()], &[]),
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

        let _held = hold_lock(root);
        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), false, false, &mut None),
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

        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), false, false, &mut None),
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

        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), false, false, &mut None),
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

        assert!(structural_hash_is_fresh(&config, "foo.rs", &hash));
        assert!(!semantic_hash_is_fresh(&config, "foo.rs", &hash));
    }

    #[test]
    fn reindex_one_reports_a_stale_semantic_file_for_the_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let src = "fn foo() {}\n";
        std::fs::write(root.join("foo.rs"), src).unwrap();
        let config = test_config(root);
        let hash = content_hash(src.as_bytes());

        // Semantic disabled: structural lands, nothing is reported.
        let mut stale = None;
        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), false, false, &mut stale),
            WorkOutcome::Done
        );
        assert!(stale.is_none());
        assert!(structural_hash_is_fresh(&config, "foo.rs", &hash));

        // Semantic enabled, structural already fresh: no lock is taken (the
        // held lock would otherwise force Skipped) and the file is handed to
        // the batch because its semantic rows are stale.
        let _held = hold_lock(root);
        let mut stale = None;
        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), true, false, &mut stale),
            WorkOutcome::Done
        );
        let info = stale.expect("stale semantic file must be reported");
        assert_eq!(info.path, "foo.rs");
        assert_eq!(info.content_hash, hash);
        drop(_held);

        // Once the semantic hash is recorded, nothing is reported.
        let semantic_db = Database::open_for_model(
            &config.db_path,
            &config.embed_config.model,
            codesage_storage::db::DEFAULT_EMBEDDING_DIM,
        )
        .unwrap();
        semantic_db
            .upsert_semantic_file_hash("foo.rs", &hash)
            .unwrap();
        drop(semantic_db);
        let mut stale = None;
        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), true, false, &mut stale),
            WorkOutcome::Done
        );
        assert!(stale.is_none());
    }

    #[test]
    fn reindex_one_withdraws_the_semantic_report_when_structural_is_skipped() {
        // A file whose structural rows never landed must not be embedded:
        // the chunk headers are built from symbols the structural pass writes.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::write(root.join("foo.rs"), "fn foo() {}\n").unwrap();
        let config = test_config(root);
        let _held = hold_lock(root);
        let mut stale = None;
        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), true, false, &mut stale),
            WorkOutcome::Skipped
        );
        assert!(stale.is_none());
    }

    #[test]
    fn should_defer_waits_under_pressure_until_the_cap() {
        let t0 = Instant::now();
        let mut since = None;
        assert!(!should_defer(None, &mut since, t0));
        assert!(since.is_none());

        assert!(should_defer(Some("busy"), &mut since, t0));
        assert_eq!(since, Some(t0));
        assert!(should_defer(
            Some("busy"),
            &mut since,
            t0 + Duration::from_secs(60)
        ));
        assert_eq!(since, Some(t0), "the streak keeps its first stamp");

        // Cap reached: run anyway and start a fresh streak next time.
        assert!(!should_defer(
            Some("busy"),
            &mut since,
            t0 + BACKPRESSURE_MAX_DEFER
        ));
        assert!(since.is_none());

        // Pressure lifting clears the streak too.
        assert!(should_defer(Some("busy"), &mut since, t0));
        assert!(!should_defer(None, &mut since, t0 + Duration::from_secs(1)));
        assert!(since.is_none());
    }

    #[test]
    fn git_index_lock_is_found_through_a_dir_and_a_worktree_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(!git_index_lock_present(root), "no .git at all");

        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert!(!git_index_lock_present(root));
        std::fs::write(root.join(".git/index.lock"), "").unwrap();
        assert!(git_index_lock_present(root));

        // Linked worktree: `.git` is a pointer file to the real git dir.
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let gitdir = tmp.path().join("real-gitdir");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        assert!(!git_index_lock_present(&wt));
        std::fs::write(gitdir.join("index.lock"), "").unwrap();
        assert!(git_index_lock_present(&wt));

        // Relative pointer resolves against the worktree root.
        let wt2 = tmp.path().join("wt2");
        std::fs::create_dir_all(&wt2).unwrap();
        std::fs::write(wt2.join(".git"), "gitdir: ../real-gitdir").unwrap();
        assert!(git_index_lock_present(&wt2));
    }

    #[test]
    fn build_process_names_are_the_long_running_ones() {
        for name in ["cargo", "rustc", "pytest", "py.test", "cargo-clippy"] {
            assert!(is_build_process_name(name), "{name}");
        }
        for name in ["ls", "git", "python3", "codesage", "vim"] {
            assert!(!is_build_process_name(name), "{name}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn build_process_under_finds_a_cargo_named_process_in_the_tree() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let sub = root.join("crates").join("x");
        std::fs::create_dir_all(&sub).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();

        // A process whose `comm` is `cargo`: a symlink to sleep named cargo.
        let fake_cargo = elsewhere.path().join("cargo");
        symlink("/bin/sleep", &fake_cargo).unwrap();

        let mut outside = std::process::Command::new(&fake_cargo)
            .arg("30")
            .current_dir(elsewhere.path())
            .spawn()
            .unwrap();
        // Give /proc time to reflect the exec'd comm.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            build_process_under(root),
            None,
            "cwd outside the root is not pressure"
        );

        let mut inside = std::process::Command::new(&fake_cargo)
            .arg("30")
            .current_dir(&sub)
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(build_process_under(root).as_deref(), Some("cargo"));

        let _ = inside.kill();
        let _ = inside.wait();
        let _ = outside.kill();
        let _ = outside.wait();
    }

    #[test]
    fn drain_pending_defers_ready_paths_under_backpressure() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/index.lock"), "").unwrap();
        std::fs::write(root.join("foo.rs"), "fn main() {}\n").unwrap();
        let mut config = test_config(root);
        config.backpressure = true;
        let filter = WatchFilter::new(root, &config.exclude_patterns).unwrap();
        let mut embedder = EmbedderHandle::new(None);

        let debounce = Duration::from_millis(100);
        let stale = Instant::now() - Duration::from_secs(10);
        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("foo.rs"), stale);
        let mut currently_indexing = HashSet::new();
        let mut recheck_queue = HashSet::new();
        let mut deferred_since = None;

        let rederive = drain_pending(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &filter,
            &mut embedder,
            false,
            debounce,
            &mut deferred_since,
        );
        assert!(!rederive);
        assert!(deferred_since.is_some(), "a deferral streak must start");
        let stamp = pending
            .get(Path::new("foo.rs"))
            .copied()
            .expect("path stays pending");
        assert!(stamp > stale, "re-armed for another debounce window");
        assert!(compute_ready(&pending, Instant::now(), debounce).is_empty());
        let db = Database::open(&config.db_path).unwrap();
        assert!(
            db.get_file_hash("foo.rs").unwrap().is_none(),
            "nothing may be indexed while git holds index.lock"
        );
        drop(db);

        // Pressure lifts: the next drain (after the debounce) indexes it.
        std::fs::remove_file(root.join(".git/index.lock")).unwrap();
        pending.insert(PathBuf::from("foo.rs"), stale);
        let mut config = config;
        // The load-average and process probes read the real host; disable
        // the gate once the deterministic signal is gone so this assertion
        // cannot depend on the runner's load.
        config.backpressure = false;
        drain_pending(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &filter,
            &mut embedder,
            false,
            debounce,
            &mut deferred_since,
        );
        let db = Database::open(&config.db_path).unwrap();
        assert!(db.get_file_hash("foo.rs").unwrap().is_some());
    }

    #[test]
    fn default_debounce_is_thirty_seconds_of_quiet() {
        assert_eq!(DEFAULT_DEBOUNCE_MS, 30_000);
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

        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), false, false, &mut None),
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

        let _held = hold_lock(root);
        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), false, false, &mut None),
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

        assert_eq!(
            reindex_one(&config, Path::new("api.h"), false, true, &mut None),
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

        assert_eq!(
            reindex_one(&config, Path::new("api.h"), false, false, &mut None),
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
            handle_removals(&config, &["src/main.cpp".to_string()], &[]),
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

    #[test]
    fn overflow_arrives_as_ok_event_with_rescan_flag() {
        // notify's inotify backend surfaces Q_OVERFLOW (and FSEvents its
        // must-scan-subdirs flag) as Ok(Event{kind: Other, flag: Rescan}),
        // never through the Err arm — the loop's catch-up must key on
        // need_rescan(), not only on forwarded errors.
        // Path attachment differs by backend (inotify none, FSEvents the
        // affected path); only the flag is the contract.
        let overflow = Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        assert!(overflow.need_rescan());
        assert!(!Event::new(EventKind::Other).need_rescan());
    }

    #[test]
    fn dir_adoption_matches_create_and_rename_events() {
        use notify::event::{CreateKind, DataChange, MetadataKind, RemoveKind, RenameMode};

        assert!(is_dir_adoption_kind(&EventKind::Create(CreateKind::Folder)));
        assert!(is_dir_adoption_kind(&EventKind::Create(CreateKind::File)));
        // inotify MOVED_TO: a directory renamed into the tree.
        assert!(is_dir_adoption_kind(&EventKind::Modify(ModifyKind::Name(
            RenameMode::To
        ))));
        assert!(is_dir_adoption_kind(&EventKind::Modify(ModifyKind::Name(
            RenameMode::Both
        ))));
        // Content edits and removals never introduce a directory.
        assert!(!is_dir_adoption_kind(&EventKind::Modify(ModifyKind::Data(
            DataChange::Content
        ))));
        assert!(!is_dir_adoption_kind(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::WriteTime)
        )));
        assert!(!is_dir_adoption_kind(&EventKind::Remove(
            RemoveKind::Folder
        )));
    }

    #[test]
    fn scan_dir_source_files_collects_nested_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("newdir/sub")).unwrap();
        std::fs::write(root.join("newdir/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("newdir/sub/b.go"), "package b\n").unwrap();
        std::fs::write(root.join("newdir/notes.md"), "n\n").unwrap();
        let filter = WatchFilter::new(root, &[]).unwrap();

        let DirScan::Files(mut found) =
            scan_dir_source_files(root, &root.join("newdir"), &filter, BATCH_THRESHOLD)
        else {
            panic!("small tree must stay under the threshold");
        };
        found.sort();
        assert_eq!(
            found,
            vec![
                PathBuf::from("newdir/a.rs"),
                PathBuf::from("newdir/sub/b.go")
            ]
        );
    }

    #[test]
    fn scan_dir_source_files_over_threshold_requests_bulk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("newdir")).unwrap();
        for i in 0..3 {
            std::fs::write(root.join(format!("newdir/f{i}.rs")), "fn f() {}\n").unwrap();
        }
        let filter = WatchFilter::new(root, &[]).unwrap();

        assert_eq!(
            scan_dir_source_files(root, &root.join("newdir"), &filter, 3),
            DirScan::OverThreshold
        );
        // One under the threshold still enumerates.
        assert!(matches!(
            scan_dir_source_files(root, &root.join("newdir"), &filter, 4),
            DirScan::Files(f) if f.len() == 3
        ));
    }

    #[test]
    fn scan_dir_source_files_skips_ignored_and_hidden_subtrees() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("newdir/vendor")).unwrap();
        std::fs::create_dir_all(root.join("newdir/.git")).unwrap();
        std::fs::write(root.join("newdir/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("newdir/vendor/v.rs"), "fn v() {}\n").unwrap();
        std::fs::write(root.join("newdir/.git/g.rs"), "fn g() {}\n").unwrap();
        let filter = WatchFilter::new(root, &["**/vendor/**".to_string()]).unwrap();

        assert_eq!(
            scan_dir_source_files(root, &root.join("newdir"), &filter, BATCH_THRESHOLD),
            DirScan::Files(vec![PathBuf::from("newdir/a.rs")])
        );
    }

    #[test]
    fn scan_dir_source_files_rejects_outside_root_and_ignored_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("x.rs"), "fn x() {}\n").unwrap();
        std::fs::write(root.join("target/t.rs"), "fn t() {}\n").unwrap();
        let filter = WatchFilter::new(&root, &["**/target/**".to_string()]).unwrap();

        assert_eq!(
            scan_dir_source_files(&root, &outside, &filter, BATCH_THRESHOLD),
            DirScan::Files(vec![])
        );
        assert_eq!(
            scan_dir_source_files(&root, &root.join("target"), &filter, BATCH_THRESHOLD),
            DirScan::Files(vec![])
        );
    }

    #[test]
    fn expand_removed_prefixes_matches_on_path_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let config = test_config(root);

        let db = Database::open(&config.db_path).unwrap();
        for path in ["foo/a.rs", "foo/bar/b.rs", "foobar/c.rs", "other.rs"] {
            db.upsert_file(&FileInfo {
                path: path.to_string(),
                language: codesage_protocol::Language::Rust,
                content_hash: "x".to_string(),
            })
            .unwrap();
        }
        drop(db);

        let mut expanded = expand_removed_prefixes(&config.db_path, &["foo".to_string()]).unwrap();
        expanded.sort();
        // "foobar/c.rs" shares the byte prefix but not the path boundary.
        assert_eq!(
            expanded,
            vec!["foo/a.rs".to_string(), "foo/bar/b.rs".to_string()]
        );

        assert!(
            expand_removed_prefixes(&config.db_path, &["nomatch".to_string()])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn expand_removed_prefixes_missing_db_is_empty_and_not_created() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let db_path = root.join(".codesage").join("index.db");

        let expanded = expand_removed_prefixes(&db_path, &["foo".to_string()]).unwrap();
        assert!(expanded.is_empty());
        assert!(
            !db_path.exists(),
            "the prefix expansion must not resurrect a reset index"
        );
    }

    #[test]
    fn handle_removals_expands_prefix_and_purges_descendants() {
        // A directory moved out of the tree queues one prefix; the removal
        // pass must resolve and purge every indexed row under it while
        // sparing files that exist on disk (re-created after the move).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::create_dir_all(root.join("keep")).unwrap();
        std::fs::write(root.join("keep/alive.rs"), "fn a() {}\n").unwrap();
        let config = test_config(root);

        let db = Database::open(&config.db_path).unwrap();
        for path in ["gone/a.rs", "gone/sub/b.rs", "keep/alive.rs"] {
            db.upsert_file(&FileInfo {
                path: path.to_string(),
                language: codesage_protocol::Language::Rust,
                content_hash: "x".to_string(),
            })
            .unwrap();
        }
        drop(db);

        assert_eq!(
            handle_removals(&config, &[], &["gone".to_string(), "keep".to_string()]),
            WorkOutcome::Done
        );

        let db = Database::open(&config.db_path).unwrap();
        assert!(db.get_file_hash("gone/a.rs").unwrap().is_none());
        assert!(db.get_file_hash("gone/sub/b.rs").unwrap().is_none());
        assert!(
            db.get_file_hash("keep/alive.rs").unwrap().is_some(),
            "a file present on disk must survive a prefix purge"
        );
    }

    #[test]
    fn handle_removals_prefix_deferred_when_lock_held() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let config = test_config(root);

        // The lock is checked before any expansion, so a held lock defers
        // the whole pass and the caller keeps the prefixes queued.
        let _held = hold_lock(root);
        assert_eq!(
            handle_removals(&config, &[], &["gone".to_string()]),
            WorkOutcome::Skipped
        );
    }

    #[test]
    fn purge_missing_db_is_done_and_not_recreated() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        let config = test_config(root);

        // "gone.rs" is absent on disk, so the purge path runs — against a
        // reset (missing) index.db it must succeed as a no-op without
        // resurrecting the database.
        assert_eq!(
            handle_removals(&config, &["gone.rs".to_string()], &[]),
            WorkOutcome::Done
        );
        assert!(
            !config.db_path.exists(),
            "a removal against a reset index must not recreate index.db"
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonical_root_resolves_symlinked_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let link = tmp.path().join("link");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(canonical_root(&link), std::fs::canonicalize(&real).unwrap());
        // A vanished root falls back to the given spelling.
        let gone = tmp.path().join("gone");
        assert_eq!(canonical_root(&gone), gone);
    }
}
