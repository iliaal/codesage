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
use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::lockfile;

/// Wait for a pause in saves, then embed quiet files together instead of re-embedding each save.
const DEFAULT_DEBOUNCE_MS: u64 = 30_000;
/// Prevent per-keystroke indexing, including when the requested debounce is zero.
pub const MIN_DEBOUNCE_MS: u64 = 1_000;
/// Persistent host load must not defer indexing indefinitely.
const BACKPRESSURE_MAX_DEFER: Duration = Duration::from_secs(15 * 60);
const BATCH_THRESHOLD: usize = 10;
const BATCH_WINDOW: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const EVENT_QUEUE_CAPACITY: usize = 256;
const EVENT_PATH_LIMIT: usize = 64;
const EVENT_PATH_BYTES: usize = 64 * 1024;
const PENDING_PATH_LIMIT: usize = 4096;
const PENDING_PATH_BYTES: usize = 4 * 1024 * 1024;
/// Coalesce backlog replay: events queued during a bulk pass must not trigger another pass per batch.
const BULK_COOLDOWN: Duration = Duration::from_secs(3);
const DEFAULT_IDLE_SECS: u64 = 1800;
/// Bound hard removal failures so queued paths cannot prevent idle exit forever.
/// Lock contention does not count toward this limit.
const MAX_REMOVAL_FAILURES: u32 = 10;

/// Lazy model lookup: daemon watchers share the pool; standalone watchers load privately.
pub type EmbedderProvider = Arc<dyn Fn() -> Result<Arc<Mutex<Embedder>>> + Send + Sync>;

/// Foreground PIDs may be signalled; daemon threads must stop through the disabled marker.
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
    /// Disable in tests so host load cannot masquerade as lock contention.
    pub backpressure: bool,
}

/// Look up the model on each use to refresh pool activity without pinning it while idle.
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

    /// Disabled indexing is complete; a failed model load leaves semantic rows stale.
    fn get(&mut self) -> EmbedderLookup {
        let Some(provider) = self.provider.as_ref() else {
            return EmbedderLookup::Disabled;
        };
        match provider() {
            Ok(emb) => EmbedderLookup::Loaded(emb),
            Err(e) => {
                tracing::warn!(error = %e, "loading embedder for watcher");
                EmbedderLookup::LoadFailed
            }
        }
    }
}

enum EmbedderLookup {
    /// Semantic indexing is off for this watcher: nothing to embed.
    Disabled,
    Loaded(Arc<Mutex<Embedder>>),
    /// Semantic indexing is on and the model did not load; the semantic rows
    /// of every path in the pass are stale until a retry.
    LoadFailed,
}

/// Retries before parking; delays grow through [`semantic_retry_extra_delay`].
const MAX_SEMANTIC_RETRIES: u32 = 5;

/// Retry parked paths even without another save; expose them through `watch.status`.
const PARKED_RETRY_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Longest extra wait a semantic retry adds on top of the debounce window.
const MAX_SEMANTIC_RETRY_EXTRA: Duration = Duration::from_secs(600);

/// Extra delay beyond debounce: `debounce × (2^(attempt-1) − 1)`, capped.
fn semantic_retry_extra_delay(attempt: u32, debounce: Duration) -> Duration {
    let factor = 2u32
        .saturating_pow(attempt.saturating_sub(1))
        .saturating_sub(1);
    debounce
        .saturating_mul(factor)
        .min(MAX_SEMANTIC_RETRY_EXTRA)
}

/// Only `Done` permits clearing covered work; `Skipped` must retain it for retry.
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

struct StatusGuard(PathBuf);

impl Drop for StatusGuard {
    fn drop(&mut self) {
        let _ = crate::fsguard::remove_state_file(&self.0);
    }
}

#[derive(Default)]
struct FilterRefresh {
    reload: bool,
    reconciling: bool,
    failures: u32,
    retry_at: Option<Instant>,
}

impl FilterRefresh {
    fn request(&mut self, now: Instant) {
        self.reload = true;
        if self.failures == 0 {
            self.retry_at = Some(self.retry_at.unwrap_or(now).max(now));
        }
    }

    fn pending(&self) -> bool {
        self.reload || self.reconciling
    }

    fn parked(&self) -> bool {
        self.failures > MAX_SEMANTIC_RETRIES
    }

    fn due(&self, now: Instant) -> bool {
        self.pending() && self.retry_at.is_none_or(|at| now >= at)
    }

    fn failed(&mut self, now: Instant, debounce: Duration) {
        self.failures = self.failures.saturating_add(1);
        let delay = if self.parked() {
            PARKED_RETRY_INTERVAL
        } else {
            let base = debounce.max(Duration::from_secs(1));
            base + semantic_retry_extra_delay(self.failures, base)
        };
        self.retry_at = Some(now + delay);
        tracing::warn!(
            failures = self.failures,
            retry_after_secs = delay.as_secs(),
            "watch filter reconciliation incomplete; retaining work for retry"
        );
    }
}

pub fn run_statewatcher(config: StateWatcherConfig) -> Result<()> {
    run_statewatcher_with_registration(config, watch_tree)
}

fn run_statewatcher_with_registration(
    config: StateWatcherConfig,
    register: impl FnMut(&mut RecommendedWatcher, &Path, &WatchFilter) -> Result<()>,
) -> Result<()> {
    let (admission, rx) = EventAdmission::channel(EVENT_QUEUE_CAPACITY);
    run_statewatcher_with_admission(config, register, admission, rx)
}

fn run_statewatcher_with_admission(
    mut config: StateWatcherConfig,
    mut register: impl FnMut(&mut RecommendedWatcher, &Path, &WatchFilter) -> Result<()>,
    admission: EventAdmission,
    rx: mpsc::Receiver<Event>,
) -> Result<()> {
    // FSEvents resolves symlinks (including /var); registration and strip_prefix must agree.
    config.project_root = canonical_root(&config.project_root);

    let project_root = config.project_root.clone();

    // Backend errors can mean lost events and require reconciliation.
    let mut watcher = event_watcher(admission.clone(), &project_root)?;

    let mut filter = WatchFilter::new(&config.project_root, &config.exclude_patterns)?;
    register(&mut watcher, &project_root, &filter)?;

    let mut embedder = EmbedderHandle::new(config.embedder.clone());

    let debounce = Duration::from_millis(config.debounce_ms);
    let disabled_marker = watch_disabled_path(&config.project_root);

    write_status(&config.project_root, config.mode, 0, true, false, false)?;
    let _status_guard = StatusGuard(watch_status_path(&config.project_root));

    let mut pending: HashMap<PathBuf, Instant> = HashMap::new();
    let mut removed_paths: Vec<String> = Vec::new();
    // Directory removals emit no child events; resolve affected rows from the index.
    let mut removed_prefixes: Vec<String> = Vec::new();
    let mut batch_event_times: Vec<Instant> = Vec::new();
    let mut currently_indexing: HashSet<PathBuf> = HashSet::new();
    let mut recheck_queue: HashSet<PathBuf> = HashSet::new();
    let mut semantic_retries: HashMap<PathBuf, u32> = HashMap::new();
    let mut parked: HashMap<PathBuf, Instant> = HashMap::new();
    let mut status_written = None;
    // Register watches first: events racing the scan stay queued for replay.
    let mut bulk_retry_at = Some(Instant::now());
    let mut startup_failures = Some(0);
    let mut startup_reconciled = false;
    let mut bulk_cooldown_until: Option<Instant> = None;
    let mut removal_retry_at: Option<Instant> = None;
    let mut removal_fail_count: u32 = 0;
    let mut last_activity = Instant::now();
    let mut header_is_cpp = header_dialect_is_cpp(&config.db_path);
    let mut deferred_since: Option<Instant> = None;
    let mut refresh = FilterRefresh::default();

    tracing::info!(
        root = %config.project_root.display(),
        debounce_ms = config.debounce_ms,
        mode = ?config.mode,
        event_queue_capacity = EVENT_QUEUE_CAPACITY,
        event_path_bytes = EVENT_PATH_BYTES,
        pending_path_bytes = PENDING_PATH_BYTES,
        "statewatcher started"
    );

    let exit_reason = loop {
        if pending_work_exceeds_budget(
            pending
                .keys()
                .chain(currently_indexing.iter())
                .chain(recheck_queue.iter())
                .chain(semantic_retries.keys())
                .chain(parked.keys())
                .map(|path| path.as_os_str().len())
                .chain(
                    removed_paths
                        .iter()
                        .chain(removed_prefixes.iter())
                        .map(String::len),
                ),
        ) {
            pending.clear();
            removed_paths.clear();
            removed_prefixes.clear();
            recheck_queue.clear();
            semantic_retries.clear();
            parked.clear();
            batch_event_times.clear();
            admission.lost.store(true, Ordering::Release);
        }
        if admission.lost.swap(false, Ordering::AcqRel) {
            refresh.request(Instant::now());
            refresh.retry_at = Some(
                refresh
                    .retry_at
                    .unwrap_or_else(Instant::now)
                    .max(bulk_cooldown_until.unwrap_or_else(Instant::now)),
            );
            tracing::warn!(
                "filesystem notification loss; retaining full reconciliation obligation"
            );
        }
        let received_event = match rx.recv_timeout(POLL_INTERVAL) {
            Ok(event) => {
                publish_status(
                    &config,
                    &mut status_written,
                    (parked.len(), true, refresh.parked(), startup_reconciled),
                );
                for path in &event.paths {
                    if path.starts_with(&config.project_root)
                        && path.file_name().is_some_and(|name| name == ".gitignore")
                        && matches!(
                            event.kind,
                            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                        )
                    {
                        refresh.request(Instant::now());
                        bulk_retry_at = Some(Instant::now());
                        continue;
                    }
                    // The root watch is non-recursive. Adopt new trees, including renames,
                    // and scan files that arrived before registration because backends do not replay them.
                    if path.is_dir() && is_dir_adoption_kind(&event.kind) {
                        if path.starts_with(&config.project_root) && !filter.is_ignored(path, true)
                        {
                            refresh.request(Instant::now());
                            bulk_retry_at = Some(Instant::now());
                        }
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
                                        if batch_event_times.len() < BATCH_THRESHOLD {
                                            batch_event_times.push(now);
                                        }
                                    }
                                }
                            }
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

                    // Rename-out emits only the directory event. Treat vanished paths as prefixes,
                    // even when a directory name has a source extension.
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
                            if !header_is_cpp
                                && rel
                                    .extension()
                                    .and_then(|e| e.to_str())
                                    .is_some_and(is_unambiguous_cpp_extension)
                            {
                                header_is_cpp = true;
                            }
                            // A delayed removal must not delete a re-created file's rows.
                            removed_paths.retain(|p| *p != rel_str);
                            pending.insert(rel, Instant::now());
                            if batch_event_times.len() < BATCH_THRESHOLD {
                                batch_event_times.push(Instant::now());
                            }
                        }
                        EventKind::Remove(_) => {
                            removed_paths.push(rel_str);
                        }
                        _ => {}
                    }
                }

                let now = Instant::now();
                batch_event_times.retain(|t| now - *t < BATCH_WINDOW);

                if !refresh.pending() && batch_event_times.len() >= BATCH_THRESHOLD {
                    let burst = batch_event_times.len();
                    batch_event_times.clear();
                    if in_bulk_cooldown(bulk_cooldown_until, now) {
                        // Retain events that raced the scan, but defer their catch-up until cooldown expiry.
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
                    let outcome = startup_outcome(
                        run_bulk_guarded(
                            &config,
                            &mut embedder,
                            &mut deferred_since,
                            &mut semantic_retries,
                            &mut parked,
                        ),
                        &mut startup_failures,
                    );
                    bulk_cooldown_until = bulk_cooldown_after(outcome, Instant::now());
                    if outcome == WorkOutcome::Done {
                        startup_reconciled = true;
                        // A bulk pass may remove the last C++ file and revert header parsing.
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
                    continue;
                }
                true
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => break "watcher channel closed",
        };

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
                &mut semantic_retries,
                &mut parked,
                &filter,
                &mut embedder,
                header_is_cpp,
            );
            break "shutdown";
        }

        if disabled_marker.exists() {
            break "disabled marker present";
        }

        publish_status(
            &config,
            &mut status_written,
            (
                parked.len(),
                refresh.pending()
                    || received_event
                    || bulk_retry_at.is_some()
                    || !pending.is_empty()
                    || !currently_indexing.is_empty()
                    || !recheck_queue.is_empty()
                    || !removed_paths.is_empty()
                    || !removed_prefixes.is_empty()
                    || !semantic_retries.is_empty()
                    || !parked.is_empty()
                    || admission.lost.load(Ordering::Acquire),
                refresh.parked(),
                startup_reconciled,
            ),
        );

        if refresh.reload && refresh.due(Instant::now()) {
            let replacement = (|| {
                let next_filter = WatchFilter::new(&config.project_root, &config.exclude_patterns)?;
                let mut next_watcher = event_watcher(admission.clone(), &project_root)?;
                // Register before scanning: edits in newly admitted trees must
                // queue while reconciliation runs, including during lock retries.
                register(&mut next_watcher, &project_root, &next_filter)?;
                Ok::<_, anyhow::Error>((next_watcher, next_filter))
            })();
            match replacement {
                Ok((next_watcher, next_filter)) => {
                    watcher = next_watcher;
                    filter = next_filter;
                    refresh.reload = false;
                    refresh.reconciling = true;
                }
                Err(error) => {
                    tracing::warn!(error = %error, "replacing watch registrations failed; keeping existing watcher");
                    refresh.failed(Instant::now(), debounce);
                }
            }
        }

        // Retry bulk work before per-file draining so success can clear the entire queue.
        if !refresh.reload
            && if refresh.reconciling {
                refresh.due(Instant::now())
            } else {
                bulk_retry_at.is_some_and(|at| Instant::now() >= at)
            }
        {
            let raw_outcome = run_bulk_guarded(
                &config,
                &mut embedder,
                &mut deferred_since,
                &mut semantic_retries,
                &mut parked,
            );
            let outcome = if refresh.reconciling {
                match raw_outcome {
                    WorkOutcome::Done => refresh = FilterRefresh::default(),
                    WorkOutcome::Skipped => refresh.retry_at = Some(Instant::now() + debounce),
                    WorkOutcome::Failed => refresh.failed(Instant::now(), debounce),
                }
                raw_outcome
            } else {
                startup_outcome(raw_outcome, &mut startup_failures)
            };
            bulk_cooldown_until = bulk_cooldown_after(outcome, Instant::now());
            if outcome == WorkOutcome::Done {
                startup_reconciled = true;
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

        if refresh.reconciling {
            last_activity = Instant::now();
            continue;
        }

        // Deletions have no later save event to recover a lock-skipped purge.
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
            &mut semantic_retries,
            &mut parked,
            &filter,
            &mut embedder,
            header_is_cpp,
            debounce,
            &mut deferred_since,
        ) {
            header_is_cpp = header_dialect_is_cpp(&config.db_path);
        }

        let revived = revive_due_parked(
            &mut parked,
            &mut semantic_retries,
            &mut pending,
            Instant::now(),
        );
        if revived > 0 {
            tracing::info!(
                files = revived,
                "retrying parked paths whose semantic rows are still stale"
            );
        }

        // Filtered-out filesystem events must not prevent idle exit; parked work must.
        if !pending.is_empty()
            || !currently_indexing.is_empty()
            || !recheck_queue.is_empty()
            || !removed_paths.is_empty()
            || !removed_prefixes.is_empty()
            || !parked.is_empty()
            || bulk_retry_at.is_some()
            || refresh.pending()
            || admission.lost.load(Ordering::Acquire)
        {
            last_activity = Instant::now();
        } else if is_idle(last_activity, config.idle_timeout) {
            break "idle timeout";
        }
    };

    // Join notify's background thread before return to avoid destroying its mutex in use.
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
    semantic_retries: &mut HashMap<PathBuf, u32>,
    parked: &mut HashMap<PathBuf, Instant>,
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
        semantic_retries,
        parked,
        filter,
        embedder,
        header_is_cpp,
        ready,
    )
}

#[allow(clippy::too_many_arguments)]
fn drain_pending_force(
    config: &StateWatcherConfig,
    pending: &mut HashMap<PathBuf, Instant>,
    currently_indexing: &mut HashSet<PathBuf>,
    recheck_queue: &mut HashSet<PathBuf>,
    semantic_retries: &mut HashMap<PathBuf, u32>,
    parked: &mut HashMap<PathBuf, Instant>,
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
        semantic_retries,
        parked,
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

/// Return true after purging a C++ source that may have been the last one.
/// The caller must then re-derive header parsing.
#[allow(clippy::too_many_arguments)]
fn process_ready(
    config: &StateWatcherConfig,
    pending: &mut HashMap<PathBuf, Instant>,
    currently_indexing: &mut HashSet<PathBuf>,
    recheck_queue: &mut HashSet<PathBuf>,
    semantic_retries: &mut HashMap<PathBuf, u32>,
    parked: &mut HashMap<PathBuf, Instant>,
    filter: &WatchFilter,
    embedder: &mut EmbedderHandle,
    header_is_cpp: bool,
    ready: Vec<PathBuf>,
) -> bool {
    let mut rederive_header = false;
    // Batch stale semantic files so they share a lock, database, and model call.
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

        // Restamp lock-skipped work to avoid retrying on every poll.
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
        match semantic_reindex_batch(config, embedder, &files) {
            WorkOutcome::Done => {
                for (path, _) in &semantic_todo {
                    semantic_retries.remove(path);
                    parked.remove(path);
                }
            }
            WorkOutcome::Skipped => {
                // Structural rows are current; the next drain retries only stale semantic rows.
                let now = Instant::now();
                for (path, _) in semantic_todo {
                    pending.insert(path, now);
                }
            }
            WorkOutcome::Failed => {
                // Back off and eventually park failures; dropping them requires another save to recover.
                let now = Instant::now();
                for path in requeue_failed_semantic(
                    pending,
                    semantic_retries,
                    semantic_todo.into_iter().map(|(path, _)| path),
                    Duration::from_millis(config.debounce_ms),
                    now,
                ) {
                    parked.insert(path, now);
                }
            }
        }
    }
    rederive_header
}
/// Requeue failed semantic work; return exhausted paths for long-backoff parking.
fn requeue_failed_semantic(
    pending: &mut HashMap<PathBuf, Instant>,
    semantic_retries: &mut HashMap<PathBuf, u32>,
    paths: impl IntoIterator<Item = PathBuf>,
    debounce: Duration,
    now: Instant,
) -> Vec<PathBuf> {
    let mut requeued = 0usize;
    let mut abandoned: Vec<PathBuf> = Vec::new();
    let mut abandoned_log: Vec<String> = Vec::new();
    let mut longest_extra = Duration::ZERO;
    let mut deepest_attempt = 0u32;
    for path in paths {
        let attempt = semantic_retries.entry(path.clone()).or_insert(0);
        *attempt += 1;
        if *attempt > MAX_SEMANTIC_RETRIES {
            semantic_retries.remove(&path);
            abandoned_log.push(path.to_string_lossy().into_owned());
            abandoned.push(path);
            continue;
        }
        let extra = semantic_retry_extra_delay(*attempt, debounce);
        deepest_attempt = deepest_attempt.max(*attempt);
        longest_extra = longest_extra.max(extra);
        // Future stamps add backoff before debounce; Instant::duration_since saturates.
        pending.insert(path, now + extra);
        requeued += 1;
    }
    if requeued > 0 {
        tracing::warn!(
            files = requeued,
            attempt = deepest_attempt,
            max_attempts = MAX_SEMANTIC_RETRIES,
            retry_after_secs = (longest_extra + debounce).as_secs(),
            "semantic reindex failed; structural rows landed, semantic rows are stale — re-queued for retry"
        );
    }
    if !abandoned.is_empty() {
        tracing::warn!(
            files = abandoned.len(),
            paths = ?abandoned_log,
            max_attempts = MAX_SEMANTIC_RETRIES,
            "semantic reindex failed repeatedly; parking these paths with a long backoff instead of abandoning them"
        );
    }
    abandoned
}
/// Move parked paths due for another attempt back into `pending` with a fresh
/// retry budget. Returns how many were revived.
fn revive_due_parked(
    parked: &mut HashMap<PathBuf, Instant>,
    semantic_retries: &mut HashMap<PathBuf, u32>,
    pending: &mut HashMap<PathBuf, Instant>,
    now: Instant,
) -> usize {
    let mut due = Vec::new();
    parked.retain(|path, since| {
        if now.duration_since(*since) >= PARKED_RETRY_INTERVAL {
            due.push(path.clone());
            false
        } else {
            true
        }
    });
    for path in &due {
        semantic_retries.remove(path);
        pending.insert(path.clone(), now);
    }
    due.len()
}

/// Batch semantic work under one lock; unchanged chunk text reuses stored vectors.
fn semantic_reindex_batch(
    config: &StateWatcherConfig,
    embedder: &mut EmbedderHandle,
    files: &[FileInfo],
) -> WorkOutcome {
    let emb_arc = match embedder.get() {
        EmbedderLookup::Loaded(emb) => emb,
        // A missing model must not attest unwritten semantic rows as current.
        EmbedderLookup::Disabled | EmbedderLookup::LoadFailed => return WorkOutcome::Failed,
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
    let fingerprint = match crate::commands::index::resolved_fingerprint(
        &db,
        &config.embed_config,
        emb.dim(),
    ) {
        Ok(fingerprint) => fingerprint,
        Err(e) => {
            tracing::warn!(error = %e, "deriving the semantic fingerprint for semantic reindex");
            return WorkOutcome::Failed;
        }
    };
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

/// Vanished or empty files are purged and may require header-dialect re-derivation.
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
        // Rename-old paths arrive as Modify, and editor swaps can disappear before this read.
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

    // Expand prefixes under the lock or rows committed during acquisition could escape removal.
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

    // A queued removal may race re-creation; preserve live files.
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
    // Do not resurrect a reset database and satisfy warm-state checks with an empty index.
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

/// Purge vanished or emptied files under the index lock.
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

/// Match discovery's header rule using indexed paths, not stored language:
/// CUDA files have C++ language but do not flip `.h` parsing. Never create a reset index.
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

/// Avoid a dialect scan unless removal could have deleted the last C++ source.
fn removed_paths_may_unflip_header(header_is_cpp: bool, paths: &[String]) -> bool {
    header_is_cpp
        && paths.iter().any(|p| {
            Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(is_unambiguous_cpp_extension)
        })
}

/// Reconcile lost events without clearing queues or bypassing the bulk cooldown.
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

/// Only completed bulk passes may suppress another burst-triggered pass.
fn bulk_cooldown_after(outcome: WorkOutcome, now: Instant) -> Option<Instant> {
    (outcome == WorkOutcome::Done).then(|| now + BULK_COOLDOWN)
}

fn in_bulk_cooldown(cooldown_until: Option<Instant>, now: Instant) -> bool {
    cooldown_until.is_some_and(|until| now < until)
}

/// Typed SQLite codes take precedence over message substrings. Busy, locked,
/// full-disk, and interrupted work can retry; other failures use bounded recovery.
fn is_retryable_db_error(err: &anyhow::Error) -> bool {
    if let Some(code) = sqlite_error_code(err) {
        use rusqlite::ErrorCode::*;
        return matches!(
            code,
            DatabaseBusy | DatabaseLocked | DiskFull | OperationInterrupted
        );
    }
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("is locked") || msg.contains("busy")
}

/// The SQLite error code carried anywhere in `err`'s context chain, if any.
fn sqlite_error_code(err: &anyhow::Error) -> Option<rusqlite::ErrorCode> {
    err.chain().find_map(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .and_then(|e| e.sqlite_error_code())
    })
}
/// `Skipped` retains work until both debounce and cooldown expire; `Failed`
/// preserves original stamps for per-file fallback. Only `Done` clears queues.
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
            // Keep per-file work unready until the bulk retry; retry_at includes debounce.
            let stamp = retry_at - debounce;
            for s in pending.values_mut() {
                *s = stamp;
            }
            Some(retry_at)
        }
        WorkOutcome::Failed => None,
    }
}

fn startup_outcome(outcome: WorkOutcome, failures: &mut Option<u32>) -> WorkOutcome {
    let Some(count) = failures.as_mut() else {
        return outcome;
    };
    match outcome {
        WorkOutcome::Done => *failures = None,
        WorkOutcome::Failed => {
            *count += 1;
            if *count < 3 {
                return WorkOutcome::Skipped;
            }
            tracing::warn!(
                "startup reconciliation failed three times; run codesage index to retry"
            );
            *failures = None;
        }
        WorkOutcome::Skipped => {}
    }
    outcome
}

/// Retain lock-skipped work without consuming the hard-failure budget.
/// Exhausted hard failures clear queues so the watcher can idle out.
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

fn run_bulk_guarded(
    config: &StateWatcherConfig,
    embedder: &mut EmbedderHandle,
    deferred_since: &mut Option<Instant>,
    semantic_retries: &mut HashMap<PathBuf, u32>,
    parked: &mut HashMap<PathBuf, Instant>,
) -> WorkOutcome {
    if config.backpressure {
        let reason = backpressure_reason(&config.project_root);
        if should_defer(reason.as_deref(), deferred_since, Instant::now()) {
            return WorkOutcome::Skipped;
        }
    }
    let outcome = run_bulk_incremental(config, embedder);
    if outcome == WorkOutcome::Done {
        semantic_retries.clear();
        parked.clear();
    }
    outcome
}

/// Bound a continuous deferral streak with [`BACKPRESSURE_MAX_DEFER`].
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

/// Defer during Git writes, excessive load, or builds under the project root.
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

/// Scan /proc only for ready batches; unrelated short-lived commands are not backpressure.
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

    match codesage_graph::incremental_index(
        &config.project_root,
        &db,
        &config.exclude_patterns,
        false,
    ) {
        Ok(stats) if stats.files_failed == 0 => {}
        Ok(stats) => {
            tracing::warn!(
                files_failed = stats.files_failed,
                "bulk structural reconciliation left unreadable or failed files"
            );
            return WorkOutcome::Failed;
        }
        Err(e) => {
            tracing::warn!(error = %e, "bulk incremental structural reindex failed");
            return WorkOutcome::Failed;
        }
    }

    let emb_arc = match embedder.get() {
        EmbedderLookup::Disabled => return WorkOutcome::Done,
        EmbedderLookup::Loaded(emb) => emb,
        EmbedderLookup::LoadFailed => {
            // Reporting Done here would discard stale semantic work before per-file recovery.
            tracing::warn!("bulk incremental semantic reindex skipped: embedder did not load");
            return WorkOutcome::Failed;
        }
    };
    {
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
        let fingerprint = match crate::commands::index::resolved_fingerprint(
            &db,
            &config.embed_config,
            emb.dim(),
        ) {
            Ok(fingerprint) => fingerprint,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "deriving the semantic fingerprint for bulk semantic incremental"
                );
                return WorkOutcome::Failed;
            }
        };
        match codesage_graph::semantic_incremental_index(
            &config.project_root,
            &db,
            &mut *emb,
            &config.exclude_patterns,
            &fingerprint,
            false,
        ) {
            Ok(stats) if stats.files_failed == 0 => {}
            Ok(stats) => {
                tracing::warn!(
                    files_failed = stats.files_failed,
                    "bulk semantic reconciliation left unreadable or failed files"
                );
                return WorkOutcome::Failed;
            }
            Err(e) => {
                tracing::warn!(error = %e, "bulk incremental semantic reindex failed");
                return WorkOutcome::Failed;
            }
        }
    }

    WorkOutcome::Done
}

#[derive(Clone)]
struct EventAdmission {
    tx: mpsc::SyncSender<Event>,
    lost: Arc<AtomicBool>,
}

impl EventAdmission {
    fn channel(capacity: usize) -> (Self, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        (
            Self {
                tx,
                lost: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }

    fn admit(&self, result: Result<Event, notify::Error>) {
        let Ok(event) = result else {
            self.lost.store(true, Ordering::Release);
            return;
        };
        if event.need_rescan() {
            self.lost.store(true, Ordering::Release);
            return;
        }
        if !matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) {
            return;
        }
        if event.paths.len() > EVENT_PATH_LIMIT
            || event
                .paths
                .iter()
                .map(|path| path.as_os_str().len())
                .sum::<usize>()
                > EVENT_PATH_BYTES
        {
            self.lost.store(true, Ordering::Release);
            return;
        }
        // Backend allocation capacities and optional diagnostic strings are not admission budgets.
        let mut bounded = Event::new(event.kind);
        bounded.paths = event
            .paths
            .iter()
            .map(|path| path.as_path().to_path_buf())
            .collect();
        if bounded.paths.iter().map(PathBuf::capacity).sum::<usize>() > EVENT_PATH_BYTES
            || self.tx.try_send(bounded).is_err()
        {
            self.lost.store(true, Ordering::Release);
        }
    }
}

fn pending_work_exceeds_budget(lengths: impl Iterator<Item = usize>) -> bool {
    let mut bytes = 0usize;
    for (index, length) in lengths.enumerate() {
        bytes = bytes.saturating_add(length);
        if index >= PENDING_PATH_LIMIT || bytes > PENDING_PATH_BYTES {
            return true;
        }
    }
    false
}

fn event_watcher(admission: EventAdmission, project_root: &Path) -> Result<RecommendedWatcher> {
    let root = project_root.to_path_buf();
    notify::recommended_watcher(move |mut res: Result<Event, notify::Error>| {
        if let Ok(event) = &mut res
            && !event.need_rescan()
        {
            event.paths.retain(|path| relevant_watch_path(&root, path));
            if event.paths.is_empty() {
                return;
            }
        }
        admission.admit(res);
    })
    .context("creating filesystem watcher")
}

fn relevant_watch_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    if relative
        .file_name()
        .is_some_and(|name| name == ".gitignore")
    {
        return true;
    }
    !relative
        .components()
        .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
}

// Avoid watching ignored top-level trees and their build/VCS churn.
fn watch_tree(watcher: &mut RecommendedWatcher, root: &Path, filter: &WatchFilter) -> Result<()> {
    watcher
        .watch(root, RecursiveMode::NonRecursive)
        .context("watching project root")?;
    let entries = std::fs::read_dir(root).context("reading project root for watch set")?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() && !filter.is_ignored(&path, true) {
            watcher
                .watch(&path, RecursiveMode::Recursive)
                .with_context(|| format!("watching subtree {}", path.display()))?;
        }
    }
    Ok(())
}

/// Rename-in arrives as Modify(Name), so Create alone misses populated trees.
fn is_dir_adoption_kind(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

/// Bound synchronous event-loop scanning; large adopted trees use bulk reconciliation.
const ADOPTION_SCAN_ENTRY_CAP: usize = 2048;

#[derive(Debug, PartialEq, Eq)]
enum DirScan {
    /// Every non-ignored source file under the directory, root-relative.
    Files(Vec<PathBuf>),
    /// Source threshold or entry cap exceeded; schedule a bulk pass.
    OverThreshold,
}

/// Capture files populated before watch registration, which backends do not replay.
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

/// Resolve indexed descendants. None retains prefixes for retry; a missing DB is
/// empty and must not be recreated.
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

/// Preserve the supplied spelling if canonicalization fails, including inaccessible ancestors.
fn canonical_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

/// Deeper directories inherit recursive watches; only new root children need registration.
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

fn is_source_file(rel: &Path) -> bool {
    detect_language(rel).is_some()
}

pub fn resolve_debounce_ms() -> u64 {
    let raw = std::env::var("REINDEX_DEBOUNCE").ok();
    match raw.as_deref().and_then(|s| s.parse::<u64>().ok()) {
        Some(ms) => {
            let floored = floor_debounce_ms(ms);
            if floored != ms {
                tracing::warn!(
                    requested_ms = ms,
                    applied_ms = floored,
                    "REINDEX_DEBOUNCE below the floor; clamped",
                );
            }
            floored
        }
        None => {
            if raw.is_some() {
                tracing::warn!(
                    default_ms = DEFAULT_DEBOUNCE_MS,
                    "unparseable REINDEX_DEBOUNCE; using the default",
                );
            }
            DEFAULT_DEBOUNCE_MS
        }
    }
}

/// Share the debounce floor between environment and CLI settings.
pub fn floor_debounce_ms(ms: u64) -> u64 {
    ms.max(MIN_DEBOUNCE_MS)
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchStatus {
    pub mode: WatcherMode,
    pub pid: u32,
    pub started_at_unix: u64,
    /// Semantic failures awaiting long-backoff retry; absent in older status files.
    #[serde(default)]
    pub stale_parked: usize,
    #[serde(default)]
    pub reconciliation_pending: bool,
    #[serde(default)]
    pub reconciliation_parked: bool,
    #[serde(default)]
    pub startup_reconciled: bool,
}

fn publish_status(
    config: &StateWatcherConfig,
    written: &mut Option<(usize, bool, bool, bool)>,
    next: (usize, bool, bool, bool),
) {
    if *written == Some(next) {
        return;
    }
    match write_status(
        &config.project_root,
        config.mode,
        next.0,
        next.1,
        next.2,
        next.3,
    ) {
        Ok(()) => *written = Some(next),
        Err(error) => tracing::warn!(error = %error, "refreshing watch status"),
    }
}

pub fn watch_status_path(root: &Path) -> PathBuf {
    root.join(".codesage").join("watch.status")
}

pub fn watch_disabled_path(root: &Path) -> PathBuf {
    root.join(".codesage").join("watch.disabled")
}

fn write_status(
    root: &Path,
    mode: WatcherMode,
    stale_parked: usize,
    reconciliation_pending: bool,
    reconciliation_parked: bool,
    startup_reconciled: bool,
) -> Result<()> {
    let started_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let status = WatchStatus {
        mode,
        pid: std::process::id(),
        started_at_unix,
        stale_parked,
        reconciliation_pending,
        reconciliation_parked,
        startup_reconciled,
    };
    let path = watch_status_path(root);
    let json = serde_json::to_string(&status)?;
    crate::fsguard::reject_symlinked_project_dir(&path)?;
    codesage_graph::state_file::replace(&path, json.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn read_status(root: &Path) -> Option<WatchStatus> {
    let path = watch_status_path(root);
    let raw = crate::fsguard::read_state_to_string(&path).ok()?;
    let status: WatchStatus = serde_json::from_str(&raw).ok()?;
    // Abrupt process death bypasses StatusGuard cleanup.
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

/// Treat dangling symlinks as present so status and write-side marker handling agree.
pub fn watch_disabled_marker_present(root: &Path) -> bool {
    // lstat still follows intermediate components; reject a symlinked parent separately.
    let dir = root.join(".codesage");
    if !std::fs::symlink_metadata(&dir)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return false;
    }
    std::fs::symlink_metadata(watch_disabled_path(root)).is_ok()
}

/// A leaked Arc keeps the atomic shutdown flag valid for every signal handler invocation.
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
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let victim = root.join("victim.key");
        std::fs::write(&victim, b"PRIVATE KEY").unwrap();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::os::unix::fs::symlink(&victim, watch_status_path(root)).unwrap();

        assert!(write_status(root, WatcherMode::Foreground, 0, false, false, false).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"PRIVATE KEY");
    }

    #[cfg(unix)]
    #[test]
    fn write_status_writes_an_ordinary_status_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();

        write_status(root, WatcherMode::Foreground, 0, false, false, true).unwrap();

        let status = read_status(root).expect("status must round-trip");
        assert_eq!(status.pid, std::process::id());
        assert!(status.startup_reconciled);
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

    fn saturate_admission(admission: &EventAdmission) {
        for _ in 0..EVENT_QUEUE_CAPACITY + 1 {
            admission.admit(Ok(Event::new(EventKind::Modify(ModifyKind::Any))));
        }
        assert!(admission.lost.load(Ordering::Acquire));
    }

    #[test]
    fn event_admission_bounds_storage_without_blocking_the_producer() {
        let (admission, rx) = EventAdmission::channel(EVENT_QUEUE_CAPACITY);
        let producer = admission.clone();
        let thread = std::thread::spawn(move || {
            for i in 0..100_000 {
                producer.admit(Ok(Event::new(EventKind::Modify(ModifyKind::Any))
                    .add_path(PathBuf::from(format!("path-{i}.rs")))));
            }
        });
        await_watcher_condition(|| thread.is_finished());
        thread.join().unwrap();
        assert!(admission.lost.load(Ordering::Acquire));
        assert_eq!(rx.try_iter().count(), EVENT_QUEUE_CAPACITY);
        admission.lost.store(false, Ordering::Release);
        for _ in 0..1000 {
            admission.admit(Ok(Event::new(EventKind::Access(
                notify::event::AccessKind::Any,
            ))));
        }
        assert!(!admission.lost.load(Ordering::Acquire));
        assert!(rx.try_recv().is_err());
        for _ in 0..EVENT_QUEUE_CAPACITY + 1 {
            let mut event = Event::new(EventKind::Modify(ModifyKind::Any));
            event.paths = vec![
                PathBuf::from("x".repeat(EVENT_PATH_BYTES / EVENT_PATH_LIMIT));
                EVENT_PATH_LIMIT
            ];
            admission.admit(Ok(event));
        }
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), EVENT_QUEUE_CAPACITY);
        let allocated_path_bytes: usize = events
            .iter()
            .flat_map(|event| &event.paths)
            .map(PathBuf::capacity)
            .sum();
        assert_eq!(
            allocated_path_bytes,
            EVENT_QUEUE_CAPACITY * EVENT_PATH_BYTES
        );
        eprintln!(
            "saturated admission: {} events, {} path allocation bytes; 100000-event producer completed without a receiver",
            events.len(),
            allocated_path_bytes
        );
        assert!(admission.lost.load(Ordering::Acquire));
        for paths in [
            vec![PathBuf::from("x"); EVENT_PATH_LIMIT + 1],
            vec![PathBuf::from("x".repeat(EVENT_PATH_BYTES + 1))],
        ] {
            admission.lost.store(false, Ordering::Release);
            let mut event = Event::new(EventKind::Modify(ModifyKind::Any));
            event.paths = paths;
            admission.admit(Ok(event));
            assert!(admission.lost.load(Ordering::Acquire));
            assert!(rx.try_recv().is_err());
        }
        admission.lost.store(false, Ordering::Release);
        admission.admit(Err(notify::Error::generic("backend lost coverage")));
        assert!(admission.lost.load(Ordering::Acquire));
        admission.lost.store(false, Ordering::Release);
        admission.admit(Ok(
            Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan)
        ));
        assert!(admission.lost.load(Ordering::Acquire));
    }

    #[test]
    fn accumulated_work_is_bounded_by_paths_and_bytes() {
        assert!(!pending_work_exceeds_budget(std::iter::repeat_n(
            1,
            PENDING_PATH_LIMIT
        )));
        assert!(pending_work_exceeds_budget(std::iter::repeat_n(
            1,
            PENDING_PATH_LIMIT + 1
        )));
        assert!(!pending_work_exceeds_budget(
            [PENDING_PATH_BYTES].into_iter()
        ));
        assert!(pending_work_exceeds_budget(
            [PENDING_PATH_BYTES, 1].into_iter()
        ));
    }

    #[test]
    fn accumulated_path_overflow_requires_repair_even_without_channel_loss() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".codesage")).unwrap();
        std::fs::write(root.join("main.rs"), "fn latest_disk_state() {}\n").unwrap();
        let config = test_config(root);
        let db = Database::open(&config.db_path).unwrap();
        let lock = hold_lock(root);
        let (admission, rx) = EventAdmission::channel(EVENT_QUEUE_CAPACITY);
        for batch in 0..=PENDING_PATH_LIMIT / EVENT_PATH_LIMIT {
            let mut event = Event::new(EventKind::Modify(ModifyKind::Any));
            for index in 0..EVENT_PATH_LIMIT {
                event
                    .paths
                    .push(root.join(format!("pending-{}-{index}.rs", batch)));
            }
            admission.admit(Ok(event));
        }
        assert!(!admission.lost.load(Ordering::Acquire));
        let shutdown = config.shutdown.clone();
        let watcher = RunningWatcher {
            shutdown,
            thread: Some(std::thread::spawn(move || {
                run_statewatcher_with_admission(config, watch_tree, admission, rx)
            })),
        };
        await_watcher_condition(|| {
            read_status(root).is_some_and(|status| status.reconciliation_pending)
        });
        assert!(!db.symbol_exists("latest_disk_state").unwrap());
        drop(lock);
        await_watcher_condition(|| {
            db.symbol_exists("latest_disk_state").unwrap()
                && read_status(root).is_some_and(|status| !status.reconciliation_pending)
        });
        drop(watcher);
        assert_eq!(db.all_files_with_id_and_language().unwrap().len(), 1);
    }

    #[test]
    fn bulk_reconciliation_retries_partial_structural_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".codesage")).unwrap();
        std::fs::write(root.join("main.rs"), "fn failed_then_recovered() {}\n").unwrap();
        let config = test_config(root);
        let db = Database::open(&config.db_path).unwrap();
        db.execute_raw_for_tests("CREATE TRIGGER reject_symbols BEFORE INSERT ON symbols BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;").unwrap();
        let mut embedder = EmbedderHandle::new(None);
        assert_eq!(
            run_bulk_incremental(&config, &mut embedder),
            WorkOutcome::Failed
        );
        assert!(!db.symbol_exists("failed_then_recovered").unwrap());
        db.execute_raw_for_tests("DROP TRIGGER reject_symbols")
            .unwrap();
        assert_eq!(
            run_bulk_incremental(&config, &mut embedder),
            WorkOutcome::Done
        );
        assert!(db.symbol_exists("failed_then_recovered").unwrap());
    }

    #[test]
    fn lost_events_survive_failed_registration_and_loss_during_repair() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".codesage")).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir(root.join("generated")).unwrap();
        std::fs::write(root.join(".gitignore"), "generated/\n").unwrap();
        std::fs::write(root.join("gone.rs"), "fn deleted_during_loss() {}\n").unwrap();
        std::fs::write(root.join("main.rs"), "fn before_loss() {}\n").unwrap();
        std::fs::write(root.join("generated/api.rs"), "fn newly_admitted() {}\n").unwrap();
        let mut config = test_config(root);
        config.idle_timeout = Duration::from_millis(100);
        let db = Database::open(&config.db_path).unwrap();
        codesage_graph::incremental_index(root, &db, &[], false).unwrap();
        assert!(db.symbol_exists("deleted_during_loss").unwrap());
        let (admission, rx) = EventAdmission::channel(1);
        let producer = admission.clone();
        let registrations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts = registrations.clone();
        let failed_registrations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let failed_attempts = failed_registrations.clone();
        let recovered = Arc::new(AtomicBool::new(false));
        let permit_recovery = recovered.clone();
        let shutdown = config.shutdown.clone();
        let mut overflow_during_repair = false;
        let watcher = RunningWatcher {
            shutdown,
            thread: Some(std::thread::spawn(move || {
                run_statewatcher_with_admission(
                    config,
                    move |watcher, root, filter| {
                        let attempt = attempts.fetch_add(1, Ordering::AcqRel);
                        if attempt == 0 {
                            watch_tree(watcher, root, filter)?;
                            saturate_admission(&producer);
                        } else if !permit_recovery.load(Ordering::Acquire) {
                            failed_attempts.fetch_add(1, Ordering::Release);
                            anyhow::bail!("registration unavailable");
                        } else {
                            watch_tree(watcher, root, filter)?;
                            if !overflow_during_repair {
                                saturate_admission(&producer);
                                overflow_during_repair = true;
                            }
                        }
                        Ok(())
                    },
                    admission,
                    rx,
                )
            })),
        };
        await_watcher_condition(|| failed_registrations.load(Ordering::Acquire) >= 1);
        assert!(read_status(root).unwrap().reconciliation_pending);
        assert!(!watcher.thread.as_ref().unwrap().is_finished());
        std::fs::remove_file(root.join("gone.rs")).unwrap();
        std::fs::write(root.join(".gitignore"), "excluded.rs\n").unwrap();
        std::fs::write(root.join("excluded.rs"), "fn must_be_excluded() {}\n").unwrap();
        std::fs::write(root.join("main.rs"), "fn after_loss() {}\n").unwrap();
        std::fs::create_dir(root.join("adopted")).unwrap();
        std::fs::write(root.join("adopted/new.rs"), "fn new_watch_root() {}\n").unwrap();
        recovered.store(true, Ordering::Release);
        await_watcher_condition(|| {
            db.symbol_exists("after_loss").unwrap()
                && db.symbol_exists("new_watch_root").unwrap()
                && db.symbol_exists("newly_admitted").unwrap()
        });
        await_watcher_condition(|| registrations.load(Ordering::Acquire) >= 4);
        drop(watcher);
        let fresh = Database::open(&root.join(".codesage/fresh.db")).unwrap();
        codesage_graph::incremental_index(root, &fresh, &[], false).unwrap();
        let paths = |db: &Database| {
            let mut rows: Vec<_> = db
                .all_files_with_id_and_language()
                .unwrap()
                .into_iter()
                .map(|(_, path, lang)| (path, lang))
                .collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            rows
        };
        assert_eq!(paths(&db), paths(&fresh));
        for name in [
            "deleted_during_loss",
            "before_loss",
            "must_be_excluded",
            "after_loss",
            "newly_admitted",
            "new_watch_root",
        ] {
            assert_eq!(
                db.symbol_exists(name).unwrap(),
                fresh.symbol_exists(name).unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    #[ignore = "requires the pinned Jina model and CUDA runtime; run with --features cuda"]
    fn saturated_indexing_repairs_structural_and_semantic_state_with_real_model() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".codesage")).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir(root.join("generated")).unwrap();
        std::fs::write(root.join(".gitignore"), "generated/\n").unwrap();
        std::fs::write(root.join("gone.rs"), "fn removed_after_scan() {}\n").unwrap();
        std::fs::write(root.join("main.rs"), "fn before_scan() {}\n").unwrap();
        std::fs::write(
            root.join("generated/api.rs"),
            "fn admitted_after_scan() {}\n",
        )
        .unwrap();
        let mut config = test_config(root);
        config.embed_config.model = "jinaai/jina-embeddings-v2-base-code".into();
        config.embed_config.device = "gpu".into();
        let model = Arc::new(Mutex::new(Embedder::new(&config.embed_config).unwrap()));
        let shared = model.clone();
        let provider: EmbedderProvider = Arc::new(move || Ok(shared.clone()));
        assert_eq!(
            run_bulk_incremental(&config, &mut EmbedderHandle::new(Some(provider.clone()))),
            WorkOutcome::Done
        );
        let dim = model.lock().dim();
        let db =
            Database::open_for_model(&config.db_path, &config.embed_config.model, dim).unwrap();
        assert!(!db.chunks_for_file("gone.rs").unwrap().is_empty());
        let (admission, rx) = EventAdmission::channel(1);
        let producer = admission.clone();
        let source_root = root.to_path_buf();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider_attempts = attempts.clone();
        config.embedder = Some(Arc::new(move || {
            let attempt = provider_attempts.fetch_add(1, Ordering::AcqRel);
            if attempt == 0 {
                std::fs::remove_file(source_root.join("gone.rs"))?;
                std::fs::write(source_root.join(".gitignore"), "excluded.rs\n")?;
                std::fs::write(
                    source_root.join("excluded.rs"),
                    "fn excluded_after_scan() {}\n",
                )?;
                std::fs::write(source_root.join("main.rs"), "fn changed_after_scan() {}\n")?;
                std::fs::create_dir(source_root.join("adopted"))?;
                std::fs::write(
                    source_root.join("adopted/new.rs"),
                    "fn adopted_after_scan() {}\n",
                )?;
                saturate_admission(&producer);
            }
            if attempt < 2 {
                anyhow::bail!("model temporarily unavailable after structural scan");
            }
            if attempt == 2 {
                std::fs::write(
                    source_root.join("main.rs"),
                    "fn changed_during_repair() {}\n",
                )?;
                saturate_admission(&producer);
            }
            Ok(model.clone())
        }));
        let mut fresh_config = test_config(root);
        fresh_config.db_path = root.join(".codesage/fresh.db");
        fresh_config.embed_config = config.embed_config.clone();
        let shutdown = config.shutdown.clone();
        let watcher = RunningWatcher {
            shutdown,
            thread: Some(std::thread::spawn(move || {
                run_statewatcher_with_admission(config, watch_tree, admission, rx)
            })),
        };
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let repaired = attempts.load(Ordering::Acquire) >= 4
                && db.symbol_exists("changed_during_repair").unwrap()
                && read_status(root).is_some_and(|status| !status.reconciliation_pending);
            if repaired {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "real-model watcher repair timed out: attempts={}, status={:?}, symbols={:?}",
                attempts.load(Ordering::Acquire),
                read_status(root),
                db.symbols_for_file("main.rs").unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::write(
            root.join("adopted/new.rs"),
            "fn edited_through_new_watch() {}\n",
        )
        .unwrap();
        await_watcher_condition(|| {
            db.symbol_exists("edited_through_new_watch").unwrap()
                && db
                    .chunks_for_file("adopted/new.rs")
                    .unwrap()
                    .iter()
                    .any(|row| row.content.contains("edited_through_new_watch"))
                && read_status(root).is_some_and(|status| !status.reconciliation_pending)
        });
        drop(watcher);
        assert_eq!(
            run_bulk_incremental(&fresh_config, &mut EmbedderHandle::new(Some(provider))),
            WorkOutcome::Done
        );
        let fresh =
            Database::open_for_model(&fresh_config.db_path, &fresh_config.embed_config.model, dim)
                .unwrap();
        let mut actual_paths = db.all_chunk_file_paths().unwrap();
        let mut expected_paths = fresh.all_chunk_file_paths().unwrap();
        actual_paths.sort();
        expected_paths.sort();
        assert_eq!(actual_paths, expected_paths);
        assert_eq!(
            actual_paths,
            vec!["adopted/new.rs", "generated/api.rs", "main.rs"]
        );
        assert_eq!(
            db.all_file_hashes().unwrap(),
            fresh.all_file_hashes().unwrap()
        );
        assert_eq!(
            db.all_semantic_file_hashes().unwrap(),
            fresh.all_semantic_file_hashes().unwrap()
        );
        assert!(db.semantic_fingerprint().unwrap().is_some());
        assert_eq!(
            db.semantic_fingerprint().unwrap(),
            fresh.semantic_fingerprint().unwrap()
        );
        for path in actual_paths {
            let chunks = |db: &Database| {
                db.chunks_for_file(&path)
                    .unwrap()
                    .into_iter()
                    .map(|row| (row.content, row.start_line, row.end_line))
                    .collect::<Vec<_>>()
            };
            assert_eq!(chunks(&db), chunks(&fresh), "{path}");
            let symbols = |db: &Database| {
                db.symbols_for_file(&path)
                    .unwrap()
                    .into_iter()
                    .map(|symbol| (symbol.name, symbol.line_start, symbol.line_end))
                    .collect::<Vec<_>>()
            };
            assert_eq!(symbols(&db), symbols(&fresh), "{path}");
            let mut actual_vectors = db.chunk_embeddings_for_file(&path).unwrap();
            let mut expected_vectors = fresh.chunk_embeddings_for_file(&path).unwrap();
            actual_vectors.sort_by(|a, b| a.0.cmp(&b.0));
            expected_vectors.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(actual_vectors.len(), expected_vectors.len());
            assert!(!actual_vectors.is_empty());
            for ((actual_text, actual), (expected_text, expected)) in
                actual_vectors.iter().zip(&expected_vectors)
            {
                assert_eq!(actual_text, expected_text);
                assert_eq!(actual.len(), dim);
                assert_eq!(actual.len(), expected.len());
                assert!(actual.iter().chain(expected).all(|value| value.is_finite()));
                let dot: f64 = actual
                    .iter()
                    .zip(expected)
                    .map(|(&a, &b)| f64::from(a) * f64::from(b))
                    .sum();
                let norm = |vector: &[f32]| {
                    vector
                        .iter()
                        .map(|&v| f64::from(v).powi(2))
                        .sum::<f64>()
                        .sqrt()
                };
                assert!(
                    dot / (norm(actual) * norm(expected)) > 0.99999,
                    "{path}: vector direction differs from fresh inference"
                );
            }
        }
        for name in [
            "removed_after_scan",
            "before_scan",
            "excluded_after_scan",
            "changed_after_scan",
        ] {
            assert!(!db.symbol_exists(name).unwrap(), "{name}");
        }
    }

    fn hold_lock(root: &Path) -> lockfile::IndexLock {
        match lockfile::try_acquire(root).unwrap() {
            lockfile::LockOutcome::Acquired(l) => l,
            lockfile::LockOutcome::AlreadyHeld => panic!("fresh tmpdir must lock"),
        }
    }

    struct RunningWatcher {
        shutdown: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<Result<()>>>,
    }

    impl RunningWatcher {
        fn start(config: StateWatcherConfig) -> Self {
            Self {
                shutdown: config.shutdown.clone(),
                thread: Some(std::thread::spawn(move || run_statewatcher(config))),
            }
        }

        fn start_with_registration(
            config: StateWatcherConfig,
            register: impl FnMut(&mut RecommendedWatcher, &Path, &WatchFilter) -> Result<()>
            + Send
            + 'static,
        ) -> Self {
            Self {
                shutdown: config.shutdown.clone(),
                thread: Some(std::thread::spawn(move || {
                    run_statewatcher_with_registration(config, register)
                })),
            }
        }
    }

    impl Drop for RunningWatcher {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let result = thread.join();
                if !std::thread::panicking() {
                    result.unwrap().unwrap();
                }
            }
        }
    }

    fn await_watcher_condition(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "watcher condition timed out");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn startup_reconciles_gap_and_post_start_edits_after_idle_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::write(root.join("main.rs"), "fn before_gap() {}\n").unwrap();
        std::fs::write(root.join("gone.rs"), "fn removed_in_gap() {}\n").unwrap();
        let config = test_config(root);
        let db = Database::open(&config.db_path).unwrap();
        codesage_graph::incremental_index(root, &db, &[], false).unwrap();
        assert!(db.symbol_exists("before_gap").unwrap());
        std::fs::remove_file(root.join("gone.rs")).unwrap();
        std::fs::write(root.join("main.rs"), "fn during_gap() {}\n").unwrap();

        for (gap, next) in [("during_gap", "after_start"), ("idle_gap", "after_restart")] {
            let mut config = test_config(root);
            config.idle_timeout = Duration::from_secs(2);
            let watcher = RunningWatcher::start(config);
            await_watcher_condition(|| db.symbol_exists(gap).unwrap());
            assert!(!db.symbol_exists("before_gap").unwrap());
            assert!(!db.symbol_exists("removed_in_gap").unwrap());
            std::fs::write(root.join("main.rs"), format!("fn {next}() {{}}\n")).unwrap();
            await_watcher_condition(|| db.symbol_exists(next).unwrap());
            assert!(!db.symbol_exists(gap).unwrap());
            await_watcher_condition(|| watcher.thread.as_ref().unwrap().is_finished());
            drop(watcher);
            std::fs::write(root.join("main.rs"), "fn idle_gap() {}\n").unwrap();
        }
    }

    #[test]
    fn startup_reconciliation_waits_for_lock_and_replays_concurrent_events() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::write(root.join("main.rs"), "fn before_lock() {}\n").unwrap();
        let mut config = test_config(root);
        config.exclude_patterns = vec!["**/excluded.rs".into()];
        let db = Database::open(&config.db_path).unwrap();
        codesage_graph::incremental_index(root, &db, &config.exclude_patterns, false).unwrap();
        let lock = hold_lock(root);
        std::fs::write(root.join("main.rs"), "fn gap_while_locked() {}\n").unwrap();
        std::fs::write(root.join("excluded.rs"), "fn excluded_gap() {}\n").unwrap();
        let _watcher = RunningWatcher::start(config);
        await_watcher_condition(|| watch_status_path(root).exists());
        let status = read_status(root).unwrap();
        assert!(status.reconciliation_pending);
        assert!(!status.startup_reconciled);
        std::thread::sleep(Duration::from_millis(600));
        let status = read_status(root).unwrap();
        assert!(status.reconciliation_pending);
        assert!(!status.startup_reconciled);
        assert!(db.symbol_exists("before_lock").unwrap());
        assert!(!db.symbol_exists("gap_while_locked").unwrap());
        std::fs::write(root.join("late.rs"), "fn arrived_while_locked() {}\n").unwrap();
        drop(lock);
        await_watcher_condition(|| {
            db.symbol_exists("gap_while_locked").unwrap()
                && db.symbol_exists("arrived_while_locked").unwrap()
        });
        std::fs::write(root.join("late.rs"), "fn post_reconciliation() {}\n").unwrap();
        await_watcher_condition(|| db.symbol_exists("post_reconciliation").unwrap());
        await_watcher_condition(|| {
            read_status(root).is_some_and(|s| s.startup_reconciled && !s.reconciliation_pending)
        });
        assert!(!db.symbol_exists("arrived_while_locked").unwrap());
        assert!(!db.symbol_exists("excluded_gap").unwrap());
    }

    #[test]
    fn filter_refresh_backs_off_and_reports_parked_work_until_success() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".codesage")).unwrap();
        let mut now = Instant::now();
        let mut refresh = FilterRefresh::default();
        refresh.request(now);
        for seconds in [1, 2, 4, 8, 16, 1800, 1800] {
            refresh.failed(now, Duration::from_millis(100));
            refresh.request(now + Duration::from_millis(1));
            let next = now + Duration::from_secs(seconds);
            assert!(!refresh.due(next - Duration::from_millis(1)));
            assert!(refresh.due(next));
            assert_eq!(refresh.parked(), seconds == 1800);
            assert!(refresh.pending());
            now = next;
        }
        write_status(
            root,
            WatcherMode::Foreground,
            2,
            refresh.pending(),
            refresh.parked(),
            false,
        )
        .unwrap();
        let status = read_status(root).unwrap();
        assert_eq!(status.stale_parked, 2);
        assert!(status.reconciliation_pending);
        assert!(status.reconciliation_parked);
    }

    #[cfg(unix)]
    #[test]
    fn failed_replacement_registration_keeps_live_coverage_and_recovers() {
        for error_code in [libc::ENOENT, libc::ENOSPC] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            std::fs::create_dir(root.join(".codesage")).unwrap();
            std::fs::create_dir(root.join(".git")).unwrap();
            std::fs::create_dir(root.join("generated")).unwrap();
            std::fs::write(root.join(".gitignore"), "generated/\n").unwrap();
            std::fs::write(root.join("main.rs"), "fn before_failure() {}\n").unwrap();
            std::fs::write(root.join("generated/api.rs"), "fn newly_admitted() {}\n").unwrap();
            let config = test_config(root);
            let db = Database::open(&config.db_path).unwrap();
            let failing = Arc::new(AtomicBool::new(true));
            let registration_failing = failing.clone();
            let mut initial = true;
            let watcher =
                RunningWatcher::start_with_registration(config, move |watcher, root, filter| {
                    if !initial && registration_failing.load(Ordering::Relaxed) {
                        return Err(std::io::Error::from_raw_os_error(error_code).into());
                    }
                    initial = false;
                    watch_tree(watcher, root, filter)
                });
            await_watcher_condition(|| db.symbol_exists("before_failure").unwrap());
            std::fs::write(root.join(".gitignore"), "").unwrap();
            await_watcher_condition(|| read_status(root).is_some_and(|s| s.reconciliation_pending));
            assert!(!watcher.thread.as_ref().unwrap().is_finished());
            std::fs::write(root.join("main.rs"), "fn existing_watch_still_live() {}\n").unwrap();
            await_watcher_condition(|| db.symbol_exists("existing_watch_still_live").unwrap());
            assert!(!db.symbol_exists("newly_admitted").unwrap());
            failing.store(false, Ordering::Relaxed);
            await_watcher_condition(|| {
                db.symbol_exists("newly_admitted").unwrap()
                    && read_status(root).is_some_and(|s| !s.reconciliation_pending)
            });
            std::fs::write(
                root.join("generated/api.rs"),
                "fn new_watch_now_live() {}\n",
            )
            .unwrap();
            await_watcher_condition(|| db.symbol_exists("new_watch_now_live").unwrap());
        }
    }

    #[test]
    fn live_filter_semantic_failure_is_visible_and_does_not_retry_each_poll() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join(".codesage")).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join("main.rs"), "fn existing() {}\n").unwrap();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider_attempts = attempts.clone();
        let registrations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider_registrations = registrations.clone();
        let filter_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider_filter_attempts = filter_attempts.clone();
        let mut config = test_config(root);
        config.embedder = Some(Arc::new(move || {
            provider_attempts.fetch_add(1, Ordering::Release);
            if provider_registrations.load(Ordering::Acquire) > 1 {
                provider_filter_attempts.fetch_add(1, Ordering::Release);
            }
            anyhow::bail!("test provider unavailable")
        }));
        let _watcher =
            RunningWatcher::start_with_registration(config, move |watcher, root, filter| {
                watch_tree(watcher, root, filter)?;
                registrations.fetch_add(1, Ordering::Release);
                Ok(())
            });
        await_watcher_condition(|| attempts.load(Ordering::Acquire) >= 3);
        std::fs::write(root.join(".gitignore"), "excluded.rs\n").unwrap();
        await_watcher_condition(|| filter_attempts.load(Ordering::Acquire) >= 1);
        assert!(read_status(root).unwrap().reconciliation_pending);
        let before = attempts.load(Ordering::Acquire);
        std::thread::sleep(Duration::from_millis(550));
        assert_eq!(attempts.load(Ordering::Acquire), before);
        assert!(read_status(root).unwrap().reconciliation_pending);
    }

    #[test]
    fn live_gitignore_reconciles_immediate_edits_and_new_watch_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir(root.join("generated")).unwrap();
        std::fs::write(root.join("main.rs"), "fn initial() {}\n").unwrap();
        std::fs::write(root.join("generated/api.rs"), "fn initially_ignored() {}\n").unwrap();
        std::fs::write(root.join("excluded.rs"), "fn config_excluded() {}\n").unwrap();
        std::fs::write(root.join(".gitignore"), "generated/\n").unwrap();
        let mut config = test_config(root);
        config.exclude_patterns = vec!["**/excluded.rs".into()];
        let db = Database::open(&config.db_path).unwrap();
        let _watcher = RunningWatcher::start(config);
        await_watcher_condition(|| db.symbol_exists("initial").unwrap());
        std::fs::write(root.join("main.rs"), "fn positive_control() {}\n").unwrap();
        await_watcher_condition(|| db.symbol_exists("positive_control").unwrap());
        assert!(!db.symbol_exists("initially_ignored").unwrap());

        std::fs::write(root.join(".gitignore"), "main.rs\n").unwrap();
        std::fs::write(root.join("main.rs"), "fn now_ignored() {}\n").unwrap();
        std::fs::write(
            root.join("generated/api.rs"),
            "fn immediately_unignored() {}\n",
        )
        .unwrap();
        await_watcher_condition(|| {
            db.symbol_exists("immediately_unignored").unwrap()
                && db.get_file_hash("main.rs").unwrap().is_none()
        });
        assert!(!db.symbol_exists("now_ignored").unwrap());
        assert!(!db.symbol_exists("config_excluded").unwrap());

        std::fs::write(root.join("generated/api.rs"), "fn subsequent_edit() {}\n").unwrap();
        await_watcher_condition(|| db.symbol_exists("subsequent_edit").unwrap());
        std::fs::remove_file(root.join(".gitignore")).unwrap();
        std::fs::write(root.join("main.rs"), "fn unignored_after_delete() {}\n").unwrap();
        await_watcher_condition(|| db.symbol_exists("unignored_after_delete").unwrap());
    }

    #[test]
    fn live_nested_gitignore_replacement_waits_for_reconciliation_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/.gitignore"), "api.rs\n").unwrap();
        std::fs::write(root.join("src/api.rs"), "fn ignored_before() {}\n").unwrap();
        std::fs::write(root.join("main.rs"), "fn initial() {}\n").unwrap();
        let config = test_config(root);
        let db = Database::open(&config.db_path).unwrap();
        let _watcher = RunningWatcher::start(config);
        await_watcher_condition(|| db.symbol_exists("initial").unwrap());
        let mut lock = None;
        await_watcher_condition(|| {
            if let lockfile::LockOutcome::Acquired(acquired) = lockfile::try_acquire(root).unwrap()
            {
                lock = Some(acquired);
            }
            lock.is_some()
        });
        std::fs::write(root.join("src/replacement"), "").unwrap();
        std::fs::rename(root.join("src/replacement"), root.join("src/.gitignore")).unwrap();
        std::fs::write(
            root.join("src/api.rs"),
            "fn changed_during_reconciliation() {}\n",
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(600));
        assert!(!db.symbol_exists("changed_during_reconciliation").unwrap());
        std::fs::write(root.join("src/api.rs"), "fn latest_while_locked() {}\n").unwrap();
        drop(lock);
        await_watcher_condition(|| db.symbol_exists("latest_while_locked").unwrap());
        std::fs::write(
            root.join("src/api.rs"),
            "fn edited_after_reconciliation() {}\n",
        )
        .unwrap();
        await_watcher_condition(|| db.symbol_exists("edited_after_reconciliation").unwrap());
        assert!(!db.symbol_exists("latest_while_locked").unwrap());
    }

    #[test]
    fn startup_reconciliation_retries_hard_failures_with_a_bound() {
        let mut failures = Some(0);
        for expected in [
            WorkOutcome::Skipped,
            WorkOutcome::Skipped,
            WorkOutcome::Failed,
        ] {
            assert_eq!(
                startup_outcome(WorkOutcome::Failed, &mut failures),
                expected
            );
        }
        assert_eq!(failures, None);
        let mut failures = Some(0);
        for _ in 0..10 {
            assert_eq!(
                startup_outcome(WorkOutcome::Skipped, &mut failures),
                WorkOutcome::Skipped
            );
        }
        assert_eq!(failures, Some(0));
        assert_eq!(
            startup_outcome(WorkOutcome::Done, &mut failures),
            WorkOutcome::Done
        );
        assert_eq!(failures, None);
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

        assert_eq!(retry, Some(now + debounce));
        assert_eq!(pending.len(), 2);
        assert_eq!(removed, vec!["gone.rs".to_string()]);
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
        assert_eq!(fails, 0);
    }

    #[test]
    fn apply_removal_outcome_gives_up_after_ceiling() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        let mut removed = vec!["gone.rs".to_string()];
        let mut prefixes = vec!["gonedir".to_string()];
        let mut fails = 0;

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

        let paths = vec!["gone.rs".to_string()];
        assert_eq!(handle_removals(&config, &paths, &[]), WorkOutcome::Done);
    }

    #[test]
    fn handle_removals_skips_recreated_file() {
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
        assert!(is_retryable_db_error(
            &anyhow::anyhow!("database is locked").context("removing files")
        ));
        assert!(!is_retryable_db_error(&anyhow::anyhow!(
            "no such table: files"
        )));
        assert!(!is_retryable_db_error(&anyhow::anyhow!("disk I/O error")));
    }

    #[test]
    fn is_retryable_db_error_trusts_typed_codes_over_substrings() {
        use rusqlite::ffi;
        fn typed(code: std::os::raw::c_int) -> anyhow::Error {
            rusqlite::Error::SqliteFailure(ffi::Error::new(code), Some("op failed".to_string()))
                .into()
        }
        for code in [
            ffi::SQLITE_BUSY,
            ffi::SQLITE_LOCKED,
            ffi::SQLITE_FULL,
            ffi::SQLITE_INTERRUPT,
        ] {
            assert!(
                is_retryable_db_error(&typed(code)),
                "code {code} must retry"
            );
        }
        for code in [ffi::SQLITE_CORRUPT, ffi::SQLITE_READONLY, ffi::SQLITE_ERROR] {
            assert!(
                !is_retryable_db_error(&typed(code)),
                "code {code} must fail fast into the bounded retry"
            );
        }
        let wrapped = typed(ffi::SQLITE_BUSY).context("removing files");
        assert!(is_retryable_db_error(&wrapped));
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

        let mut stale = None;
        assert_eq!(
            reindex_one(&config, Path::new("foo.rs"), false, false, &mut stale),
            WorkOutcome::Done
        );
        assert!(stale.is_none());
        assert!(structural_hash_is_fresh(&config, "foo.rs", &hash));

        // A held lock proves the fresh structural path avoids reacquiring it.
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

        assert!(!should_defer(
            Some("busy"),
            &mut since,
            t0 + BACKPRESSURE_MAX_DEFER
        ));
        assert!(since.is_none());

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

        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let gitdir = tmp.path().join("real-gitdir");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        assert!(!git_index_lock_present(&wt));
        std::fs::write(gitdir.join("index.lock"), "").unwrap();
        assert!(git_index_lock_present(&wt));

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
        let mut semantic_retries = HashMap::new();
        let mut parked = HashMap::new();
        let mut deferred_since = None;

        let rederive = drain_pending(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &mut semantic_retries,
            &mut parked,
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

        std::fs::remove_file(root.join(".git/index.lock")).unwrap();
        pending.insert(PathBuf::from("foo.rs"), stale);
        let mut config = config;
        // Disable live-host probes after removing the deterministic pressure signal.
        config.backpressure = false;
        drain_pending(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &mut semantic_retries,
            &mut parked,
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
    fn semantic_retry_delay_doubles_from_one_debounce_and_caps() {
        let debounce = Duration::from_secs(30);
        assert_eq!(semantic_retry_extra_delay(1, debounce), Duration::ZERO);
        assert_eq!(
            semantic_retry_extra_delay(2, debounce),
            Duration::from_secs(30)
        );
        assert_eq!(
            semantic_retry_extra_delay(3, debounce),
            Duration::from_secs(90)
        );
        assert_eq!(
            semantic_retry_extra_delay(4, debounce),
            Duration::from_secs(210)
        );
        assert_eq!(
            semantic_retry_extra_delay(5, debounce),
            Duration::from_secs(450)
        );
        assert_eq!(
            semantic_retry_extra_delay(40, debounce),
            MAX_SEMANTIC_RETRY_EXTRA,
            "the extra wait is bounded"
        );
    }

    #[test]
    fn requeue_failed_semantic_bounds_the_retries_and_then_parks_the_path() {
        let debounce = Duration::from_millis(100);
        let now = Instant::now();
        let mut pending = HashMap::new();
        let mut retries = HashMap::new();
        let path = PathBuf::from("foo.rs");

        for attempt in 1..=MAX_SEMANTIC_RETRIES {
            requeue_failed_semantic(&mut pending, &mut retries, [path.clone()], debounce, now);
            assert_eq!(retries.get(&path), Some(&attempt));
            let stamp = pending.get(&path).copied().expect("re-queued");
            assert_eq!(
                stamp,
                now + semantic_retry_extra_delay(attempt, debounce),
                "attempt {attempt} waits its backoff"
            );
            assert!(compute_ready(&pending, now, debounce).is_empty());
            assert_eq!(
                compute_ready(&pending, stamp + debounce, debounce),
                vec![path.clone()],
                "attempt {attempt} becomes ready once its window has passed"
            );
            pending.remove(&path);
        }

        let parked =
            requeue_failed_semantic(&mut pending, &mut retries, [path.clone()], debounce, now);
        assert_eq!(
            parked,
            vec![path.clone()],
            "the caller parks what comes back"
        );
        assert!(
            !pending.contains_key(&path),
            "past the bound the path is parked, not spun on"
        );
        assert!(
            !retries.contains_key(&path),
            "the counter is cleared so the next save starts fresh"
        );
    }

    #[test]
    fn revive_due_parked_revives_only_entries_past_the_interval() {
        let now = Instant::now();
        let mut parked = HashMap::new();
        let mut retries = HashMap::new();
        let mut pending = HashMap::new();
        let fresh = PathBuf::from("fresh.rs");
        let due = PathBuf::from("due.rs");
        parked.insert(fresh.clone(), now);
        parked.insert(
            due.clone(),
            now - PARKED_RETRY_INTERVAL - Duration::from_secs(60),
        );
        retries.insert(due.clone(), MAX_SEMANTIC_RETRIES);

        assert_eq!(
            revive_due_parked(&mut parked, &mut retries, &mut pending, now),
            1
        );
        assert!(parked.contains_key(&fresh), "fresh entries stay parked");
        assert!(!parked.contains_key(&due), "due entries leave the set");
        assert_eq!(
            pending.get(&due),
            Some(&now),
            "revived paths are immediately drainable"
        );
        assert!(
            !retries.contains_key(&due),
            "revival grants a fresh retry budget"
        );
    }

    #[test]
    fn revive_due_parked_with_nothing_parked_revives_nothing() {
        let now = Instant::now();
        let mut parked = HashMap::new();
        let mut retries = HashMap::new();
        let mut pending = HashMap::new();
        assert_eq!(
            revive_due_parked(&mut parked, &mut retries, &mut pending, now),
            0
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn debounce_floor_clamps_sub_floor_windows() {
        assert_eq!(
            floor_debounce_ms(0),
            MIN_DEBOUNCE_MS,
            "a zero flag must batch, not re-index hot"
        );
        assert_eq!(floor_debounce_ms(MIN_DEBOUNCE_MS - 1), MIN_DEBOUNCE_MS);
        assert_eq!(floor_debounce_ms(MIN_DEBOUNCE_MS), MIN_DEBOUNCE_MS);
        assert_eq!(
            floor_debounce_ms(DEFAULT_DEBOUNCE_MS),
            DEFAULT_DEBOUNCE_MS,
            "above the floor passes through untouched"
        );
    }

    #[test]
    fn process_ready_requeues_the_path_when_the_semantic_pass_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::write(root.join("foo.rs"), "fn main() {}\n").unwrap();
        let mut config = test_config(root);
        let provider: EmbedderProvider =
            Arc::new(|| anyhow::bail!("simulated embedder load failure"));
        config.embedder = Some(provider);
        let filter = WatchFilter::new(root, &config.exclude_patterns).unwrap();
        let mut embedder = EmbedderHandle::new(config.embedder.clone());

        let debounce = Duration::from_millis(config.debounce_ms);
        let stale = Instant::now() - Duration::from_secs(10);
        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("foo.rs"), stale);
        let mut currently_indexing = HashSet::new();
        let mut recheck_queue = HashSet::new();
        let mut semantic_retries = HashMap::new();

        let mut parked = HashMap::new();
        let before = Instant::now();
        process_ready(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &mut semantic_retries,
            &mut parked,
            &filter,
            &mut embedder,
            false,
            vec![PathBuf::from("foo.rs")],
        );

        let db = Database::open(&config.db_path).unwrap();
        assert!(
            db.get_file_hash("foo.rs").unwrap().is_some(),
            "the structural pass must still land"
        );
        drop(db);
        let stamp = pending
            .get(Path::new("foo.rs"))
            .copied()
            .expect("a failed semantic pass must keep the path pending");
        assert!(stamp >= before, "re-stamped, not left drain-ready");
        assert_eq!(semantic_retries.get(Path::new("foo.rs")), Some(&1));
        assert!(
            compute_ready(&pending, stamp + debounce, debounce).contains(&PathBuf::from("foo.rs")),
            "retried on the next tick after one debounce"
        );

        process_ready(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &mut semantic_retries,
            &mut parked,
            &filter,
            &mut embedder,
            false,
            vec![PathBuf::from("foo.rs")],
        );
        let second = pending
            .get(Path::new("foo.rs"))
            .copied()
            .expect("still pending after the second failure");
        assert_eq!(semantic_retries.get(Path::new("foo.rs")), Some(&2));
        assert!(
            second >= before + debounce,
            "second retry waits at least one extra debounce"
        );
        assert!(currently_indexing.is_empty());
    }

    #[test]
    fn successful_bulk_recovery_clears_semantic_retry_and_parked_obligations() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        for name in ["retry.rs", "parked.rs"] {
            std::fs::write(root.join(name), "fn recovery_subject() {}\n").unwrap();
        }
        let mut config = test_config(root);
        config.embedder = Some(Arc::new(|| anyhow::bail!("embedder unavailable")));
        let filter = WatchFilter::new(root, &config.exclude_patterns).unwrap();
        let mut embedder = EmbedderHandle::new(config.embedder.clone());
        let mut pending = HashMap::new();
        let mut indexing = HashSet::new();
        let mut recheck = HashSet::new();
        let mut retries = HashMap::new();
        let mut parked = HashMap::new();
        for _ in 0..=MAX_SEMANTIC_RETRIES {
            process_ready(
                &config,
                &mut pending,
                &mut indexing,
                &mut recheck,
                &mut retries,
                &mut parked,
                &filter,
                &mut embedder,
                false,
                vec![PathBuf::from("parked.rs")],
            );
        }
        process_ready(
            &config,
            &mut pending,
            &mut indexing,
            &mut recheck,
            &mut retries,
            &mut parked,
            &filter,
            &mut embedder,
            false,
            vec![PathBuf::from("retry.rs")],
        );
        assert_eq!(retries.get(Path::new("retry.rs")), Some(&1));
        assert!(parked.contains_key(Path::new("parked.rs")));
        assert!(pending.contains_key(Path::new("retry.rs")));
        let retained_retries = retries.clone();
        let retained_parked = parked.clone();
        let mut deferred = None;
        let lock = hold_lock(root);
        assert_eq!(
            run_bulk_guarded(
                &config,
                &mut embedder,
                &mut deferred,
                &mut retries,
                &mut parked
            ),
            WorkOutcome::Skipped
        );
        assert_eq!(retries, retained_retries);
        assert_eq!(parked, retained_parked);
        drop(lock);
        assert_eq!(
            run_bulk_guarded(
                &config,
                &mut embedder,
                &mut deferred,
                &mut retries,
                &mut parked
            ),
            WorkOutcome::Failed
        );
        assert_eq!(retries, retained_retries);
        assert_eq!(parked, retained_parked);

        config.embedder = None;
        let mut embedder = EmbedderHandle::new(None);
        let outcome = run_bulk_guarded(
            &config,
            &mut embedder,
            &mut deferred,
            &mut retries,
            &mut parked,
        );
        assert_eq!(outcome, WorkOutcome::Done);
        let mut removed = Vec::new();
        let bulk_retry = apply_bulk_outcome(
            outcome,
            &mut pending,
            &mut removed,
            Instant::now(),
            Duration::from_millis(config.debounce_ms),
            None,
        );
        assert!(bulk_retry.is_none());
        assert!(pending.is_empty());
        assert!(retries.is_empty());
        assert!(parked.is_empty());
        assert!(indexing.is_empty());
        assert!(recheck.is_empty());
    }

    #[test]
    fn bulk_incremental_with_a_failing_embedder_is_failed_and_keeps_the_paths_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::write(root.join("foo.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("bar.rs"), "fn bar() {}\n").unwrap();
        let mut config = test_config(root);
        let provider: EmbedderProvider =
            Arc::new(|| anyhow::bail!("simulated embedder load failure"));
        config.embedder = Some(provider);
        let mut embedder = EmbedderHandle::new(config.embedder.clone());

        let outcome = run_bulk_incremental(&config, &mut embedder);
        assert_eq!(
            outcome,
            WorkOutcome::Failed,
            "an embedder that does not load is not a completed semantic pass"
        );
        let db = Database::open(&config.db_path).unwrap();
        assert!(
            db.get_file_hash("foo.rs").unwrap().is_some(),
            "the structural pass must still land"
        );
        drop(db);

        let debounce = Duration::from_millis(config.debounce_ms);
        let now = Instant::now();
        let stale = now - Duration::from_secs(10);
        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("foo.rs"), stale);
        pending.insert(PathBuf::from("bar.rs"), stale);
        let mut removed = Vec::new();
        let retry = apply_bulk_outcome(outcome, &mut pending, &mut removed, now, debounce, None);
        assert_eq!(retry, None);
        assert_eq!(pending.len(), 2, "a failed bulk pass drops no path");
        assert_eq!(compute_ready(&pending, now, debounce).len(), 2);
        assert_eq!(
            bulk_cooldown_after(outcome, now),
            None,
            "a failed pass arms no cooldown"
        );
    }

    #[test]
    fn bulk_incremental_with_semantic_disabled_is_done() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::write(root.join("foo.rs"), "fn main() {}\n").unwrap();
        let config = test_config(root);
        let mut embedder = EmbedderHandle::new(None);

        let outcome = run_bulk_incremental(&config, &mut embedder);
        assert_eq!(
            outcome,
            WorkOutcome::Done,
            "nothing to embed is a completed pass"
        );

        let debounce = Duration::from_millis(config.debounce_ms);
        let now = Instant::now();
        let mut pending = HashMap::new();
        pending.insert(PathBuf::from("foo.rs"), now - Duration::from_secs(10));
        let mut removed = Vec::new();
        apply_bulk_outcome(outcome, &mut pending, &mut removed, now, debounce, None);
        assert!(pending.is_empty());
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
        let mut semantic_retries = HashMap::new();

        let mut parked = HashMap::new();
        let _held = hold_lock(root);
        process_ready(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &mut semantic_retries,
            &mut parked,
            &filter,
            &mut embedder,
            false,
            vec![PathBuf::from("foo.rs")],
        );

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
        let mut semantic_retries = HashMap::new();

        let mut parked = HashMap::new();
        let rederive = process_ready(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &mut semantic_retries,
            &mut parked,
            &filter,
            &mut embedder,
            true,
            vec![PathBuf::from("main.cpp")],
        );
        assert!(!rederive);

        std::fs::remove_file(root.join("main.cpp")).unwrap();
        let rederive = process_ready(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &mut semantic_retries,
            &mut parked,
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
        // Only the Rescan flag is portable; inotify and FSEvents attach different paths.
        let overflow = Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        assert!(overflow.need_rescan());
        assert!(!Event::new(EventKind::Other).need_rescan());
    }

    #[test]
    fn dir_adoption_matches_create_and_rename_events() {
        use notify::event::{CreateKind, DataChange, MetadataKind, RemoveKind, RenameMode};

        assert!(is_dir_adoption_kind(&EventKind::Create(CreateKind::Folder)));
        assert!(is_dir_adoption_kind(&EventKind::Create(CreateKind::File)));
        assert!(is_dir_adoption_kind(&EventKind::Modify(ModifyKind::Name(
            RenameMode::To
        ))));
        assert!(is_dir_adoption_kind(&EventKind::Modify(ModifyKind::Name(
            RenameMode::Both
        ))));
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
        let gone = tmp.path().join("gone");
        assert_eq!(canonical_root(&gone), gone);
    }
}
