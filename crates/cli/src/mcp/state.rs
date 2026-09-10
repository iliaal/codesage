use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use codesage_embed::config::EmbeddingConfig;
use codesage_embed::model::Embedder;
use codesage_embed::reranker::Reranker;
use codesage_storage::Database;
use parking_lot::Mutex;

use super::CodeSageServer;
use super::params::{EmbedTextsResult, RerankPairsParams, RerankPairsResult};

const MCP_TEST_QUERY_EMBEDDING_ENV: &str = "CODESAGE_MCP_TEST_QUERY_EMBEDDING";

/// Bound debug-override dimensions before selecting a chunk table.
const MAX_TEST_QUERY_EMBEDDING_DIM: usize = 4096;

#[derive(Debug, Clone)]
pub(super) struct ProjectState {
    pub(super) db_path: PathBuf,
    pub(super) embedding_config: EmbeddingConfig,
    embedding_config_error: Option<String>,
    /// `[index] watch` from the project config: `Some(true)` asks for a live
    /// watcher on the first call of any kind, `Some(false)` refuses one, and
    /// `None` (the default) starts it on the first semantic query only.
    watch: Option<bool>,
    exclude_patterns: Vec<String>,
    /// Revalidate each resolution so config edits, creation, and deletion invalidate cached state.
    config_mtime: Option<std::time::SystemTime>,
}

impl ProjectState {
    fn config_path(&self) -> Option<PathBuf> {
        self.db_path.parent().map(|dir| dir.join("config.toml"))
    }

    fn config_changed(&self) -> bool {
        match self.config_path() {
            Some(path) => config_toml_mtime(&path) != self.config_mtime,
            None => true,
        }
    }

    /// A reset deletes index.db; cached state must not recreate an empty index.
    fn still_valid(&self) -> bool {
        !self.config_changed() && self.db_path.exists()
    }
}

fn config_toml_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[derive(Debug)]
struct LoadedEmbeddingConfig {
    config: EmbeddingConfig,
    semantic_error: Option<String>,
    watch: Option<bool>,
    exclude_patterns: Vec<String>,
}

/// Start on semantic queries unless explicitly configured; structural calls still
/// reconcile existing watchers and honor opt-out.
fn watcher_start_wanted(config_watch: Option<bool>, semantic_query: bool) -> bool {
    match config_watch {
        Some(true) => true,
        Some(false) => false,
        None => semantic_query,
    }
}

/// Per-key locks serialize cold loads without blocking unrelated model keys.
type ModelSlot<T> = Arc<Mutex<Option<Arc<Mutex<T>>>>>;

/// The enclosing map mutex protects `last_used`.
struct ModelEntry<T> {
    slot: ModelSlot<T>,
    last_used: Instant,
}

type ModelMap<T> = Mutex<HashMap<String, ModelEntry<T>>>;

fn model_lock<T>(mutex: &Mutex<T>) -> Result<parking_lot::MutexGuard<'_, T>> {
    loop {
        codesage_protocol::work::checkpoint()?;
        if let Some(guard) = mutex.try_lock_for(Duration::from_millis(25)) {
            codesage_protocol::work::checkpoint()?;
            return Ok(guard);
        }
    }
}

/// Hold the map lock only for lookup; load under the per-key lock to prevent duplicate sessions.
fn get_or_load_slot<T, F>(map: &ModelMap<T>, key: String, load: F) -> Result<Arc<Mutex<T>>>
where
    F: FnOnce() -> Result<T>,
{
    let slot = {
        let mut guard = map.lock();
        let entry = guard.entry(key).or_insert_with(|| ModelEntry {
            slot: Arc::new(Mutex::new(None)),
            last_used: Instant::now(),
        });
        entry.last_used = Instant::now();
        entry.slot.clone()
    };
    let mut slot_guard = model_lock(&slot)?;
    if let Some(arc) = slot_guard.as_ref() {
        return Ok(arc.clone());
    }
    let value = load()?;
    let arc = Arc::new(Mutex::new(value));
    *slot_guard = Some(arc.clone());
    Ok(arc)
}

/// Evict idle models only when the pool owns the last reference. Skip busy load
/// slots and drop models outside the map lock because CUDA teardown can block.
fn evict_idle_from_map<T>(map: &ModelMap<T>, timeout: Duration) -> usize {
    let mut taken: Vec<Arc<Mutex<T>>> = Vec::new();
    {
        let mut guard = map.lock();
        guard.retain(|_key, entry| {
            if entry.last_used.elapsed() < timeout {
                return true;
            }
            let Some(mut slot_guard) = entry.slot.try_lock() else {
                return true; // busy (loading or cloning) — leave it
            };
            match slot_guard.as_ref() {
                Some(arc) if Arc::strong_count(arc) == 1 => {
                    taken.push(slot_guard.take().expect("just matched Some"));
                    false // drop the now-empty entry from the map
                }
                _ => true, // unloaded already, or a call holds a clone
            }
        });
    }
    let count = taken.len();
    drop(taken); // ORT Session teardown happens here, outside the map lock
    count
}

pub(crate) struct CodeSageServerState {
    pub(super) diagnostics: super::diagnostics::Diagnostics,
    pub(super) work: super::work::WorkCoordinator,
    pub(super) overview_cache: super::overview_cache::OverviewCache,
    pub(super) overview_cache_enabled: bool,
    projects: Mutex<HashMap<PathBuf, ProjectState>>,
    /// Raw-path cache avoids repeated canonicalization; `projects` deduplicates canonical roots.
    resolved: Mutex<HashMap<String, ProjectState>>,
    embedders: ModelMap<Embedder>,
    rerankers: ModelMap<Reranker>,
    /// One watcher per canonical project root, started lazily and reaped on shutdown.
    watchers: Mutex<HashMap<PathBuf, WatcherEntry>>,
    watcher_lifecycle: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

/// A signalled but alive watcher retains its slot while draining. A restart waits
/// for exit to avoid overlapping indexing and status-file writes.
struct WatcherEntry {
    shutdown: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    config_key: String,
    thread: Option<std::thread::JoinHandle<()>>,
    /// First observed stop; bounds how long a wedged thread reserves its slot.
    stopping_since: Option<Instant>,
}

/// Bound restart waits inside tool calls; later calls retry.
const WATCHER_RESTART_WAIT: Duration = Duration::from_secs(5);

/// Detach long-wedged watchers so one stuck drain cannot permanently disable reindexing.
/// The detached thread retains its own AliveGuard.
const WATCHER_STUCK_FORCE_DETACH: Duration = Duration::from_secs(15 * 60);

/// How long the last-client stop waits for each watcher to finish its drain
/// before leaving its slot in place as still-stopping.
pub(crate) const WATCHER_STOP_WAIT: Duration = Duration::from_secs(60);

const WATCHER_EXIT_POLL: Duration = Duration::from_millis(20);

fn wait_for_watcher_exit(alive: &AtomicBool, deadline: Instant) -> bool {
    loop {
        if !alive.load(Ordering::SeqCst) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(WATCHER_EXIT_POLL.min(deadline - now));
    }
}

/// Signal all watchers and share one wait deadline. Timed-out slots retain their
/// join handles and remain unavailable for replacement.
fn stop_all_watchers(watchers: &Mutex<HashMap<PathBuf, WatcherEntry>>, wait: Duration) -> usize {
    stop_all_watchers_if(watchers, wait, || true).expect("an unconditional stop never aborts")
}

/// Check `still_wanted` under the registry lock before signalling. This closes the
/// race between last-client disconnect and a new client finding the old watcher.
fn stop_all_watchers_if(
    watchers: &Mutex<HashMap<PathBuf, WatcherEntry>>,
    wait: Duration,
    still_wanted: impl FnOnce() -> bool,
) -> Option<usize> {
    let deadline = Instant::now() + wait;
    let mut stopping = Vec::new();
    {
        let mut guard = watchers.lock();
        if !still_wanted() {
            tracing::info!("watcher stop aborted: a client connected before it landed");
            return None;
        }
        for (root, entry) in guard.iter_mut() {
            entry.shutdown.store(true, Ordering::SeqCst);
            tracing::info!(root = %root.display(), "signalling watcher shutdown");
            stopping.push((root.clone(), entry.alive.clone(), entry.thread.take()));
        }
    }
    let mut still_stopping = 0;
    for (root, alive, thread) in stopping {
        if wait_for_watcher_exit(&alive, deadline) {
            if let Some(handle) = thread
                && handle.join().is_err()
            {
                tracing::warn!(root = %root.display(), "watcher thread panicked during shutdown");
            }
            let mut guard = watchers.lock();
            // Only the slot this stop owns: a start request may already have
            // replaced a dead entry with a fresh watcher.
            if guard
                .get(&root)
                .is_some_and(|entry| Arc::ptr_eq(&entry.alive, &alive))
            {
                guard.remove(&root);
            }
            tracing::info!(root = %root.display(), "watcher stopped");
        } else {
            still_stopping += 1;
            tracing::warn!(
                root = %root.display(),
                waited_secs = wait.as_secs(),
                "watcher still draining after the stop wait; its slot stays reserved until it exits"
            );
            let mut guard = watchers.lock();
            if let Some(entry) = guard
                .get_mut(&root)
                .filter(|entry| Arc::ptr_eq(&entry.alive, &alive))
            {
                entry.thread = thread;
            }
        }
    }
    Some(still_stopping)
}

#[derive(Debug, PartialEq, Eq)]
enum WatcherSlot {
    /// A live watcher with this config already owns the slot.
    Running,
    /// The slot is reserved for the caller, who must spawn into it (or let
    /// its [`WatcherReservation`] release it).
    Reserved,
    /// A watcher on this root is still stopping past the wait. Nothing was
    /// reserved; the caller must not spawn. A later call retries.
    StillStopping,
}

/// Retire stale configurations and wait for draining watchers before reserving.
/// Overlapping watchers duplicate indexing and race on the status file.
fn reserve_watcher_slot(
    watchers: &Mutex<HashMap<PathBuf, WatcherEntry>>,
    root: &Path,
    config_key: &str,
    shutdown: &Arc<AtomicBool>,
    alive: &Arc<AtomicBool>,
    wait: Duration,
) -> WatcherSlot {
    let deadline = Instant::now() + wait;
    loop {
        let mut guard = watchers.lock();
        if let Some(entry) = guard.get_mut(root)
            && entry.alive.load(Ordering::SeqCst)
        {
            if !entry.shutdown.load(Ordering::SeqCst) {
                if entry.config_key == config_key {
                    return WatcherSlot::Running;
                }
                entry.shutdown.store(true, Ordering::SeqCst);
            }
            let old_alive = entry.alive.clone();
            let thread = entry.thread.take();
            drop(guard);
            if !wait_for_watcher_exit(&old_alive, deadline) {
                let mut guard = watchers.lock();
                // Restore the handle only if this is still our slot; detach only past the wedge timeout.
                let wedged = match guard
                    .get_mut(root)
                    .filter(|entry| Arc::ptr_eq(&entry.alive, &old_alive))
                {
                    Some(entry) => {
                        entry.thread = thread;
                        let since = *entry.stopping_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= WATCHER_STUCK_FORCE_DETACH {
                            guard.remove(root);
                            tracing::error!(
                                root = %root.display(),
                                stuck_secs = since.elapsed().as_secs(),
                                "watcher wedged in shutdown; detaching its slot so a fresh watcher can spawn"
                            );
                            true
                        } else {
                            false
                        }
                    }
                    // Another stop path already reaped the slot; nothing to
                    // spawn into from this call.
                    None => return WatcherSlot::StillStopping,
                };
                drop(guard);
                if wedged {
                    continue;
                }
                return WatcherSlot::StillStopping;
            }
            if let Some(handle) = thread
                && handle.join().is_err()
            {
                tracing::warn!(root = %root.display(), "watcher thread panicked before restart");
            }
            // Re-check under the lock: another start request may have
            // replaced the dead entry while this one waited.
            continue;
        }
        guard.insert(
            root.to_path_buf(),
            WatcherEntry {
                shutdown: shutdown.clone(),
                alive: alive.clone(),
                config_key: config_key.to_string(),
                thread: None,
                stopping_since: None,
            },
        );
        return WatcherSlot::Reserved;
    }
}

/// Clear liveness even when the watcher unwinds, allowing later calls to respawn it.
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Release failed spawns' reservations; token identity prevents removing a replacement.
struct WatcherReservation<'a> {
    watchers: &'a Mutex<HashMap<PathBuf, WatcherEntry>>,
    root: PathBuf,
    token: Arc<AtomicBool>,
    armed: bool,
}

impl WatcherReservation<'_> {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for WatcherReservation<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut watchers = self.watchers.lock();
        if watchers
            .get(&self.root)
            .is_some_and(|e| Arc::ptr_eq(&e.shutdown, &self.token))
        {
            watchers.remove(&self.root);
        }
    }
}

/// Retire watchers when embedding identity or exclusions change. Use cached artifact
/// stat metadata, not hashing or downloads, on this per-call path. First load replaces
/// `uncached` and causes one restart.
fn watcher_config_key(state: &ProjectState) -> String {
    let embedding =
        if state.embedding_config.model.is_empty() || state.embedding_config_error.is_some() {
            "structural-only".to_string()
        } else {
            let identity = cached_artifact_identity(&state.embedding_config.model);
            watcher_key(&state.embedding_config, &identity)
        };
    format!("{embedding}|excludes:{:?}", state.exclude_patterns)
}

fn watcher_key(config: &EmbeddingConfig, artifact_identity: &str) -> String {
    match embedder_pool_key(config, artifact_identity) {
        Ok(key) => key,
        Err(e) => format!("invalid-config|{e}"),
    }
}

/// Path/size/mtime of the model files already in the local cache; a label
/// when they are not there or cannot be stat'ed. Never downloads or reads.
fn cached_artifact_identity(model: &str) -> String {
    match codesage_embed::model::cached_model_artifacts(model) {
        Some(artifacts) => artifacts
            .stat_key()
            .unwrap_or_else(|| "unreadable".to_string()),
        None => "uncached".to_string(),
    }
}

/// Include pooling and artifact digest: the same model name may produce incompatible vectors.
fn embedder_pool_key(config: &EmbeddingConfig, artifact_digest: &str) -> Result<String> {
    let batch_size = config.effective_batch_size()?;
    Ok(format!(
        "{}|{}|{}|{:?}|{artifact_digest}",
        config.model,
        config.device,
        batch_size.get(),
        config.pooling_strategy()
    ))
}

/// [`embedder_pool_key`] over the model files a load would open, resolving
/// them (downloading on a cache miss) exactly as `Embedder::new` is about to.
fn resolved_embedder_pool_key(config: &EmbeddingConfig) -> Result<String> {
    let artifacts = codesage_embed::model::resolve_model_artifacts(&config.model)
        .with_context(|| format!("resolving model files for {:?}", config.model))?;
    let digest = codesage_embed::fingerprint::model_artifact_digest(&artifacts)?;
    embedder_pool_key(config, &digest)
}

/// Include the resident execution provider; artifact digests are cached by pool-key resolution.
fn session_fingerprint(
    config: &EmbeddingConfig,
    embedder: &Embedder,
) -> Result<codesage_graph::SemanticFingerprint> {
    Ok(
        codesage_graph::SemanticFingerprint::compute(config, embedder.dim())?
            .with_execution_provider(embedder.execution_provider()),
    )
}

/// Non-empty batches must attest this session's fingerprint. Empty probes may omit it.
/// The mismatch marker tells clients to abort rather than fall back privately.
fn check_expected_fingerprint(expected: Option<&str>, produces: &str, probe: bool) -> Result<()> {
    match expected {
        Some(expected) if expected != produces => bail!(
            "{} daemon session produces {produces:?}, caller attests {expected:?}",
            super::EMBED_TEXTS_FINGERPRINT_MISMATCH
        ),
        None if !probe => bail!(
            "{} caller sent no fingerprint with a non-empty request; daemon session produces \
             {produces:?}",
            super::EMBED_TEXTS_FINGERPRINT_MISMATCH
        ),
        _ => Ok(()),
    }
}

impl CodeSageServerState {
    pub(crate) fn new() -> Self {
        Self {
            diagnostics: super::diagnostics::Diagnostics::new(
                std::env::var("CODESAGE_DIAGNOSTICS").as_deref() != Ok("0"),
            ),
            work: super::work::WorkCoordinator::new(super::work::WorkLimits::default())
                .expect("built-in work limits are valid"),
            overview_cache: super::overview_cache::OverviewCache::default(),
            overview_cache_enabled: std::env::var("CODESAGE_OVERVIEW_CACHE").as_deref() != Ok("0"),
            projects: Mutex::new(HashMap::new()),
            resolved: Mutex::new(HashMap::new()),
            embedders: Mutex::new(HashMap::new()),
            rerankers: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
            watcher_lifecycle: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn shutdown_work(&self) {
        self.work.shutdown();
    }

    pub(crate) fn active_work(&self) -> usize {
        let snapshot = self.work.snapshot();
        snapshot.running.iter().sum::<usize>() + snapshot.queued.iter().sum::<usize>()
    }

    /// Evict unused models without touching in-flight references. Call malloc_trim once
    /// after the sweep, not per model.
    pub(crate) fn evict_idle_models(&self, timeout: Duration) -> usize {
        evict_idle_from_map(&self.embedders, timeout)
            + evict_idle_from_map(&self.rerankers, timeout)
    }

    /// Stop and join watchers within `wait`; timed-out slots stay reserved.
    /// Blocks: call outside async workers.
    pub(crate) fn shutdown_all_watchers(&self, wait: Duration) -> usize {
        stop_all_watchers(&self.watchers, wait)
    }

    /// Abort without signalling if a new client arrived before the registry lock was acquired.
    pub(crate) fn shutdown_watchers_if_no_client(
        &self,
        wait: Duration,
        active_clients: &std::sync::atomic::AtomicUsize,
    ) -> Option<usize> {
        stop_all_watchers_if(&self.watchers, wait, || {
            active_clients.load(Ordering::SeqCst) == 0
        })
    }
}

impl CodeSageServer {
    pub(super) fn resolve_project(&self, project: &str) -> Result<ProjectState> {
        let state = self.resolve_project_inner(project)?;
        self.maybe_start_watcher(&state, false);
        Ok(state)
    }

    fn maybe_start_watcher(&self, state: &ProjectState, semantic_query: bool) {
        let Some(root) = state.db_path.parent().and_then(|p| p.parent()) else {
            return;
        };
        let lifecycle = self
            .state
            .watcher_lifecycle
            .lock()
            .entry(root.to_path_buf())
            .or_default()
            .clone();
        let _lifecycle = lifecycle.lock();
        if !state.db_path.exists() {
            self.stop_project_watcher(root);
            return;
        }
        // A query may have resolved the old config before another query
        // stopped its watcher. Never let that delayed query restore it.
        let refreshed;
        let state = if state.still_valid() {
            state
        } else {
            match self.resolve_project_inner(&root.to_string_lossy()) {
                Ok(current) => {
                    refreshed = current;
                    &refreshed
                }
                Err(error) => {
                    tracing::warn!(%error, root = %root.display(), "could not refresh watcher config");
                    self.stop_project_watcher(root);
                    return;
                }
            }
        };
        if !crate::statewatcher::watch_enabled(root, state.watch)
            || state.embedding_config_error.is_some()
        {
            self.stop_project_watcher(root);
            return;
        }
        let already_started = self
            .state
            .watchers
            .lock()
            .get(root)
            .is_some_and(|entry| entry.alive.load(Ordering::SeqCst));
        if watcher_start_wanted(state.watch, semantic_query) || already_started {
            self.ensure_watcher(root, state);
        }
    }

    fn stop_project_watcher(&self, root: &Path) {
        let (alive, thread) = {
            let mut watchers = self.state.watchers.lock();
            let Some(entry) = watchers.get_mut(root) else {
                return;
            };
            entry.shutdown.store(true, Ordering::SeqCst);
            (entry.alive.clone(), entry.thread.take())
        };
        if wait_for_watcher_exit(&alive, Instant::now() + WATCHER_RESTART_WAIT) {
            if let Some(thread) = thread
                && thread.join().is_err()
            {
                tracing::warn!(root = %root.display(), "watcher thread panicked during config reload");
            }
            let mut watchers = self.state.watchers.lock();
            if watchers
                .get(root)
                .is_some_and(|entry| Arc::ptr_eq(&entry.alive, &alive))
            {
                watchers.remove(root);
            }
        } else {
            let mut watchers = self.state.watchers.lock();
            if let Some(entry) = watchers
                .get_mut(root)
                .filter(|entry| Arc::ptr_eq(&entry.alive, &alive))
            {
                entry.thread = thread;
            }
            tracing::warn!(root = %root.display(), "disabled watcher still draining; replacement remains blocked");
        }
    }

    pub(super) fn resolve_project_inner(&self, project: &str) -> Result<ProjectState> {
        // Cached roots still need config-mtime and index-existence checks after edits or resets.
        {
            let guard = self.state.resolved.lock();
            if let Some(state) = guard.get(project)
                && state.still_valid()
            {
                return Ok(state.clone());
            }
        }
        let path = PathBuf::from(project);
        if !path.is_absolute() {
            bail!(
                "`project` must be an absolute path, got `{}`. Pass the absolute project root.",
                project
            );
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("project path `{}` does not exist: {}", project, e))?;
        {
            let guard = self.state.projects.lock();
            if let Some(state) = guard.get(&canonical)
                && state.still_valid()
            {
                let state = state.clone();
                drop(guard);
                self.state
                    .resolved
                    .lock()
                    .insert(project.to_string(), state.clone());
                return Ok(state);
            }
        }
        // Match CLI root discovery: the nearest onboarded ancestor owns nested paths.
        let mut enclosing = canonical.clone();
        loop {
            if enclosing.join(".codesage").join("index.db").exists() {
                break;
            }
            if !enclosing.pop() {
                bail!(
                    "project `{}` is not onboarded (no .codesage/index.db). \
                    Run `/codesage-onboard {}` to initialize.",
                    canonical.display(),
                    canonical.display()
                );
            }
        }
        let canonical = enclosing;
        let codesage_dir = canonical.join(".codesage");
        let db_path = codesage_dir.join("index.db");
        let config_path = codesage_dir.join("config.toml");
        // Stat before reading so a racing replacement leaves an old stamp and forces a reload.
        let config_mtime = config_toml_mtime(&config_path);
        let embedding_config = load_embedding_config(&config_path);
        let state = ProjectState {
            db_path: db_path.clone(),
            embedding_config: embedding_config.config,
            embedding_config_error: embedding_config.semantic_error,
            watch: embedding_config.watch,
            exclude_patterns: embedding_config.exclude_patterns,
            config_mtime,
        };
        // Keep structural calls available, but never cache errors that a config repair can fix.
        if state.embedding_config_error.is_some() {
            return Ok(state);
        }
        // Reloads replace stale state; only first registration writes drift telemetry.
        let newly_registered = {
            let mut guard = self.state.projects.lock();
            guard.insert(canonical.clone(), state.clone()).is_none()
        };
        if newly_registered && let Err(e) = write_drift_log_for_project(&canonical, &db_path) {
            tracing::debug!(error = %e, "drift log append failed");
        }
        self.state
            .resolved
            .lock()
            .insert(project.to_string(), state.clone());
        Ok(state)
    }

    /// Ensure a pooled-model watcher exists. Spawn errors are logged without failing the tool call.
    fn ensure_watcher(&self, root: &Path, state: &ProjectState) {
        let config_key = watcher_config_key(state);
        let shutdown = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));

        // Reserve under the registry lock to prevent concurrent first calls from double-spawning.
        match reserve_watcher_slot(
            &self.state.watchers,
            root,
            &config_key,
            &shutdown,
            &alive,
            WATCHER_RESTART_WAIT,
        ) {
            WatcherSlot::Running => return,
            WatcherSlot::StillStopping => {
                tracing::info!(
                    root = %root.display(),
                    "previous watcher still draining; not spawning a replacement on this call"
                );
                return;
            }
            WatcherSlot::Reserved => {}
        }
        // Early returns must release the reservation or a never-spawned watcher looks alive forever.
        let reservation = WatcherReservation {
            watchers: &self.state.watchers,
            root: root.to_path_buf(),
            token: shutdown.clone(),
            armed: true,
        };

        // Invalid config must not silently substitute default exclusions; retry after repair.
        let project_config = match crate::load_project_config(root) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    root = %root.display(),
                    "could not load project config; not spawning watcher"
                );
                return;
            }
        };
        let config_watch = project_config.index.as_ref().and_then(|i| i.watch);
        if !crate::statewatcher::watch_enabled(root, config_watch) {
            return;
        }

        let exclude_patterns = crate::get_exclude_patterns(&project_config);

        // Resolve the pooled model lazily so idle watchers do not force a load.
        let embedder: Option<crate::statewatcher::EmbedderProvider> =
            if state.embedding_config.model.is_empty() || state.embedding_config_error.is_some() {
                None
            } else {
                let server = self.clone();
                let cfg = state.embedding_config.clone();
                Some(Arc::new(move || server.get_or_load_embedder(&cfg)))
            };

        let watcher_config = crate::statewatcher::StateWatcherConfig {
            project_root: root.to_path_buf(),
            db_path: state.db_path.clone(),
            embed_config: state.embedding_config.clone(),
            exclude_patterns,
            debounce_ms: crate::statewatcher::resolve_debounce_ms(),
            idle_timeout: crate::statewatcher::resolve_idle_timeout(),
            mode: crate::statewatcher::WatcherMode::Daemon,
            embedder,
            shutdown: shutdown.clone(),
            backpressure: true,
        };

        let alive_clone = alive.clone();
        let root_disp = root.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("cs-watch".to_string())
            .spawn(move || {
                let _alive_guard = AliveGuard(alive_clone);
                if let Err(e) = crate::statewatcher::run_statewatcher(watcher_config) {
                    tracing::error!(error = %e, root = %root_disp.display(), "watcher exited with error");
                }
            });

        match spawned {
            Ok(join) => {
                {
                    let mut watchers = self.state.watchers.lock();
                    if let Some(entry) = watchers
                        .get_mut(root)
                        .filter(|entry| Arc::ptr_eq(&entry.shutdown, &shutdown))
                    {
                        entry.thread = Some(join);
                    }
                }
                reservation.disarm();
                tracing::info!(root = %root.display(), "live watcher started");
            }
            Err(e) => {
                tracing::warn!(error = %e, root = %root.display(), "failed to spawn watcher thread");
            }
        }
    }

    fn get_or_load_embedder(&self, config: &EmbeddingConfig) -> Result<Arc<Mutex<Embedder>>> {
        let key = resolved_embedder_pool_key(config)?;
        get_or_load_slot(&self.state.embedders, key, || {
            Embedder::new(config).with_context(|| {
                format!(
                    "loading embedding model '{}' on device '{}'",
                    config.model, config.device
                )
            })
        })
    }

    fn get_or_load_reranker(
        &self,
        reranker_model: &str,
        device: &str,
    ) -> Result<Arc<Mutex<Reranker>>> {
        let key = format!("{}|{}", reranker_model, device);
        get_or_load_slot(&self.state.rerankers, key, || {
            Reranker::new(reranker_model, device).with_context(|| {
                format!("loading reranker model '{reranker_model}' on device '{device}'")
            })
        })
    }

    fn semantic_embedding_config<'a>(
        &self,
        state: &'a ProjectState,
    ) -> Result<&'a EmbeddingConfig> {
        if let Some(error) = &state.embedding_config_error {
            bail!("{error}");
        }
        Ok(&state.embedding_config)
    }

    /// Require an attested chunk table matching the configured vector identity.
    fn open_db_for(&self, state: &ProjectState) -> Result<Database> {
        let config = self.semantic_embedding_config(state)?;
        let embedder_arc = self.get_or_load_embedder(config)?;
        let dim = model_lock(&embedder_arc)?.dim();
        let db = Database::open_for_model_existing(&state.db_path, &config.model, dim)?;
        let fingerprint = crate::commands::index::resolved_fingerprint(&db, config, dim)?;
        codesage_graph::require_current_semantic_table(&db, &fingerprint)?;
        Ok(db)
    }

    pub(super) fn open_structural_db_for(&self, state: &ProjectState) -> Result<Database> {
        Database::open_existing(&state.db_path)
    }

    fn open_context_db_for(&self, state: &ProjectState) -> Result<Database> {
        let config = self.semantic_embedding_config(state)?;
        Database::open_for_existing_model(&state.db_path, &config.model)
    }

    // Real-binary integration tests cannot use cfg(test) fakes. Honor seeded vectors only
    // in debug builds; retain dimension/freshness checks and mark override responses.
    fn test_query_embedding_override(&self) -> Result<Option<Vec<f32>>> {
        Self::parse_test_query_embedding_override(
            std::env::var(MCP_TEST_QUERY_EMBEDDING_ENV).ok().as_deref(),
            cfg!(debug_assertions),
        )
    }

    /// Parameterize the build guard to test release-path logic in a debug test binary.
    fn parse_test_query_embedding_override(
        raw: Option<&str>,
        honored: bool,
    ) -> Result<Option<Vec<f32>>> {
        if !honored {
            return Ok(None);
        }
        let Some(raw) = raw else {
            return Ok(None);
        };

        let mut embedding = Vec::new();
        for (i, part) in raw.split(',').enumerate() {
            let value = part.trim();
            if value.is_empty() {
                bail!(
                    "{MCP_TEST_QUERY_EMBEDDING_ENV} component {} is empty",
                    i + 1
                );
            }
            let parsed: f32 = value.parse().with_context(|| {
                format!(
                    "{MCP_TEST_QUERY_EMBEDDING_ENV} component {} must be an f32",
                    i + 1
                )
            })?;
            if !parsed.is_finite() {
                bail!(
                    "{MCP_TEST_QUERY_EMBEDDING_ENV} component {} must be finite",
                    i + 1
                );
            }
            embedding.push(parsed);
        }
        if embedding.is_empty() {
            bail!("{MCP_TEST_QUERY_EMBEDDING_ENV} must contain at least one f32");
        }
        if embedding.len() > MAX_TEST_QUERY_EMBEDDING_DIM {
            bail!(
                "{MCP_TEST_QUERY_EMBEDDING_ENV} has {} components, over the {MAX_TEST_QUERY_EMBEDDING_DIM} cap",
                embedding.len()
            );
        }
        Ok(Some(embedding))
    }

    /// Render-layer probe for `_meta.test_override`; malformed overrides fail on the query path.
    pub(super) fn test_override_active() -> bool {
        Self::parse_test_query_embedding_override(
            std::env::var(MCP_TEST_QUERY_EMBEDDING_ENV).ok().as_deref(),
            cfg!(debug_assertions),
        )
        .is_ok_and(|opt| opt.is_some())
    }

    /// Open an existing debug-fixture table without downloading a model. Require the
    /// recorded dimension; compare fingerprints only when both attestation and local
    /// artifacts exist. Never create a table for an override.
    fn open_test_override_db(
        &self,
        state: &ProjectState,
        config: &EmbeddingConfig,
        dim: usize,
    ) -> Result<Database> {
        let db = Database::open_for_existing_model(&state.db_path, &config.model)?;
        let recorded = db.recorded_semantic_dim()?.ok_or_else(|| {
            anyhow::anyhow!(
                "test query-embedding override is set but model {:?} has no recorded chunk table; run `codesage index`",
                config.model
            )
        })?;
        if recorded != dim {
            bail!(
                "test query-embedding override has dim {dim} but model {:?} recorded dim {recorded}",
                config.model
            );
        }
        // Unattested fixtures have nothing to compare; avoid expensive artifact resolution.
        // Attested tables resolve locally and still reject mismatches.
        if db.semantic_fingerprint()?.is_some() {
            let expected = codesage_graph::resolve_semantic_fingerprint(
                &db,
                config,
                recorded,
                codesage_graph::ArtifactLookup::CachedOnly,
            )?;
            Self::enforce_test_override_freshness(&db, expected.as_ref())?;
        } else {
            tracing::debug!(
                "test query-embedding override proceeds without a fingerprint check: chunk table records no fingerprint"
            );
        }
        Database::open_for_model_existing(&state.db_path, &config.model, recorded)
    }

    /// Debug fixtures may lack attestation or cached artifacts; enforce dimension alone
    /// in those cases. A comparable recorded mismatch is still an error.
    fn enforce_test_override_freshness(
        db: &Database,
        expected: Option<&codesage_graph::SemanticFingerprint>,
    ) -> Result<()> {
        let Some(expected) = expected else {
            tracing::debug!(
                "test query-embedding override proceeds without a fingerprint check: model artifacts not in the local cache"
            );
            return Ok(());
        };
        match codesage_graph::semantic_table_state(db, expected)? {
            codesage_graph::SemanticTableState::Current => Ok(()),
            codesage_graph::SemanticTableState::Unrecorded => {
                tracing::debug!(
                    "test query-embedding override proceeds without a fingerprint check: chunk table records no fingerprint"
                );
                Ok(())
            }
            codesage_graph::SemanticTableState::Mismatch { stored } => {
                bail!(
                    "test query-embedding override refused: semantic index was embedded under a different setup (stored {stored}, current {}); run `codesage index --full`",
                    expected.as_str()
                )
            }
        }
    }

    pub(super) fn with_project_db<F, R>(&self, project: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Database) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        let db = self.open_structural_db_for(&state)?;
        f(&db)
    }

    /// Also expose the canonical root for tools that persist project state.
    pub(super) fn with_project_root_db<F, R>(&self, project: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Path, &Database) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        let db = self.open_structural_db_for(&state)?;
        let root = state
            .db_path
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow::anyhow!("could not derive project root from db path"))?;
        f(root, &db)
    }

    pub(super) fn with_project_context_db<F, R>(&self, project: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Database) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        let db = self.open_context_db_for(&state)?;
        f(&db)
    }

    /// Embed with the configured resident model; empty texts probe its identity.
    /// Non-empty requests must supply the full fingerprint, since model name and
    /// dimension alone cannot detect pooling, device, or artifact changes.
    pub(super) fn embed_texts_for(
        &self,
        project: &str,
        model: &str,
        expected_fingerprint: Option<&str>,
        texts: &[String],
    ) -> Result<EmbedTextsResult> {
        let state = self.resolve_project(project)?;
        let config = self.semantic_embedding_config(&state)?;
        if config.model != model {
            bail!(
                "daemon serves model {:?} for this project, caller asked for {:?}; \
                 re-run after the config change settles or embed privately",
                config.model,
                model
            );
        }
        let embedder_arc = self.get_or_load_embedder(config)?;
        let mut embedder = model_lock(&embedder_arc)?;
        let dim = embedder.dim();
        let fingerprint = session_fingerprint(config, &embedder)?;
        check_expected_fingerprint(expected_fingerprint, fingerprint.as_str(), texts.is_empty())?;
        let embeddings = if texts.is_empty() {
            Vec::new()
        } else {
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            embedder.embed_batch(&refs)?
        };
        Ok(EmbedTextsResult {
            model: config.model.clone(),
            dim,
            fingerprint: fingerprint.as_str().to_string(),
            embeddings,
        })
    }

    pub(super) fn rerank_pairs_for(
        &self,
        params: &RerankPairsParams,
        cancelled: impl Fn() -> bool,
    ) -> Result<RerankPairsResult> {
        let documents: Vec<&str> = params.documents.iter().map(String::as_str).collect();
        crate::query_reranker::check_caps(&params.query, &documents)?;
        anyhow::ensure!(!cancelled(), "rerank_pairs request cancelled");
        let state = self.resolve_project(&params.project)?;
        let config = self.semantic_embedding_config(&state)?;
        anyhow::ensure!(
            config.reranker.as_deref() == Some(params.model.as_str())
                && config.device == params.device,
            "daemon reranker model/device differs from requested configuration; re-run after the config change settles"
        );
        let scores = if documents.is_empty() {
            Vec::new()
        } else {
            let reranker = self.get_or_load_reranker(&params.model, &params.device)?;
            let deadline = std::time::Instant::now() + Duration::from_secs(120);
            loop {
                anyhow::ensure!(!cancelled(), "rerank_pairs request cancelled");
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "rerank_pairs timed out waiting for the pooled reranker"
                );
                if let Some(mut guard) = reranker.try_lock_for(Duration::from_millis(25)) {
                    anyhow::ensure!(!cancelled(), "rerank_pairs request cancelled");
                    break guard.score_pairs(&params.query, &documents)?;
                }
            }
        };
        crate::query_reranker::check_scores(&scores, documents.len())?;
        Ok(RerankPairsResult {
            model: params.model.clone(),
            device: params.device.clone(),
            scores,
        })
    }

    pub(super) fn with_project_query<F, R>(&self, project: &str, query: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Database, &[f32], Option<codesage_graph::RerankFn<'_>>) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        self.maybe_start_watcher(&state, true);
        let config = self.semantic_embedding_config(&state)?;
        if let Some(query_embedding) = self.test_query_embedding_override()? {
            // The override skips model loads; table compatibility remains checked and the render layer marks it.
            let dim = query_embedding.len();
            let db = self.open_test_override_db(&state, config, dim)?;
            return f(&db, &query_embedding, None);
        }
        let db = self.open_db_for(&state)?;
        let embedder_arc = self.get_or_load_embedder(config)?;
        let reranker_arc = config
            .reranker
            .as_deref()
            .map(|m| self.get_or_load_reranker(m, &config.device))
            .transpose()?;

        let query_embedding = {
            let mut guard = model_lock(&embedder_arc)?;
            // Config compatibility is insufficient: verify the resident session's execution provider too.
            let produces = session_fingerprint(config, &guard)?;
            codesage_graph::require_current_semantic_table(&db, &produces)?;
            guard.embed_one(query)?
        };

        let rerank_fn: Option<codesage_graph::RerankFn<'_>> = reranker_arc.map(|rr| {
            // Hold the model lock only during inference, not SQL retrieval or post-processing.
            Box::new(move |q: &str, docs: &[&str]| model_lock(&rr)?.score_pairs(q, docs))
                as Box<dyn FnMut(&str, &[&str]) -> Result<Vec<f32>>>
        });

        f(&db, &query_embedding, rerank_fn)
    }
}

/// Config failures disable semantic tools but preserve structural queries.
/// CLI indexing still rejects the malformed config.
fn load_embedding_config(path: &Path) -> LoadedEmbeddingConfig {
    let content = match crate::fsguard::read_state_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return LoadedEmbeddingConfig {
                config: EmbeddingConfig::default(),
                semantic_error: None,
                watch: None,
                exclude_patterns: Vec::new(),
            };
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not read project config; falling back to embedding defaults",
            );
            return LoadedEmbeddingConfig {
                config: EmbeddingConfig::default(),
                semantic_error: Some(format!(
                    "could not read project config `{}`: {e}",
                    path.display()
                )),
                watch: None,
                exclude_patterns: Vec::new(),
            };
        }
    };
    #[derive(serde::Deserialize)]
    struct IndexSection {
        watch: Option<bool>,
        #[serde(default)]
        exclude_patterns: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    struct Config {
        embedding: Option<EmbeddingConfig>,
        index: Option<IndexSection>,
    }
    match toml::from_str::<Config>(&content) {
        Ok(parsed) => LoadedEmbeddingConfig {
            config: parsed.embedding.unwrap_or_default(),
            semantic_error: None,
            watch: parsed.index.as_ref().and_then(|i| i.watch),
            exclude_patterns: parsed.index.map(|i| i.exclude_patterns).unwrap_or_default(),
        },
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not parse project config; falling back to embedding defaults",
            );
            LoadedEmbeddingConfig {
                config: EmbeddingConfig::default(),
                semantic_error: Some(format!(
                    "could not parse project config `{}`: {e}",
                    path.display()
                )),
                watch: None,
                exclude_patterns: Vec::new(),
            }
        }
    }
}

/// Record drift on registration; the caller logs failures without failing tool calls.
fn write_drift_log_for_project(project_root: &Path, db_path: &Path) -> Result<()> {
    let db = Database::open_existing(db_path)?;
    let report = codesage_graph::drift::check_drift(project_root, &db);
    codesage_graph::drift::append_drift_log(project_root, ".codesage", &report)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded_entry(value: i32, age: Duration) -> ModelEntry<i32> {
        ModelEntry {
            slot: Arc::new(Mutex::new(Some(Arc::new(Mutex::new(value))))),
            last_used: Instant::now().checked_sub(age).expect("test clock"),
        }
    }

    #[test]
    fn evict_idle_drops_only_idle_and_unreferenced_models() {
        let timeout = Duration::from_secs(900);
        let map: ModelMap<i32> = Mutex::new(HashMap::new());

        map.lock()
            .insert("fresh".into(), loaded_entry(1, Duration::from_secs(0)));
        map.lock()
            .insert("idle".into(), loaded_entry(2, Duration::from_secs(1200)));
        let idle_busy = loaded_entry(3, Duration::from_secs(1200));
        let in_flight = idle_busy.slot.lock().as_ref().expect("loaded").clone();
        map.lock().insert("idle_busy".into(), idle_busy);

        let evicted = evict_idle_from_map(&map, timeout);

        assert_eq!(evicted, 1, "only the idle, unreferenced model is evicted");
        let guard = map.lock();
        assert!(guard.contains_key("fresh"), "recently used model retained");
        assert!(!guard.contains_key("idle"), "idle model removed from pool");
        assert!(
            guard.contains_key("idle_busy"),
            "in-flight model retained despite being idle"
        );
        drop(in_flight);
    }

    #[test]
    fn evict_idle_is_noop_when_nothing_is_stale() {
        let map: ModelMap<i32> = Mutex::new(HashMap::new());
        map.lock()
            .insert("a".into(), loaded_entry(1, Duration::from_secs(10)));
        assert_eq!(evict_idle_from_map(&map, Duration::from_secs(900)), 0);
        assert_eq!(map.lock().len(), 1);
    }

    fn write_tmp(name: &str, content: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("codesage-mcp-test-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn malformed_config_keeps_structural_defaults_and_reports_semantic_error() {
        let path = write_tmp("malformed", "embedding = { this is not valid toml ===");
        let loaded = load_embedding_config(&path);
        assert_eq!(loaded.config.model, EmbeddingConfig::default().model);
        assert!(
            loaded
                .semantic_error
                .as_deref()
                .is_some_and(|e| e.contains("could not parse project config")),
            "semantic paths should fail loudly on malformed config: {loaded:?}"
        );
    }

    #[test]
    fn missing_config_returns_defaults() {
        let path = std::env::temp_dir().join(format!(
            "codesage-mcp-test-missing-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let loaded = load_embedding_config(&path);
        assert_eq!(loaded.config.model, EmbeddingConfig::default().model);
        assert!(loaded.semantic_error.is_none());
    }

    #[test]
    fn well_formed_config_parses() {
        let path = write_tmp(
            "valid",
            "[embedding]\nmodel = \"sentence-transformers/all-MiniLM-L6-v2\"\ndevice = \"cpu\"\n",
        );
        let loaded = load_embedding_config(&path);
        assert_eq!(
            loaded.config.model,
            "sentence-transformers/all-MiniLM-L6-v2"
        );
        assert_eq!(loaded.config.device, "cpu");
        assert!(loaded.semantic_error.is_none());
    }

    #[test]
    fn config_without_embedding_section_returns_defaults() {
        let path = write_tmp("no-embedding", "[project]\nname = \"foo\"\n");
        let loaded = load_embedding_config(&path);
        assert_eq!(loaded.config.model, EmbeddingConfig::default().model);
        assert!(loaded.semantic_error.is_none());
    }

    #[test]
    fn server_instances_can_share_cache_state() {
        let state = Arc::new(CodeSageServerState::new());
        let first = CodeSageServer::with_state(state.clone());
        let second = CodeSageServer::with_state(state.clone());

        assert!(Arc::ptr_eq(&first.state, &second.state));
    }

    #[test]
    fn alive_guard_flips_flag_on_drop() {
        let alive = Arc::new(AtomicBool::new(true));
        {
            let _guard = AliveGuard(alive.clone());
            assert!(alive.load(Ordering::SeqCst), "guard alone must not flip");
        }
        assert!(
            !alive.load(Ordering::SeqCst),
            "drop must flip alive to false"
        );
    }

    #[test]
    fn alive_guard_flips_flag_when_thread_panics() {
        let alive = Arc::new(AtomicBool::new(true));
        let alive_thread = alive.clone();
        let joined = std::thread::spawn(move || {
            let _guard = AliveGuard(alive_thread);
            panic!("simulated watcher panic");
        })
        .join();
        assert!(joined.is_err(), "thread must have panicked");
        assert!(
            !alive.load(Ordering::SeqCst),
            "alive must flip to false even when the thread panics"
        );
    }

    #[test]
    fn embedder_pool_key_separates_pooling_strategies_and_model_bytes() {
        let mut config = EmbeddingConfig::default();
        let mean = embedder_pool_key(&config, "digest-a").unwrap();
        config.pooling = Some(codesage_embed::config::PoolingStrategy::Cls);
        let cls = embedder_pool_key(&config, "digest-a").unwrap();
        assert_ne!(
            mean, cls,
            "a pooling switch under one model name must not share a session"
        );
        config.pooling = Some(codesage_embed::config::PoolingStrategy::Mean);
        assert_eq!(embedder_pool_key(&config, "digest-a").unwrap(), mean);
        assert_ne!(
            embedder_pool_key(&config, "digest-b").unwrap(),
            mean,
            "a same-name model whose files changed must not share a session"
        );
    }

    #[test]
    fn watcher_key_changes_with_pooling_and_retires_the_running_watcher() {
        let mut config = EmbeddingConfig::default();
        let mean_key = watcher_key(&config, "digest-a");
        config.pooling = Some(codesage_embed::config::PoolingStrategy::Cls);
        let cls_key = watcher_key(&config, "digest-a");
        assert_ne!(
            mean_key, cls_key,
            "a pooling change must not keep the watcher embedding the old way"
        );
        assert_ne!(
            watcher_key(&config, "digest-b"),
            cls_key,
            "changed model bytes must not keep the watcher"
        );

        let watchers: Mutex<HashMap<PathBuf, WatcherEntry>> = Mutex::new(HashMap::new());
        let root = PathBuf::from("/proj");
        let (old_shutdown, _old_alive) =
            fake_watcher(&watchers, &root, &mean_key, Duration::from_millis(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let slot = reserve_watcher_slot(
            &watchers,
            &root,
            &cls_key,
            &shutdown,
            &alive,
            Duration::from_secs(5),
        );
        assert!(matches!(slot, WatcherSlot::Reserved), "{slot:?}");
        assert!(
            old_shutdown.load(Ordering::SeqCst),
            "the old-pooling watcher must have been signalled to stop"
        );
        let entry = watchers.lock();
        assert_eq!(entry.get(&root).unwrap().config_key, cls_key);
    }

    #[test]
    fn watcher_reservation_releases_on_drop_and_keeps_on_disarm() {
        let watchers: Mutex<HashMap<PathBuf, WatcherEntry>> = Mutex::new(HashMap::new());
        let root = PathBuf::from("/proj");
        let make_entry = || {
            let shutdown = Arc::new(AtomicBool::new(false));
            let entry = WatcherEntry {
                shutdown: shutdown.clone(),
                alive: Arc::new(AtomicBool::new(true)),
                config_key: "k".to_string(),
                thread: None,
                stopping_since: None,
            };
            (shutdown, entry)
        };

        let (token, entry) = make_entry();
        watchers.lock().insert(root.clone(), entry);
        drop(WatcherReservation {
            watchers: &watchers,
            root: root.clone(),
            token,
            armed: true,
        });
        assert!(watchers.lock().is_empty(), "armed drop must release");

        let (token, entry) = make_entry();
        watchers.lock().insert(root.clone(), entry);
        WatcherReservation {
            watchers: &watchers,
            root: root.clone(),
            token,
            armed: true,
        }
        .disarm();
        assert!(watchers.lock().contains_key(&root), "disarm must keep");

        let (_other_token, entry) = make_entry();
        watchers.lock().insert(root.clone(), entry);
        drop(WatcherReservation {
            watchers: &watchers,
            root: root.clone(),
            token: Arc::new(AtomicBool::new(false)),
            armed: true,
        });
        assert!(
            watchers.lock().contains_key(&root),
            "foreign entry must survive another reservation's drop"
        );
    }

    /// Simulate a watcher drain, using the production liveness guard.
    fn fake_watcher(
        watchers: &Mutex<HashMap<PathBuf, WatcherEntry>>,
        root: &Path,
        config_key: &str,
        drain: Duration,
    ) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        let shutdown = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let (shutdown_t, alive_t) = (shutdown.clone(), alive.clone());
        let thread = std::thread::spawn(move || {
            let _guard = AliveGuard(alive_t);
            while !shutdown_t.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
            std::thread::sleep(drain);
        });
        watchers.lock().insert(
            root.to_path_buf(),
            WatcherEntry {
                shutdown: shutdown.clone(),
                alive: alive.clone(),
                config_key: config_key.to_string(),
                thread: Some(thread),
                stopping_since: None,
            },
        );
        (shutdown, alive)
    }

    #[test]
    fn stop_then_start_race_yields_exactly_one_watcher() {
        let watchers: Arc<Mutex<HashMap<PathBuf, WatcherEntry>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let root = PathBuf::from("/proj");
        let (_old_shutdown, old_alive) =
            fake_watcher(&watchers, &root, "k", Duration::from_millis(300));

        let stopper = {
            let watchers = watchers.clone();
            std::thread::spawn(move || stop_all_watchers(&watchers, Duration::from_secs(10)))
        };
        // Let the stop signal land so the start observes a STOPPING entry.
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            old_alive.load(Ordering::SeqCst),
            "old watcher must still be draining"
        );

        let new_shutdown = Arc::new(AtomicBool::new(false));
        let new_alive = Arc::new(AtomicBool::new(true));
        let slot = reserve_watcher_slot(
            &watchers,
            &root,
            "k",
            &new_shutdown,
            &new_alive,
            Duration::from_secs(10),
        );

        assert_eq!(slot, WatcherSlot::Reserved);
        assert!(
            !old_alive.load(Ordering::SeqCst),
            "the slot may be handed over only after the old watcher has exited"
        );
        assert_eq!(stopper.join().unwrap(), 0, "the stop saw every watcher out");
        let guard = watchers.lock();
        assert_eq!(guard.len(), 1, "exactly one watcher slot");
        let entry = guard.get(&root).expect("the new reservation owns the slot");
        assert!(
            Arc::ptr_eq(&entry.alive, &new_alive),
            "the surviving entry is the new one, not the stopped watcher's"
        );
        assert!(entry.thread.is_none(), "the reservation has no thread yet");
    }

    #[test]
    fn a_watcher_wedged_in_shutdown_is_detached_past_the_bound() {
        let watchers: Mutex<HashMap<PathBuf, WatcherEntry>> = Mutex::new(HashMap::new());
        let root = PathBuf::from("/proj");
        watchers.lock().insert(
            root.clone(),
            WatcherEntry {
                shutdown: Arc::new(AtomicBool::new(true)),
                alive: Arc::new(AtomicBool::new(true)),
                config_key: "k".to_string(),
                thread: None,
                stopping_since: None,
            },
        );
        let shutdown = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let slot = reserve_watcher_slot(&watchers, &root, "k", &shutdown, &alive, Duration::ZERO);
        assert_eq!(slot, WatcherSlot::StillStopping);
        assert!(
            watchers.lock().get(&root).unwrap().stopping_since.is_some(),
            "first sighting of the wedge must start the clock"
        );
        watchers.lock().get_mut(&root).unwrap().stopping_since =
            Some(Instant::now() - WATCHER_STUCK_FORCE_DETACH - Duration::from_secs(1));
        let fresh_shutdown = Arc::new(AtomicBool::new(false));
        let fresh_alive = Arc::new(AtomicBool::new(true));
        let slot = reserve_watcher_slot(
            &watchers,
            &root,
            "k",
            &fresh_shutdown,
            &fresh_alive,
            Duration::ZERO,
        );
        assert_eq!(slot, WatcherSlot::Reserved);
        let guard = watchers.lock();
        let entry = guard
            .get(&root)
            .expect("the fresh reservation owns the slot");
        assert!(
            Arc::ptr_eq(&entry.alive, &fresh_alive),
            "the wedged entry must be gone, replaced by the fresh reservation"
        );
    }

    #[test]
    fn a_session_producing_another_identity_than_the_table_is_refused_as_stale() {
        let config = EmbeddingConfig {
            device: "cuda".to_string(),
            ..EmbeddingConfig::default()
        };
        let table = codesage_graph::SemanticFingerprint::with_artifact_digest(&config, 4, "d");
        let db = Database::open_in_memory().unwrap();
        db.record_semantic_fingerprint(table.as_str()).unwrap();
        codesage_graph::require_current_semantic_table(&db, &table).unwrap();

        let produces = table.with_execution_provider("cpu");
        let err = codesage_graph::require_current_semantic_table(&db, &produces).unwrap_err();
        assert!(
            err.downcast_ref::<codesage_graph::StaleSemanticTable>()
                .is_some(),
            "typed as the stale-table refusal: {err}"
        );
        let err = err.to_string();
        assert!(err.contains("codesage index --full"), "{err}");
        assert!(
            err.contains("device=cuda") && err.contains("device=cpu"),
            "{err}"
        );
    }

    #[test]
    fn embed_texts_fingerprint_gate_refuses_a_mismatch_and_an_unbound_batch() {
        let ok = check_expected_fingerprint(Some("v3;a"), "v3;a", false);
        assert!(ok.is_ok());
        assert!(
            check_expected_fingerprint(None, "v3;a", true).is_ok(),
            "a probe may omit it"
        );

        let err = check_expected_fingerprint(Some("v3;a"), "v3;b", false)
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with(super::super::EMBED_TEXTS_FINGERPRINT_MISMATCH),
            "{err}"
        );
        assert!(err.contains("v3;a") && err.contains("v3;b"), "{err}");

        let err = check_expected_fingerprint(Some("v3;a"), "v3;b", true)
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with(super::super::EMBED_TEXTS_FINGERPRINT_MISMATCH),
            "{err}"
        );

        let err = check_expected_fingerprint(None, "v3;a", false)
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with(super::super::EMBED_TEXTS_FINGERPRINT_MISMATCH),
            "{err}"
        );
        assert!(err.contains("no fingerprint"), "{err}");
    }

    #[test]
    fn last_client_stop_aborts_when_a_client_connected_before_it_landed() {
        let watchers: Mutex<HashMap<PathBuf, WatcherEntry>> = Mutex::new(HashMap::new());
        let root = PathBuf::from("/proj");
        let (shutdown, alive) = fake_watcher(&watchers, &root, "k", Duration::ZERO);
        let active = std::sync::atomic::AtomicUsize::new(0);

        active.fetch_add(1, Ordering::SeqCst);
        let stopped = stop_all_watchers_if(&watchers, Duration::from_secs(5), || {
            active.load(Ordering::SeqCst) == 0
        });

        assert_eq!(stopped, None, "a stop with a client connected must abort");
        assert!(!shutdown.load(Ordering::SeqCst), "no shutdown signalled");
        assert!(alive.load(Ordering::SeqCst), "the watcher keeps running");
        assert!(watchers.lock().contains_key(&root));

        active.fetch_sub(1, Ordering::SeqCst);
        let stopped = stop_all_watchers_if(&watchers, Duration::from_secs(5), || {
            active.load(Ordering::SeqCst) == 0
        });
        assert_eq!(stopped, Some(0));
        assert!(shutdown.load(Ordering::SeqCst));
        assert!(!alive.load(Ordering::SeqCst));
        assert!(watchers.lock().is_empty());
    }

    #[test]
    fn start_during_a_stop_that_outlives_the_wait_spawns_nothing() {
        let watchers: Mutex<HashMap<PathBuf, WatcherEntry>> = Mutex::new(HashMap::new());
        let root = PathBuf::from("/proj");
        let (old_shutdown, old_alive) =
            fake_watcher(&watchers, &root, "k", Duration::from_millis(400));
        old_shutdown.store(true, Ordering::SeqCst);

        let slot = reserve_watcher_slot(
            &watchers,
            &root,
            "k",
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(true)),
            Duration::from_millis(50),
        );

        assert_eq!(slot, WatcherSlot::StillStopping);
        {
            let guard = watchers.lock();
            assert_eq!(guard.len(), 1);
            let entry = guard.get(&root).unwrap();
            assert!(
                Arc::ptr_eq(&entry.alive, &old_alive),
                "the stopping entry keeps its slot"
            );
            assert!(
                entry.thread.is_some(),
                "the join handle is put back for the next waiter"
            );
        }

        assert!(wait_for_watcher_exit(
            &old_alive,
            Instant::now() + Duration::from_secs(5)
        ));
        let new_alive = Arc::new(AtomicBool::new(true));
        let slot = reserve_watcher_slot(
            &watchers,
            &root,
            "k",
            &Arc::new(AtomicBool::new(false)),
            &new_alive,
            Duration::from_millis(50),
        );
        assert_eq!(slot, WatcherSlot::Reserved);
        let guard = watchers.lock();
        assert_eq!(guard.len(), 1);
        assert!(Arc::ptr_eq(&guard.get(&root).unwrap().alive, &new_alive));
    }

    #[test]
    fn stop_all_keeps_a_slot_whose_thread_outlives_the_wait() {
        let watchers: Mutex<HashMap<PathBuf, WatcherEntry>> = Mutex::new(HashMap::new());
        let root = PathBuf::from("/proj");
        let (_shutdown, alive) = fake_watcher(&watchers, &root, "k", Duration::from_millis(300));

        let still = stop_all_watchers(&watchers, Duration::from_millis(30));

        assert_eq!(still, 1);
        assert!(alive.load(Ordering::SeqCst));
        assert!(
            watchers.lock().contains_key(&root),
            "a draining watcher must keep its slot so nothing spawns beside it"
        );
        assert!(wait_for_watcher_exit(
            &alive,
            Instant::now() + Duration::from_secs(5)
        ));
        assert_eq!(stop_all_watchers(&watchers, Duration::from_millis(30)), 0);
        assert!(watchers.lock().is_empty());
    }

    #[test]
    fn reserve_returns_running_for_a_live_watcher_with_the_same_config() {
        let watchers: Mutex<HashMap<PathBuf, WatcherEntry>> = Mutex::new(HashMap::new());
        let root = PathBuf::from("/proj");
        let (shutdown, alive) = fake_watcher(&watchers, &root, "k", Duration::ZERO);

        let slot = reserve_watcher_slot(
            &watchers,
            &root,
            "k",
            &Arc::new(AtomicBool::new(false)),
            &Arc::new(AtomicBool::new(true)),
            Duration::from_millis(50),
        );
        assert_eq!(slot, WatcherSlot::Running);
        assert!(!shutdown.load(Ordering::SeqCst));

        let new_alive = Arc::new(AtomicBool::new(true));
        let slot = reserve_watcher_slot(
            &watchers,
            &root,
            "k2",
            &Arc::new(AtomicBool::new(false)),
            &new_alive,
            Duration::from_secs(5),
        );
        assert_eq!(slot, WatcherSlot::Reserved);
        assert!(shutdown.load(Ordering::SeqCst));
        assert!(!alive.load(Ordering::SeqCst));
        assert!(Arc::ptr_eq(
            &watchers.lock().get(&root).unwrap().alive,
            &new_alive
        ));
    }

    /// Onboarded project scaffold: `.codesage/index.db` exists, optional
    /// config.toml content. Returns (tempdir guard, canonical root).
    fn onboarded_project(config: Option<&str>) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let codesage_dir = root.join(".codesage");
        std::fs::create_dir_all(&codesage_dir).unwrap();
        Database::open(&codesage_dir.join("index.db")).unwrap();
        if let Some(content) = config {
            std::fs::write(codesage_dir.join("config.toml"), content).unwrap();
        }
        (dir, root)
    }

    #[test]
    fn resolve_project_rejects_relative_path() {
        let server = CodeSageServer::new();
        let err = server
            .resolve_project_inner("relative/path")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("absolute"),
            "relative-path error must tell the agent to pass an absolute path: {err}"
        );
    }

    #[test]
    fn resolve_project_reports_nonexistent_path() {
        let server = CodeSageServer::new();
        let err = server
            .resolve_project_inner("/codesage-test-no-such-dir-e23c52d8")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not exist"),
            "nonexistent-path error must say the path does not exist: {err}"
        );
    }

    #[test]
    fn resolve_project_reports_not_onboarded() {
        let dir = tempfile::tempdir().unwrap();
        let server = CodeSageServer::new();
        let err = server
            .resolve_project_inner(dir.path().to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not onboarded") && err.contains("onboard"),
            "not-onboarded error must carry the onboarding remediation hint: {err}"
        );
    }

    #[test]
    fn resolve_project_walks_up_to_the_enclosing_onboarded_root() {
        let (_dir, root) = onboarded_project(None);
        let sub = root.join("src").join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        let server = CodeSageServer::new();
        let state = server.resolve_project_inner(sub.to_str().unwrap()).unwrap();
        assert_eq!(state.db_path, root.join(".codesage").join("index.db"));
    }

    #[test]
    fn resolve_project_prefers_the_nearest_enclosing_codesage() {
        let (_outer_dir, outer) = onboarded_project(None);
        let inner = outer.join("inner");
        std::fs::create_dir_all(inner.join(".codesage")).unwrap();
        Database::open(&inner.join(".codesage").join("index.db")).unwrap();
        let deep = inner.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        let server = CodeSageServer::new();
        let state = server
            .resolve_project_inner(deep.to_str().unwrap())
            .unwrap();
        assert_eq!(state.db_path, inner.join(".codesage").join("index.db"));
    }

    #[test]
    fn resolve_project_errors_after_index_db_deleted_without_recreating_it() {
        let (_dir, root) = onboarded_project(None);
        let project = root.to_str().unwrap();
        let server = CodeSageServer::new();

        server.resolve_project_inner(project).unwrap();
        server.resolve_project_inner(project).unwrap();

        let db_path = root.join(".codesage/index.db");
        std::fs::remove_file(&db_path).unwrap();

        let err = server
            .resolve_project_inner(project)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not onboarded"),
            "deleted index.db must surface the not-onboarded error, got: {err}"
        );
        assert!(
            !db_path.exists(),
            "resolution must not recreate the index file"
        );
    }

    #[test]
    fn with_project_db_errors_after_index_db_deleted() {
        let (_dir, root) = onboarded_project(None);
        let project = root.to_str().unwrap();
        let server = CodeSageServer::new();
        server.resolve_project_inner(project).unwrap();

        let db_path = root.join(".codesage/index.db");
        std::fs::remove_file(&db_path).unwrap();

        let err = server
            .with_project_db(project, |db| db.file_count())
            .unwrap_err();
        assert!(
            err.to_string().contains("not onboarded"),
            "tool call on a deleted index must error, not return empty results: {err:#}"
        );
        assert!(
            !db_path.exists(),
            "tool call must not recreate an empty index"
        );
    }

    #[test]
    fn watcher_starts_on_semantic_queries_or_explicit_opt_in_only() {
        assert!(!watcher_start_wanted(None, false));
        assert!(watcher_start_wanted(None, true));
        assert!(watcher_start_wanted(Some(true), false));
        assert!(watcher_start_wanted(Some(true), true));
        assert!(!watcher_start_wanted(Some(false), false));
        assert!(!watcher_start_wanted(Some(false), true));
    }

    #[test]
    fn resolve_project_does_not_spawn_a_watcher_for_a_structural_call() {
        let (_dir, root) = onboarded_project(None);
        let server = CodeSageServer::new();
        let state = server.resolve_project(root.to_str().unwrap()).unwrap();
        assert_eq!(state.watch, None);
        assert!(
            server.state.watchers.lock().is_empty(),
            "a structural resolution must not start a live watcher"
        );
    }

    #[test]
    fn index_watch_setting_is_read_into_project_state() {
        let (_dir, root) = onboarded_project(Some("[index]\nwatch = false\n"));
        let server = CodeSageServer::new();
        let state = server
            .resolve_project_inner(root.to_str().unwrap())
            .unwrap();
        assert_eq!(state.watch, Some(false));
        assert!(state.embedding_config_error.is_none());

        let (_dir2, root2) = onboarded_project(Some(
            "[index]\nwatch = true\n[embedding]\nmodel = \"m\"\ndevice = \"cpu\"\n",
        ));
        let state2 = server
            .resolve_project_inner(root2.to_str().unwrap())
            .unwrap();
        assert_eq!(state2.watch, Some(true));
        assert_eq!(state2.embedding_config.model, "m");
    }

    #[test]
    fn stale_enabled_query_cannot_restore_a_disabled_watcher() {
        let (_dir, root) = onboarded_project(Some("[index]\nwatch = true\n"));
        let server = CodeSageServer::new();
        let stale = server
            .resolve_project_inner(root.to_str().unwrap())
            .unwrap();
        let (shutdown, alive) = fake_watcher(
            &server.state.watchers,
            &root,
            &watcher_config_key(&stale),
            Duration::ZERO,
        );
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(
            root.join(".codesage/config.toml"),
            "[index]\nwatch = false\n",
        )
        .unwrap();
        server.maybe_start_watcher(&stale, true);
        assert!(shutdown.load(Ordering::SeqCst));
        assert!(!alive.load(Ordering::SeqCst));
        assert!(server.state.watchers.lock().is_empty());
    }

    #[test]
    fn ensure_watcher_skips_spawn_when_config_is_malformed() {
        let (_dir, root) = onboarded_project(Some("embedding = { this is not valid toml ==="));
        let server = CodeSageServer::new();
        let state = server
            .resolve_project_inner(root.to_str().unwrap())
            .unwrap();
        assert!(state.embedding_config_error.is_some());

        server.ensure_watcher(&root, &state);

        assert!(
            server.state.watchers.lock().is_empty(),
            "unloadable config must skip the watcher spawn"
        );
    }

    #[test]
    fn resolve_project_retries_after_config_error_is_fixed() {
        let (_dir, root) = onboarded_project(Some("embedding = { this is not valid toml ==="));
        let project = root.to_str().unwrap();
        let server = CodeSageServer::new();

        let broken = server.resolve_project_inner(project).unwrap();
        assert!(
            broken.embedding_config_error.is_some(),
            "malformed config must surface a semantic error"
        );

        std::fs::write(
            root.join(".codesage/config.toml"),
            "[embedding]\nmodel = \"fixed/model\"\ndevice = \"cpu\"\n",
        )
        .unwrap();

        let fixed = server.resolve_project_inner(project).unwrap();
        assert!(
            fixed.embedding_config_error.is_none(),
            "fixing the file must clear the error without a daemon restart"
        );
        assert_eq!(fixed.embedding_config.model, "fixed/model");
    }

    #[test]
    fn resolve_project_reloads_config_on_mtime_change() {
        let (_dir, root) =
            onboarded_project(Some("[embedding]\nmodel = \"model/a\"\ndevice = \"cpu\"\n"));
        let project = root.to_str().unwrap();
        let config_path = root.join(".codesage/config.toml");
        let server = CodeSageServer::new();

        let first = server.resolve_project_inner(project).unwrap();
        assert_eq!(first.embedding_config.model, "model/a");
        let cached = server.resolve_project_inner(project).unwrap();
        assert_eq!(cached.embedding_config.model, "model/a");

        std::fs::write(
            &config_path,
            "[embedding]\nmodel = \"model/b\"\ndevice = \"cpu\"\n",
        )
        .unwrap();
        // Force a distinct mtime: same-second writes can collide on coarse
        // filesystem timestamp granularity.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&config_path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(2))
            .unwrap();

        let reloaded = server.resolve_project_inner(project).unwrap();
        assert_eq!(
            reloaded.embedding_config.model, "model/b",
            "mtime bump must reload the embedding config"
        );
    }

    #[test]
    fn resolve_project_picks_up_config_created_after_first_resolution() {
        let (_dir, root) = onboarded_project(None);
        let project = root.to_str().unwrap();
        let server = CodeSageServer::new();

        let defaults = server.resolve_project_inner(project).unwrap();
        assert_eq!(
            defaults.embedding_config.model,
            EmbeddingConfig::default().model
        );

        std::fs::write(
            root.join(".codesage/config.toml"),
            "[embedding]\nmodel = \"late/model\"\ndevice = \"cpu\"\n",
        )
        .unwrap();

        let reloaded = server.resolve_project_inner(project).unwrap();
        assert_eq!(reloaded.embedding_config.model, "late/model");
    }

    #[test]
    fn slot_loader_runs_exactly_once_under_concurrent_first_callers() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let map: Arc<ModelMap<u32>> = Arc::new(Mutex::new(HashMap::new()));
        let load_count = Arc::new(AtomicUsize::new(0));

        let start = Arc::new(std::sync::Barrier::new(16));

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let map = map.clone();
                let load_count = load_count.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    get_or_load_slot(&map, "shared-key".to_string(), || {
                        load_count.fetch_add(1, Ordering::SeqCst);
                        // Widen the concurrent cold-load window.
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        Ok::<u32, anyhow::Error>(42 + i as u32)
                    })
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert_eq!(
            load_count.load(Ordering::SeqCst),
            1,
            "loader must run exactly once across all concurrent callers"
        );

        let first = results[0].as_ref().unwrap().clone();
        for r in &results {
            let arc = r.as_ref().unwrap();
            assert!(Arc::ptr_eq(&first, arc), "all callers should share one Arc");
        }
    }

    #[test]
    fn slot_loader_runs_per_key_in_parallel() {
        let map: Arc<ModelMap<u32>> = Arc::new(Mutex::new(HashMap::new()));
        let start = Arc::new(std::sync::Barrier::new(2));

        let t0 = std::time::Instant::now();
        let h1 = {
            let map = map.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                get_or_load_slot(&map, "k1".to_string(), || {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                    Ok::<u32, anyhow::Error>(1)
                })
            })
        };
        let h2 = {
            let map = map.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                get_or_load_slot(&map, "k2".to_string(), || {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                    Ok::<u32, anyhow::Error>(2)
                })
            })
        };
        h1.join().unwrap().unwrap();
        h2.join().unwrap().unwrap();
        let elapsed = t0.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(140),
            "distinct keys must load in parallel; elapsed {:?} suggests serialization",
            elapsed
        );
    }

    #[test]
    fn slot_loader_failure_leaves_slot_retryable() {
        let map: Arc<ModelMap<u32>> = Arc::new(Mutex::new(HashMap::new()));

        let first: Result<_, anyhow::Error> =
            get_or_load_slot(&map, "k".to_string(), || anyhow::bail!("boom"));
        assert!(first.is_err());

        let second = get_or_load_slot(&map, "k".to_string(), || Ok::<u32, anyhow::Error>(99));
        let arc = second.unwrap();
        assert_eq!(*arc.lock(), 99);
    }

    #[test]
    fn structural_project_db_does_not_load_embedding_model() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let codesage_dir = root.join(".codesage");
        std::fs::create_dir_all(&codesage_dir).unwrap();
        std::fs::write(
            codesage_dir.join("config.toml"),
            "[embedding]\nmodel = \"codesage-test/does-not-exist\"\ndevice = \"cpu\"\n",
        )
        .unwrap();
        let db_path = codesage_dir.join("index.db");
        Database::open(&db_path).unwrap();

        let server = CodeSageServer::new();
        let count = server
            .with_project_db(root.to_str().unwrap(), |db| db.file_count())
            .unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn rerank_pairs_validates_caps_cancellation_and_configuration_before_loading() {
        let (_dir, root) = onboarded_project(Some(
            "[embedding]\nmodel = \"codesage-test/missing\"\nreranker = \"codesage-test/reranker\"\ndevice = \"cpu\"\n[index]\nwatch = false\n",
        ));
        let server = CodeSageServer::new();
        let mut params = RerankPairsParams {
            project: root.to_str().unwrap().into(),
            model: "codesage-test/reranker".into(),
            device: "cpu".into(),
            query: "query".into(),
            documents: Vec::new(),
        };
        assert!(
            server
                .rerank_pairs_for(&params, || false)
                .unwrap()
                .scores
                .is_empty()
        );
        params.documents.push("doc".into());
        assert!(
            server
                .rerank_pairs_for(&params, || true)
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        params.device = "gpu".into();
        assert!(
            server
                .rerank_pairs_for(&params, || false)
                .unwrap_err()
                .to_string()
                .contains("configuration")
        );
        params.device = "cpu".into();
        params.model = "other".into();
        assert!(
            server
                .rerank_pairs_for(&params, || false)
                .unwrap_err()
                .to_string()
                .contains("configuration")
        );
        params.documents = vec!["x".repeat(crate::query_reranker::MAX_RERANK_TEXT_BYTES + 1)];
        assert!(
            server
                .rerank_pairs_for(&params, || false)
                .unwrap_err()
                .to_string()
                .contains("over cap")
        );
        assert!(server.state.rerankers.lock().is_empty());
        assert!(server.state.embedders.lock().is_empty());
    }

    #[test]
    fn structural_project_db_still_opens_with_malformed_config() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let codesage_dir = root.join(".codesage");
        std::fs::create_dir_all(&codesage_dir).unwrap();
        std::fs::write(
            codesage_dir.join("config.toml"),
            "embedding = { this is not valid toml ===",
        )
        .unwrap();
        let db_path = codesage_dir.join("index.db");
        Database::open(&db_path).unwrap();

        let server = CodeSageServer::new();
        let count = server
            .with_project_db(root.to_str().unwrap(), |db| db.file_count())
            .unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn semantic_project_query_rejects_malformed_config_without_creating_default_table() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let codesage_dir = root.join(".codesage");
        std::fs::create_dir_all(&codesage_dir).unwrap();
        std::fs::write(
            codesage_dir.join("config.toml"),
            "embedding = { this is not valid toml ===",
        )
        .unwrap();
        let db_path = codesage_dir.join("index.db");
        Database::open(&db_path).unwrap();

        let server = CodeSageServer::new();
        let err = server
            .with_project_query(root.to_str().unwrap(), "query", |_, _, _| Ok(()))
            .unwrap_err();

        assert!(
            err.to_string().contains("could not parse project config"),
            "unexpected error: {err:#}"
        );
        let db = Database::open(&db_path).unwrap();
        assert!(
            db.list_vec_tables().unwrap().is_empty(),
            "semantic query should fail before creating a default vec table"
        );
    }

    #[test]
    fn test_override_ignored_when_not_honored_release_path() {
        // This debug test verifies release-path logic, not the release build flag.
        assert!(
            CodeSageServer::parse_test_query_embedding_override(Some("0.1,0.2,0.3,0.4"), false)
                .unwrap()
                .is_none(),
            "release path must ignore the override even when set"
        );
        assert!(
            CodeSageServer::parse_test_query_embedding_override(None, false)
                .unwrap()
                .is_none()
        );
        assert!(
            !CodeSageServer::test_override_active() || cfg!(debug_assertions),
            "override must never report active in a release build"
        );
    }

    #[test]
    fn test_override_parses_comma_floats_when_honored() {
        assert!(
            CodeSageServer::parse_test_query_embedding_override(None, true)
                .unwrap()
                .is_none(),
            "unset variable means no override"
        );
        let parsed =
            CodeSageServer::parse_test_query_embedding_override(Some("0.1, 0.2,0.3, 0.4"), true)
                .unwrap()
                .expect("honored debug build must parse the override");
        assert_eq!(parsed, vec![0.1_f32, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn test_override_rejects_malformed_components() {
        for raw in [
            "",
            "   ",
            "0.1,,0.3",
            "0.1,abc,0.3",
            "0.1,nan,0.3",
            "0.1,inf,0.3",
            "0.1,-inf,0.3",
        ] {
            assert!(
                CodeSageServer::parse_test_query_embedding_override(Some(raw), true).is_err(),
                "override {raw:?} must be refused"
            );
        }
    }

    #[test]
    fn test_override_caps_attacker_controlled_dim() {
        let at_cap = (0..MAX_TEST_QUERY_EMBEDDING_DIM)
            .map(|_| "0.5")
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            CodeSageServer::parse_test_query_embedding_override(Some(&at_cap), true)
                .unwrap()
                .is_some(),
            "exactly the cap must still parse"
        );
        let over_cap = format!("{at_cap},0.5");
        let err = CodeSageServer::parse_test_query_embedding_override(Some(&over_cap), true)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("over the"),
            "over-cap override must name the cap: {err}"
        );
    }

    /// Onboard a project on the uncached fake model and resolve its state.
    fn override_project_with_recorded_dim() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        CodeSageServer,
        ProjectState,
    ) {
        const MODEL: &str = "codesage-test/does-not-exist";
        let (dir, root, server, state) = onboarded_project_and_state(
            "[embedding]\nmodel = \"codesage-test/does-not-exist\"\ndevice = \"cpu\"\n",
        );
        assert_eq!(state.embedding_config.model, MODEL);
        // The nonexistent model keeps CachedOnly resolution independent of local caches.
        let db_path = root.join(".codesage").join("index.db");
        Database::open_for_model(&db_path, MODEL, 4).unwrap();
        (dir, root, server, state)
    }

    /// Variant of `onboarded_project` that also resolves the project state.
    fn onboarded_project_and_state(
        config: &str,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        CodeSageServer,
        ProjectState,
    ) {
        let (dir, root) = onboarded_project(Some(config));
        let server = CodeSageServer::new();
        let project = root.to_str().unwrap().to_string();
        let state = server.resolve_project_inner(&project).unwrap();
        (dir, root, server, state)
    }

    #[test]
    fn test_override_open_accepts_recorded_dim() {
        let (_dir, _root, server, state) = override_project_with_recorded_dim();
        let db = server
            .open_test_override_db(&state, &state.embedding_config, 4)
            .expect("matching dim must open");
        assert_eq!(db.recorded_semantic_dim().unwrap(), Some(4));
    }

    #[test]
    fn test_override_open_refuses_dim_mismatch_without_creating_tables() {
        let (_dir, root, server, state) = override_project_with_recorded_dim();
        let err = server
            .open_test_override_db(&state, &state.embedding_config, 8)
            .err()
            .expect("dim mismatch must be refused")
            .to_string();
        assert!(
            err.contains("recorded dim 4"),
            "dim mismatch must name the recorded dim: {err}"
        );
        let db = Database::open(&root.join(".codesage").join("index.db")).unwrap();
        assert!(
            !db.list_vec_tables()
                .unwrap()
                .iter()
                .any(|t| t.contains("_8")),
            "a refused dim must not mint a dim-8 chunk table"
        );
    }

    #[test]
    fn test_override_open_refuses_model_without_recorded_table() {
        let (_dir, _root, server, state) = onboarded_project_and_state(
            "[embedding]\nmodel = \"codesage-test/does-not-exist\"\ndevice = \"cpu\"\n",
        );
        let err = server
            .open_test_override_db(&state, &state.embedding_config, 4)
            .err()
            .expect("unindexed model must be refused")
            .to_string();
        assert!(
            err.contains("no recorded chunk table"),
            "unindexed model must fail instead of minting tables: {err}"
        );
    }

    #[test]
    fn test_override_freshness_refuses_mismatch_accepts_current() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let config = EmbeddingConfig::default();
        let db = Database::open_for_model(&db_path, &config.model, 4).unwrap();

        CodeSageServer::enforce_test_override_freshness(&db, None)
            .expect("absent artifacts must not fail a debug-only override");

        let expected = codesage_graph::SemanticFingerprint::with_artifact_digest(
            &config,
            4,
            "digest-for-override-test",
        );
        CodeSageServer::enforce_test_override_freshness(&db, Some(&expected))
            .expect("unattested table must not fail the debug-only override");

        db.record_semantic_fingerprint("stale-setup-fingerprint")
            .unwrap();
        let err = CodeSageServer::enforce_test_override_freshness(&db, Some(&expected))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("different setup") && err.contains("codesage index --full"),
            "stale table must name the repair: {err}"
        );

        db.record_semantic_fingerprint(expected.as_str()).unwrap();
        CodeSageServer::enforce_test_override_freshness(&db, Some(&expected))
            .expect("current table must pass the override gate");
    }
}
