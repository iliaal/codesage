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
use super::params::EmbedTextsResult;

const MCP_TEST_QUERY_EMBEDDING_ENV: &str = "CODESAGE_MCP_TEST_QUERY_EMBEDDING";

#[derive(Debug, Clone)]
pub(super) struct ProjectState {
    pub(super) db_path: PathBuf,
    pub(super) embedding_config: EmbeddingConfig,
    embedding_config_error: Option<String>,
    /// `[index] watch` from the project config: `Some(true)` asks for a live
    /// watcher on the first call of any kind, `Some(false)` refuses one, and
    /// `None` (the default) starts it on the first semantic query only.
    watch: Option<bool>,
    /// mtime of `.codesage/config.toml` when this state was loaded; `None`
    /// means the file was absent (defaults in effect). Checked on every
    /// resolution so a model switch or a config fix takes effect without a
    /// daemon restart — a stale cached config would keep embedding
    /// into the old `chunks_{model}_{dim}` table and silently fork the
    /// semantic index.
    config_mtime: Option<std::time::SystemTime>,
}

impl ProjectState {
    fn config_path(&self) -> Option<PathBuf> {
        // db_path = <root>/.codesage/index.db; config.toml sits beside it.
        self.db_path.parent().map(|dir| dir.join("config.toml"))
    }

    /// True when the on-disk config no longer matches what this state was
    /// built from (edited, created, or deleted since load).
    fn config_changed(&self) -> bool {
        match self.config_path() {
            Some(path) => config_toml_mtime(&path) != self.config_mtime,
            None => true,
        }
    }

    /// True when this cached state can still serve calls. Beyond the config
    /// check, the index DB itself must still exist: `/codesage-reset` deletes
    /// it under a live daemon, and serving the stale state would let a
    /// downstream open recreate an empty index that answers every query with
    /// zero results.
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
}

/// Whether a tool call should make sure a live watcher runs. A watcher
/// re-embeds every saved file, so it is not worth starting for a session
/// that only reads structure: it starts on the first semantic query, or on
/// any call when the project config opts in with `[index] watch = true`.
/// `Some(false)` is honored again in `watch_enabled`; it is refused here too
/// so the spawn path is never entered for it.
fn watcher_start_wanted(config_watch: Option<bool>, semantic_query: bool) -> bool {
    match config_watch {
        Some(true) => true,
        Some(false) => false,
        None => semantic_query,
    }
}

/// One model "slot" per key — the outer mutex serializes the cold load
/// for that key, the inner option holds the loaded model once init
/// succeeds. Concurrent callers for the same key wait on the per-key
/// mutex; callers for different keys run in parallel because they
/// hold different slots. Previously `get_or_load_*` checked the map,
/// dropped the lock, called `new()`, then raced to insert — two
/// concurrent cold misses for the same model loaded two ORT sessions
/// and the loser was thrown away.
type ModelSlot<T> = Arc<Mutex<Option<Arc<Mutex<T>>>>>;

/// A pooled model: its load slot plus the last time it was requested.
/// `last_used` is protected by the enclosing map mutex (stamped on every
/// `get_or_load_slot` and read by the idle reaper), so it needs no lock
/// of its own.
struct ModelEntry<T> {
    slot: ModelSlot<T>,
    last_used: Instant,
}

type ModelMap<T> = Mutex<HashMap<String, ModelEntry<T>>>;

/// Find or create the slot for `key` and, if not yet populated, run
/// `load()` under the slot lock. Returns the shared `Arc<Mutex<T>>`
/// either way. The map lock is held only long enough to find-or-insert
/// the slot (and stamp `last_used`); the loader runs while only the
/// per-key slot lock is held, so concurrent calls for *different* keys
/// never wait on each other.
///
/// The race this closes was `check map → drop → load → insert`: two threads
/// hitting the same cold key both ran `load()` and the loser's value
/// got dropped. This helper closes that window — for a single key, the
/// first thread to reach the slot lock runs `load()` exactly once; the
/// rest read the populated `Some(arc)` and return immediately.
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
    let mut slot_guard = slot.lock();
    if let Some(arc) = slot_guard.as_ref() {
        return Ok(arc.clone());
    }
    let value = load()?;
    let arc = Arc::new(Mutex::new(value));
    *slot_guard = Some(arc.clone());
    Ok(arc)
}

/// Drop pooled models that have sat unused longer than `timeout` and are
/// not currently in flight, returning how many were evicted. The loaded
/// model (and its ORT `Session`) is dropped *outside* the map lock so a
/// slow CUDA teardown never stalls other lookups.
///
/// "Not in flight" is `Arc::strong_count(inner) == 1` — only the pool
/// holds the model, no tool call has a clone out. We `try_lock` each
/// slot rather than block: a slot held by an in-flight cold load is
/// skipped this round instead of pinning the map lock for seconds.
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
    projects: Mutex<HashMap<PathBuf, ProjectState>>,
    /// Fast-path cache keyed by the raw `project` arg string. Agents pass the
    /// same literal absolute path on every call, so this lets `resolve_project`
    /// skip `canonicalize()`'s per-component lstat on the hot path. `projects`
    /// (keyed by canonical path) stays the source of truth and dedupes distinct
    /// spellings of the same root.
    resolved: Mutex<HashMap<String, ProjectState>>,
    embedders: ModelMap<Embedder>,
    rerankers: ModelMap<Reranker>,
    /// Live filesystem watchers, one per project, keyed by canonical root.
    /// Spawned lazily on first tool call for a project (see
    /// [`CodeSageServer::ensure_watcher`]) and reaped on daemon shutdown.
    watchers: Mutex<HashMap<PathBuf, WatcherEntry>>,
}

/// Handle to a per-project watcher thread. `alive` flips to false when the
/// thread exits (idle timeout, disabled marker, error), so `ensure_watcher`
/// can tell a dead entry from a running one and respawn. `config_key`
/// records the embedding config the watcher was spawned with so a config
/// change can retire it (see [`watcher_config_key`]). `thread` is the
/// spawned thread's join handle, taken by whichever stop path waits it out;
/// `None` while the spawn is still in flight or once the handle is taken.
///
/// An entry with `shutdown` set and `alive` still true is STOPPING: its
/// thread is force-draining. The slot stays occupied until the thread has
/// exited, so a start request for the same root waits instead of spawning a
/// second watcher beside it (see [`reserve_watcher_slot`]).
struct WatcherEntry {
    shutdown: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    config_key: String,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// How long a start request waits for a stopping watcher on the same root
/// before giving up on spawning for this call. Bounded because it runs
/// inside a tool call; the next call retries.
const WATCHER_RESTART_WAIT: Duration = Duration::from_secs(5);

/// How long the last-client stop waits for each watcher to finish its drain
/// before leaving its slot in place as still-stopping.
pub(crate) const WATCHER_STOP_WAIT: Duration = Duration::from_secs(60);

/// Poll interval for the alive-flag waits below. The watcher loop notices
/// `shutdown` within its own 500 ms poll, so finer polling buys nothing.
const WATCHER_EXIT_POLL: Duration = Duration::from_millis(20);

/// Block until `alive` reads false or `deadline` passes. True when the
/// thread has exited.
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

/// Signal every live watcher, then wait (up to `wait` in total) for each to
/// exit, joining its thread and freeing its slot only once it has. A slot
/// whose thread outlives the wait stays in the map as stopping, with its
/// handle put back for whoever finishes the wait later; nothing may spawn
/// into that slot meanwhile. Returns how many watchers were still stopping
/// at the deadline.
fn stop_all_watchers(watchers: &Mutex<HashMap<PathBuf, WatcherEntry>>, wait: Duration) -> usize {
    stop_all_watchers_if(watchers, wait, || true).expect("an unconditional stop never aborts")
}

/// [`stop_all_watchers`], gated: `still_wanted` is evaluated under the
/// registry lock immediately before the first shutdown signal, and a `false`
/// aborts the whole stop with `None` and no watcher signalled. The
/// last-client stop passes the client count here: a client that connected
/// between the disconnect that scheduled the stop and this point would
/// otherwise see its watcher running and then stopped under it, because a
/// start request only waits on the slot once the signal has landed.
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

/// Decision of [`reserve_watcher_slot`].
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

/// Claim the watcher slot for `root` with the given `shutdown`/`alive`
/// tokens. A live watcher with a matching `config_key` keeps the slot; one
/// with a stale key is signalled to stop. A stopping watcher — signalled by
/// a config change or by the last client's disconnect — is waited out (up to
/// `wait`) before the slot is handed over, so a shim reconnecting while the
/// previous watcher force-drains never gets a second watcher on the same
/// root: overlapping watchers reindex the same saves twice and race on the
/// status file.
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
                // Spawned with an outdated embedding config (model switch
                // in config.toml). Retire it; the wait below sees it out so
                // the respawn never overlaps the drain.
                entry.shutdown.store(true, Ordering::SeqCst);
            }
            let old_alive = entry.alive.clone();
            let thread = entry.thread.take();
            drop(guard);
            if !wait_for_watcher_exit(&old_alive, deadline) {
                let mut guard = watchers.lock();
                if let Some(entry) = guard
                    .get_mut(root)
                    .filter(|entry| Arc::ptr_eq(&entry.alive, &old_alive))
                {
                    entry.thread = thread;
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
            },
        );
        return WatcherSlot::Reserved;
    }
}

/// Flips a watcher's `alive` flag to false when its thread exits by ANY
/// path — including a panic unwinding out of `run_statewatcher`.
/// Without the guard a panicked watcher left `alive` true forever and
/// `ensure_watcher`'s hot check never respawned it. Mirrors statewatcher's
/// `StatusGuard` pattern.
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Removes a reserved watcher-map entry unless [`disarm`](Self::disarm)ed.
/// `ensure_watcher` inserts the entry BEFORE dropping the map lock so a
/// concurrent caller can't double-spawn; this guard makes sure the
/// reservation can't leak when a path between the insert and a successful
/// thread spawn bails out. Identity is checked via `Arc::ptr_eq` on the
/// `shutdown` handle so the guard never removes an entry it doesn't own.
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

/// Key identifying the embedding setup a watcher runs with. A live watcher
/// whose key no longer matches the project's current config is retired
/// (shutdown signalled) so the next resolution respawns it with the fresh
/// config instead of embedding into the old model's chunk table forever.
///
/// Every input of the semantic fingerprint is in the key — model, device,
/// batch size, pooling, and the identity of the model files on disk —
/// because a watcher that survives any of them changing keeps producing
/// vectors the new fingerprint disowns. The file identity is path, size, and
/// mtime of the cached artifacts (the key the content digest is cached by),
/// never a read of their bytes: this runs on every tool call and must not
/// block on hashing a model or on a download. An uncached model keys as
/// `uncached`; the first load changes the key and restarts the watcher once.
fn watcher_config_key(state: &ProjectState) -> String {
    if state.embedding_config.model.is_empty() || state.embedding_config_error.is_some() {
        return "structural-only".to_string();
    }
    let identity = cached_artifact_identity(&state.embedding_config.model);
    watcher_key(&state.embedding_config, &identity)
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

/// Pool key for a resident [`Embedder`]: everything `Embedder::new` bakes
/// into the session's output. Pooling is part of it — a project that
/// switches `[embedding].pooling` under the same model name must get a fresh
/// session, not the one still pooling the other way — and so is the digest
/// of the model files, so a same-name model whose bytes changed on disk gets
/// a session over the new bytes rather than the one loaded from the old.
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

/// The semantic fingerprint of the vectors a resident `embedder` loaded for
/// `config` produces. The artifact digest is already cached from the pool
/// key the load resolved, so this reads no model file.
fn session_fingerprint(
    config: &EmbeddingConfig,
    embedder: &Embedder,
) -> Result<codesage_graph::SemanticFingerprint> {
    Ok(
        codesage_graph::SemanticFingerprint::compute(config, embedder.dim())?
            .with_execution_provider(embedder.execution_provider()),
    )
}

/// The `embed_texts` fingerprint gate: a caller's `expected` fingerprint
/// must equal the one this session `produces`; a non-empty request must
/// carry one. A probe (empty texts) may omit it. Refusals carry
/// [`super::EMBED_TEXTS_FINGERPRINT_MISMATCH`] so the client aborts rather
/// than embedding privately.
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
            projects: Mutex::new(HashMap::new()),
            resolved: Mutex::new(HashMap::new()),
            embedders: Mutex::new(HashMap::new()),
            rerankers: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
        }
    }

    /// Drop pooled embedders + rerankers idle longer than `timeout`,
    /// returning how many were evicted. Frees the ORT `Session` (and thus
    /// its GPU VRAM); the host-side `malloc_trim` is the caller's job so it
    /// runs once per sweep rather than per model. Models with an in-flight
    /// call (a held `Arc` clone) are left alone. Called periodically by the
    /// daemon's model-eviction task.
    pub(crate) fn evict_idle_models(&self, timeout: Duration) -> usize {
        evict_idle_from_map(&self.embedders, timeout)
            + evict_idle_from_map(&self.rerankers, timeout)
    }

    /// Signal every live watcher to drain and exit, then wait up to `wait`
    /// for them to do so, freeing each registry slot only once its thread
    /// has been joined. Called when the last client disconnects and on
    /// daemon exit. A watcher still draining at the deadline keeps its slot
    /// as stopping, so a start request for that root waits rather than
    /// spawning beside it. Blocks; call from a blocking context.
    pub(crate) fn shutdown_all_watchers(&self, wait: Duration) -> usize {
        stop_all_watchers(&self.watchers, wait)
    }

    /// The last-client variant of [`Self::shutdown_all_watchers`]: stops
    /// only if `active_clients` still reads zero under the registry lock,
    /// and returns `None` without signalling any watcher when a client has
    /// connected since the disconnect that scheduled this stop.
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

    /// Spawn (or respawn after an idle exit) the project's live watcher when
    /// [`watcher_start_wanted`] says this call warrants one. Cheap when a
    /// watcher is already running. Root is `<...>/.codesage/index.db` → two
    /// parents.
    fn maybe_start_watcher(&self, state: &ProjectState, semantic_query: bool) {
        if !watcher_start_wanted(state.watch, semantic_query) {
            return;
        }
        if let Some(root) = state.db_path.parent().and_then(|p| p.parent()) {
            self.ensure_watcher(root, state);
        }
    }

    fn resolve_project_inner(&self, project: &str) -> Result<ProjectState> {
        // Fast path: same raw arg string seen before — skip canonicalize().
        // Two stats guard the cache: config.toml (an edited / created /
        // deleted config falls through to a reload, so model switches and
        // config fixes take effect without a daemon restart) and
        // index.db (a deleted index falls through to the cold path's
        // not-onboarded gate instead of being silently recreated).
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
        let codesage_dir = canonical.join(".codesage");
        let db_path = codesage_dir.join("index.db");
        if !db_path.exists() {
            bail!(
                "project `{}` is not onboarded (no .codesage/index.db). \
                Run `/codesage-onboard {}` to initialize.",
                canonical.display(),
                canonical.display()
            );
        }
        let config_path = codesage_dir.join("config.toml");
        // Stamp the mtime BEFORE reading: if the file is replaced between the
        // stat and the read we hold an older stamp and reload on the next
        // call, never serving a config newer than the stamp claims.
        let config_mtime = config_toml_mtime(&config_path);
        let embedding_config = load_embedding_config(&config_path);
        let state = ProjectState {
            db_path: db_path.clone(),
            embedding_config: embedding_config.config,
            embedding_config_error: embedding_config.semantic_error,
            watch: embedding_config.watch,
            config_mtime,
        };
        // A load error is never cached: structural tools still work off this
        // state for the current call (semantic paths surface the error), but
        // the next resolution re-reads the config so fixing the file doesn't
        // need a daemon restart.
        if state.embedding_config_error.is_some() {
            return Ok(state);
        }
        // `insert` (not insert-if-absent): a config reload must replace the
        // stale entry. First registration still gets the drift log below.
        let newly_registered = {
            let mut guard = self.state.projects.lock();
            guard.insert(canonical.clone(), state.clone()).is_none()
        };
        // Drift telemetry: on first resolution of a project in this MCP
        // session, append one JSON line to `.codesage/drift.log`. Non-fatal —
        // telemetry errors stay in tracing so a drift write never blocks a
        // tool call.
        if newly_registered && let Err(e) = write_drift_log_for_project(&canonical, &db_path) {
            tracing::debug!(error = %e, "drift log append failed");
        }
        self.state
            .resolved
            .lock()
            .insert(project.to_string(), state.clone());
        Ok(state)
    }

    /// Ensure a live filesystem watcher is running for `root`, spawning one if
    /// absent and the project hasn't opted out. The watcher reuses the daemon's
    /// pooled embedder (no extra model load) and self-exits after idle; this
    /// just guarantees one exists. Errors are logged, never propagated.
    fn ensure_watcher(&self, root: &Path, state: &ProjectState) {
        let config_key = watcher_config_key(state);
        let shutdown = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));

        // Hot path: a live watcher already exists. Just a lock + atomic load,
        // no config I/O — this runs on every tool call. The (re)spawn path
        // reserves the map entry under the SAME lock before spawning, so two
        // concurrent first calls can't both spawn a watcher and orphan one
        // entry's shutdown/alive handles; a stopping watcher is waited out
        // first so the respawn never overlaps its drain.
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
        // Every early return below must release the reservation, else the
        // hot check would treat a never-spawned watcher as alive forever.
        let reservation = WatcherReservation {
            watchers: &self.state.watchers,
            root: root.to_path_buf(),
            token: shutdown.clone(),
            armed: true,
        };

        // (Re)spawn path: load config and honor opt-out / disabled marker.
        // A config that fails to load must not fall back to defaults: the
        // watcher would silently index with default exclude patterns while
        // `codesage index` hard-errors on the same file. Skip the spawn; the
        // next resolution retries once the file is fixed.
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

        // Hand the watcher the daemon's pooled embedder, resolved lazily so an
        // idle watcher never forces a model load. `None` = structural-only.
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

    /// Open the chunk table a semantic query reads, refusing one whose
    /// fingerprint is absent or differs from the configured setup: its
    /// vectors are another setup's output and the error names the repair.
    fn open_db_for(&self, state: &ProjectState) -> Result<Database> {
        let config = self.semantic_embedding_config(state)?;
        let embedder_arc = self.get_or_load_embedder(config)?;
        let dim = embedder_arc.lock().dim();
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

    // Daemon integration tests spawn the real binary, where ordinary
    // `#[cfg(test)]` fakes are unavailable. This test-named env var lets that
    // binary exercise MCP search with a seeded vector table and no model
    // download; release/user paths ignore it unless explicitly set.
    fn test_query_embedding_override(&self) -> Result<Option<Vec<f32>>> {
        let Ok(raw) = std::env::var(MCP_TEST_QUERY_EMBEDDING_ENV) else {
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
        Ok(Some(embedding))
    }

    /// Resolve project, open its DB, run `f` with the DB. Error handling funnel:
    /// each handler's body lives under this so the tool dispatch stays one-liner.
    pub(super) fn with_project_db<F, R>(&self, project: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Database) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        let db = self.open_structural_db_for(&state)?;
        f(&db)
    }

    /// Variant of `with_project_db` that also passes the canonical project
    /// root path. Used by tools like `session_start` that need to write
    /// alongside `.codesage/index.db` (e.g. `.codesage/sessions/<id>.json`).
    pub(super) fn with_project_root_db<F, R>(&self, project: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Path, &Database) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        let db = self.open_structural_db_for(&state)?;
        // db_path = <project_root>/.codesage/index.db; pop twice to recover root.
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

    /// Resolve a project, embed `query`, and call `f` with the resulting
    /// query embedding + a reranker callback that lazily locks the shared
    /// reranker only when the search pipeline actually invokes it.
    ///
    /// The lock scopes are deliberately tight: the embedder mutex is held
    /// only for the `embed_one` call, the reranker mutex is held only for
    /// the (single) `score_pairs` call inside `search`. SQLite retrieval
    /// and result post-processing run lock-free, so concurrent agents on
    /// the same project can interleave their SQL work while one is in the
    /// (slow) ORT call. Pre-daemon each shim had a per-process embedder
    /// pool so calls were already parallel; this preserves that property
    /// under the shared-daemon model.
    /// Embed `texts` with the project's resident embedder for the hidden
    /// `embed_texts` tool. `model` must be the project's configured model:
    /// the caller is about to write these vectors into that model's chunk
    /// table, and a daemon whose config moved on would silently fork the
    /// index. An empty `texts` probes model, dimension, and fingerprint.
    ///
    /// `expected_fingerprint` is the full semantic fingerprint the caller
    /// will attest the vectors under. Model and dimension alone let a
    /// same-model pooling, device, or model-file change on this side slip
    /// into a table recorded under the caller's original identity, so a
    /// non-empty request without one, or with one that differs from the
    /// fingerprint this session produces, is refused under
    /// [`super::EMBED_TEXTS_FINGERPRINT_MISMATCH`].
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
        let mut embedder = embedder_arc.lock();
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

    pub(super) fn with_project_query<F, R>(&self, project: &str, query: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Database, &[f32], Option<codesage_graph::RerankFn<'_>>) -> Result<R>,
    {
        let state = self.resolve_project(project)?;
        // A semantic query is what a live watcher exists to keep fresh.
        self.maybe_start_watcher(&state, true);
        let config = self.semantic_embedding_config(&state)?;
        if let Some(query_embedding) = self.test_query_embedding_override()? {
            let db = Database::open_for_model_existing(
                &state.db_path,
                &config.model,
                query_embedding.len(),
            )?;
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
            let mut guard = embedder_arc.lock();
            // The table is current for the configured setup; the query
            // vector must come from a session producing that same identity,
            // or its neighbours are another setup's. Refused as the stale
            // table is, naming `codesage index --full`.
            let produces = session_fingerprint(config, &guard)?;
            codesage_graph::require_current_semantic_table(&db, &produces)?;
            guard.embed_one(query)?
        };

        let rerank_fn: Option<codesage_graph::RerankFn<'_>> = reranker_arc.map(|rr| {
            // Closure captures the Arc; per-call .lock() means the reranker
            // mutex is held only across the score_pairs call inside search,
            // not for the surrounding SQL retrieval and post-processing.
            Box::new(move |q: &str, docs: &[&str]| rr.lock().score_pairs(q, docs))
                as Box<dyn FnMut(&str, &[&str]) -> Result<Vec<f32>>>
        });

        f(&db, &query_embedding, rerank_fn)
    }
}

/// Load the per-project embedding config for the MCP server.
///
/// MCP serves multiple projects through one process; a malformed
/// `.codesage/config.toml` in one project must not poison structural tools
/// (`assess_risk`, `find_coupling`, `find_symbol`, ...). Read or parse failures
/// keep defaults available for those structural paths, but semantic tools fail
/// before loading a model or creating a default vec table because the indexed
/// model is no longer trustworthy.
///
/// The CLI path (`load_project_config` in `main.rs`) deliberately keeps the
/// loud-fail behavior: a user running `codesage index` interactively wants to
/// know their config is broken.
fn load_embedding_config(path: &Path) -> LoadedEmbeddingConfig {
    let content = match crate::fsguard::read_state_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return LoadedEmbeddingConfig {
                config: EmbeddingConfig::default(),
                semantic_error: None,
                watch: None,
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
            };
        }
    };
    #[derive(serde::Deserialize)]
    struct IndexSection {
        watch: Option<bool>,
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
            watch: parsed.index.and_then(|i| i.watch),
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
            }
        }
    }
}

/// Opens the project DB read-only-enough to compute a drift snapshot and
/// append one JSON line to `.codesage/drift.log`. Returns quickly — the DB
/// handle drops at the end of this call. Failures propagate so the caller
/// can log them; drift telemetry never kills a tool call.
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

        // fresh: used just now -> kept.
        map.lock()
            .insert("fresh".into(), loaded_entry(1, Duration::from_secs(0)));
        // idle + unreferenced -> evicted.
        map.lock()
            .insert("idle".into(), loaded_entry(2, Duration::from_secs(1200)));
        // idle but a tool call holds a clone (strong_count > 1) -> kept.
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
        // ensure path doesn't exist
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
        // A valid TOML that just doesn't have an `[embedding]` table — the
        // file is fine, the embedding section is absent, defaults apply.
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
        // Regression: the watcher thread previously stored `false` AFTER
        // run_statewatcher returned, so a panic unwound past the store and
        // `alive` stayed true forever — ensure_watcher never respawned.
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

        // The running watcher was spawned under mean pooling; the next
        // resolution under CLS must signal it and take the slot.
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
        // The reservation placed under the first lock must be
        // removed on any bail-out path (watch disabled, spawn failure) and
        // kept once a thread owns it.
        let watchers: Mutex<HashMap<PathBuf, WatcherEntry>> = Mutex::new(HashMap::new());
        let root = PathBuf::from("/proj");
        let make_entry = || {
            let shutdown = Arc::new(AtomicBool::new(false));
            let entry = WatcherEntry {
                shutdown: shutdown.clone(),
                alive: Arc::new(AtomicBool::new(true)),
                config_key: "k".to_string(),
                thread: None,
            };
            (shutdown, entry)
        };

        // Armed drop removes the reserved entry.
        let (token, entry) = make_entry();
        watchers.lock().insert(root.clone(), entry);
        drop(WatcherReservation {
            watchers: &watchers,
            root: root.clone(),
            token,
            armed: true,
        });
        assert!(watchers.lock().is_empty(), "armed drop must release");

        // Disarmed drop keeps the entry.
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

        // A reservation never removes an entry it doesn't own.
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

    /// A stand-in watcher thread: spins until `shutdown`, then keeps running
    /// for `drain` (the force-drain a real watcher performs), then exits and
    /// flips `alive` through the same guard the real spawn uses.
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
            },
        );
        (shutdown, alive)
    }

    #[test]
    fn stop_then_start_race_yields_exactly_one_watcher() {
        // Last client disconnects (stop) while a reconnecting shim resolves
        // the same project (start). The old watcher force-drains for a
        // while; the start must wait for it, not spawn beside it.
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
    fn a_session_producing_another_identity_than_the_table_is_refused_as_stale() {
        // The table was attested under the configured CUDA setup; a resident
        // session producing the CPU identity must not answer its queries.
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
        // The last client disconnects and schedules the stop; a new client
        // connects BEFORE the stop signal lands. The stop must observe that
        // client and leave the watcher untouched, or the reconnecting
        // client sees its watcher running and then stopped under it.
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

        // That client leaves: the next stop goes through.
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

        // Once the drain finishes, the next start request takes the slot.
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
        // A second stop finds it exited and frees the slot.
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

        // A stale config key retires it and waits it out before reserving.
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
    fn resolve_project_errors_after_index_db_deleted_without_recreating_it() {
        // The warm-daemon fast path must revalidate the DB file, not just
        // config.toml: /codesage-reset deletes .codesage/index.db under a
        // live daemon, and a cached ProjectState that survives the deletion
        // would let the storage layer recreate an empty index that serves
        // zero-result answers forever.
        let (_dir, root) = onboarded_project(None);
        let project = root.to_str().unwrap();
        let server = CodeSageServer::new();

        server.resolve_project_inner(project).unwrap();
        // Second resolution serves from the fast-path cache.
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
        // Default: structural calls never start a watcher, semantic ones do.
        assert!(!watcher_start_wanted(None, false));
        assert!(watcher_start_wanted(None, true));
        // Explicit opt-in: any call.
        assert!(watcher_start_wanted(Some(true), false));
        assert!(watcher_start_wanted(Some(true), true));
        // Explicit opt-out: never, not even for a semantic query.
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
    fn ensure_watcher_skips_spawn_when_config_is_malformed() {
        // A config that `codesage index` hard-errors on must not produce a
        // watcher running with default exclude patterns.
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
        // An error state must never be cached — previously, a transient
        // config read/parse failure at first resolution pinned the semantic
        // error for the daemon's whole life.
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
        // A model switch in config.toml must be picked up by the
        // cached ProjectState — otherwise the daemon keeps embedding into
        // the old `chunks_{model}_{dim}` table until restarted.
        let (_dir, root) =
            onboarded_project(Some("[embedding]\nmodel = \"model/a\"\ndevice = \"cpu\"\n"));
        let project = root.to_str().unwrap();
        let config_path = root.join(".codesage/config.toml");
        let server = CodeSageServer::new();

        let first = server.resolve_project_inner(project).unwrap();
        assert_eq!(first.embedding_config.model, "model/a");
        // Second call is a cache hit (exercises the fast path's mtime check).
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
        // Missing config is its own cached state (mtime None); creating the
        // file later must invalidate it.
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
        // Regression: the old path was check map → drop lock → call new()
        // → race to insert. Two cold misses for the same key both ran
        // the loader and the loser's value was thrown away. With the
        // per-key slot lock, only the first thread runs `load`; the rest
        // observe Some(arc) and return it.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let map: Arc<ModelMap<u32>> = Arc::new(Mutex::new(HashMap::new()));
        let load_count = Arc::new(AtomicUsize::new(0));

        // Gate the loader on a shared start signal so all threads are
        // poised to race, then release them simultaneously.
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
                        // Brief sleep widens the race window so a buggy
                        // implementation actually loses.
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

        // All callers must observe the SAME Arc (pointer equality), not
        // separate constructions.
        let first = results[0].as_ref().unwrap().clone();
        for r in &results {
            let arc = r.as_ref().unwrap();
            assert!(Arc::ptr_eq(&first, arc), "all callers should share one Arc");
        }
    }

    #[test]
    fn slot_loader_runs_per_key_in_parallel() {
        // Distinct keys must hold distinct slots — different cold loads
        // should not serialize on each other. Verify by measuring that
        // two loaders that each block for ~80ms complete in well under
        // 160ms (they run concurrently, not back-to-back).
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
        // A failed load must not poison the slot: the next caller should
        // be able to retry. Pre-fix code had the same behavior (the
        // failed value never went into the map); the helper preserves
        // that property by writing to *slot_guard only on Ok.
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
}
