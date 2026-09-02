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
/// change can retire it (see [`watcher_config_key`]).
struct WatcherEntry {
    shutdown: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    config_key: String,
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
fn watcher_config_key(state: &ProjectState) -> String {
    if state.embedding_config.model.is_empty() || state.embedding_config_error.is_some() {
        "structural-only".to_string()
    } else {
        format!(
            "{}|{}",
            state.embedding_config.model, state.embedding_config.device
        )
    }
}

/// Pool key for a resident [`Embedder`]: everything `Embedder::new` bakes
/// into the session's output. Pooling is part of it — a project that
/// switches `[embedding].pooling` under the same model name must get a fresh
/// session, not the one still pooling the other way.
fn embedder_pool_key(config: &EmbeddingConfig) -> Result<String> {
    let batch_size = config.effective_batch_size()?;
    Ok(format!(
        "{}|{}|{}|{:?}",
        config.model,
        config.device,
        batch_size.get(),
        config.pooling_strategy()
    ))
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

    /// Signal every live watcher to drain and exit. Called from the daemon's
    /// shutdown path. Threads notice within one poll interval; we don't join
    /// (the process exit reaps any straggler), so this never blocks shutdown.
    pub(crate) fn shutdown_all_watchers(&self) {
        let mut guard = self.watchers.lock();
        for (root, entry) in guard.iter() {
            entry.shutdown.store(true, Ordering::SeqCst);
            tracing::info!(root = %root.display(), "signalling watcher shutdown");
        }
        guard.clear();
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
        // entry's shutdown/alive handles.
        {
            let mut watchers = self.state.watchers.lock();
            if let Some(entry) = watchers.get(root)
                && entry.alive.load(Ordering::SeqCst)
            {
                if entry.config_key == config_key {
                    return;
                }
                // The live watcher was spawned with an outdated embedding
                // config (model switch in config.toml). Signal it to drain;
                // a later resolution respawns with the fresh config once it
                // exits, so the watcher stops embedding into the old model's
                // chunk table.
                entry.shutdown.store(true, Ordering::SeqCst);
                return;
            }
            watchers.insert(
                root.to_path_buf(),
                WatcherEntry {
                    shutdown: shutdown.clone(),
                    alive: alive.clone(),
                    config_key,
                },
            );
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
            Ok(_join) => {
                reservation.disarm();
                tracing::info!(root = %root.display(), "live watcher started");
            }
            Err(e) => {
                tracing::warn!(error = %e, root = %root.display(), "failed to spawn watcher thread");
            }
        }
    }

    fn get_or_load_embedder(&self, config: &EmbeddingConfig) -> Result<Arc<Mutex<Embedder>>> {
        let key = embedder_pool_key(config)?;
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

    fn open_db_for(&self, state: &ProjectState) -> Result<Database> {
        let config = self.semantic_embedding_config(state)?;
        let embedder_arc = self.get_or_load_embedder(config)?;
        let embedder = embedder_arc.lock();
        Database::open_for_model_existing(&state.db_path, &config.model, embedder.dim())
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
    /// index. An empty `texts` probes model and dimension only.
    pub(super) fn embed_texts_for(
        &self,
        project: &str,
        model: &str,
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
        let embeddings = if texts.is_empty() {
            Vec::new()
        } else {
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            embedder.embed_batch(&refs)?
        };
        Ok(EmbedTextsResult {
            model: config.model.clone(),
            dim: embedder.dim(),
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
    fn embedder_pool_key_separates_pooling_strategies() {
        let mut config = EmbeddingConfig::default();
        let mean = embedder_pool_key(&config).unwrap();
        config.pooling = Some(codesage_embed::config::PoolingStrategy::Cls);
        let cls = embedder_pool_key(&config).unwrap();
        assert_ne!(
            mean, cls,
            "a pooling switch under one model name must not share a session"
        );
        config.pooling = Some(codesage_embed::config::PoolingStrategy::Mean);
        assert_eq!(embedder_pool_key(&config).unwrap(), mean);
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
