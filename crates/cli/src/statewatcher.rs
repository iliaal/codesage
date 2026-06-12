use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use codesage_embed::config::EmbeddingConfig;
use codesage_embed::model::Embedder;
use codesage_graph::{
    index_files, list_dependencies, remove_files, semantic_index_files, semantic_remove_files,
};
use codesage_parser::detect::detect_language;
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
const DEFAULT_IDLE_SECS: u64 = 1800;

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

/// Caches the result of the embedder provider so the model is resolved at most
/// once, and only when a semantic reindex actually needs it (an idle watcher
/// never triggers a model load).
struct EmbedderHandle {
    provider: Option<EmbedderProvider>,
    cached: Option<Arc<Mutex<Embedder>>>,
}

impl EmbedderHandle {
    fn new(provider: Option<EmbedderProvider>) -> Self {
        Self {
            provider,
            cached: None,
        }
    }

    fn enabled(&self) -> bool {
        self.provider.is_some()
    }

    fn get(&mut self) -> Option<Arc<Mutex<Embedder>>> {
        if self.cached.is_none() {
            let provider = self.provider.as_ref()?;
            match provider() {
                Ok(emb) => self.cached = Some(emb),
                Err(e) => {
                    tracing::warn!(error = %e, "loading embedder for watcher");
                    return None;
                }
            }
        }
        self.cached.clone()
    }
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

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
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
    let mut last_activity = Instant::now();

    tracing::info!(
        root = %config.project_root.display(),
        debounce_ms = config.debounce_ms,
        mode = ?config.mode,
        "statewatcher started"
    );

    let exit_reason = loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(event) => {
                last_activity = Instant::now();
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
            Err(mpsc::RecvTimeoutError::Disconnected) => break "watcher channel closed",
        }

        if config.shutdown.load(Ordering::Relaxed) {
            tracing::info!("shutdown requested, draining pending work");
            drain_pending_force(
                &config,
                &mut pending,
                &mut currently_indexing,
                &mut recheck_queue,
                &filter,
                &mut embedder,
            );
            break "shutdown";
        }

        if disabled_marker.exists() {
            break "disabled marker present";
        }

        drain_pending(
            &config,
            &mut pending,
            &mut currently_indexing,
            &mut recheck_queue,
            &filter,
            &mut embedder,
            debounce,
        );

        if !pending.is_empty() || !currently_indexing.is_empty() || !recheck_queue.is_empty() {
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

fn drain_pending(
    config: &StateWatcherConfig,
    pending: &mut HashMap<PathBuf, Instant>,
    currently_indexing: &mut HashSet<PathBuf>,
    recheck_queue: &mut HashSet<PathBuf>,
    filter: &WatchFilter,
    embedder: &mut EmbedderHandle,
    debounce: Duration,
) {
    let ready = compute_ready(pending, Instant::now(), debounce);
    process_ready(
        config,
        pending,
        currently_indexing,
        recheck_queue,
        filter,
        embedder,
        ready,
    );
}

fn drain_pending_force(
    config: &StateWatcherConfig,
    pending: &mut HashMap<PathBuf, Instant>,
    currently_indexing: &mut HashSet<PathBuf>,
    recheck_queue: &mut HashSet<PathBuf>,
    filter: &WatchFilter,
    embedder: &mut EmbedderHandle,
) {
    let ready: Vec<PathBuf> = pending.keys().cloned().collect();
    process_ready(
        config,
        pending,
        currently_indexing,
        recheck_queue,
        filter,
        embedder,
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

#[allow(clippy::too_many_arguments)]
fn process_ready(
    config: &StateWatcherConfig,
    pending: &mut HashMap<PathBuf, Instant>,
    currently_indexing: &mut HashSet<PathBuf>,
    recheck_queue: &mut HashSet<PathBuf>,
    filter: &WatchFilter,
    embedder: &mut EmbedderHandle,
    ready: Vec<PathBuf>,
) {
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
        if embedder.enabled()
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

fn reindex_one(config: &StateWatcherConfig, rel: &Path, embedder: &mut EmbedderHandle) {
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
                return;
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

    let db = match Database::open(&config.db_path) {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!(error = %e, "opening DB for removal");
            return;
        }
    };

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

    let _ = semantic_remove_files(&db, paths);
}

fn run_bulk_incremental(config: &StateWatcherConfig, embedder: &mut EmbedderHandle) {
    let _lock = match lockfile::try_acquire(&config.project_root) {
        Ok(lockfile::LockOutcome::Acquired(lock)) => Some(lock),
        _ => {
            tracing::debug!("skipping bulk incremental: index lock held");
            return;
        }
    };

    let db = match Database::open(&config.db_path) {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!(error = %e, "opening DB for bulk incremental");
            return;
        }
    };

    if let Err(e) = codesage_graph::incremental_index(
        &config.project_root,
        &db,
        &config.exclude_patterns,
        false,
    ) {
        tracing::warn!(error = %e, "bulk incremental structural reindex failed");
        return;
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
                return;
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
        }
    }
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
