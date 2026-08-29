mod brief_gate;
mod commands;
mod coverage;
mod daemon;
mod doctor;
#[cfg(target_os = "android")]
mod flock_override;
mod installer;
mod lockfile;
mod mcp;
mod statewatcher;
mod util;

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use codesage_embed::config::{EmbeddingConfig, ProjectConfig};
use codesage_embed::model::Embedder;
use codesage_parser::discover::DEFAULT_EXCLUDE_PATTERNS;
use codesage_storage::Database;

use util::init_tracing;

pub(crate) const PROJECT_DIR: &str = ".codesage";
pub(crate) const DB_FILE: &str = "index.db";

fn parse_positive_usize(value: &str) -> Result<NonZeroUsize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|e| format!("expected a positive integer ({e})"))?;
    NonZeroUsize::new(parsed).ok_or_else(|| "expected a positive integer".to_string())
}

#[derive(Parser)]
#[command(
    name = "codesage",
    version,
    about = "Code intelligence engine for AI agents: semantic search, structural graph, impact analysis"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize CodeSage for the current project
    Init {
        /// Initialize even in a directory codesage would refuse (filesystem
        /// root, home directory, credential directories)
        #[arg(long)]
        force: bool,
    },
    /// Index the project (incremental by default). Use --full to reindex everything,
    /// --no-semantic to skip embeddings, --verbose for per-phase progress via tracing.
    Index {
        /// Force a full reindex
        #[arg(long)]
        full: bool,
        /// Skip semantic indexing (embeddings)
        #[arg(long)]
        no_semantic: bool,
        /// Skip the feature-mapping stage
        #[arg(long)]
        no_features: bool,
        /// Emit structured progress logs (set RUST_LOG=codesage=info)
        #[arg(long, short)]
        verbose: bool,
        /// Embedding batch size for this index run
        #[arg(long, value_parser = parse_positive_usize)]
        batch_size: Option<NonZeroUsize>,
        /// Wait up to SECS for the project index lock instead of skipping
        /// immediately when another indexer holds it (0 = skip, the default)
        #[arg(long, value_name = "SECS", default_value_t = 0)]
        lock_wait: u64,
    },
    /// Find symbol definitions by name
    FindSymbol {
        /// Symbol name to search for
        name: String,
        /// Filter by kind (function, method, class, trait, interface, struct, enum, constant, macro)
        #[arg(long)]
        kind: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Find all references to a symbol
    FindReferences {
        /// Symbol name to find references for
        name: String,
        /// Filter by kind (import, include, call, instantiation, inheritance, trait_use, type_hint)
        #[arg(long)]
        kind: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show import/include dependencies for a file
    Dependencies {
        /// File path (relative to project root)
        file: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Semantic code search
    Search {
        /// Natural language query
        query: String,
        /// Maximum results
        #[arg(long, default_value = "10")]
        limit: usize,
        /// Results offset for pagination
        #[arg(long, default_value = "0")]
        offset: usize,
        /// Filter by language
        #[arg(long)]
        language: Option<String>,
        /// Filter by file path glob
        #[arg(long)]
        path: Option<Vec<String>>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// What to know about a file before editing it (history facts + tests).
    /// Prints nothing when there is nothing worth saying.
    Brief {
        /// File path, repo-relative
        file: String,
        /// Output as JSON (always prints, even when empty)
        #[arg(long)]
        json: bool,
        /// Treat this as a served fire in the given session and suppress
        /// repeats: same payload already served, same path within the
        /// cooldown, or the session's token budget spent. A suppressed fire
        /// prints nothing, --json included.
        #[arg(long)]
        session: Option<String>,
    },
    /// Shortest call chain from one symbol to another
    Trace {
        /// Origin symbol
        from: String,
        /// Target symbol
        to: String,
        /// Maximum hops to search before giving up
        #[arg(long, default_value = "6")]
        max_depth: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Analyze change impact for a symbol or file
    Impact {
        /// Symbol name or file path (auto-detected)
        target: String,
        /// Treat target as a file path explicitly
        #[arg(long, conflicts_with = "symbol")]
        file: bool,
        /// Treat target as a symbol explicitly
        #[arg(long, conflicts_with = "file")]
        symbol: bool,
        /// Recursion depth (how many hops to trace)
        #[arg(long, default_value = "2")]
        depth: usize,
        /// Exclude test and config files from results
        #[arg(long)]
        source_only: bool,
        /// Also list the target's import targets (the modules/symbols it imports)
        #[arg(long)]
        forward: bool,
        /// Also list the symbols defined alongside the target in its file
        #[arg(long)]
        siblings: bool,
        /// Cap the reverse-impact result list to this many entries
        #[arg(long)]
        limit: Option<usize>,
        /// Drop per-reason detail and print a rollup summary instead
        #[arg(long)]
        summary_only: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// One-call project orientation: languages, freshness, features, risk,
    /// trust boundaries, test conventions, and suggested next calls
    Overview {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Export a context bundle for a query or symbol
    Export {
        /// Query string or symbol name
        target: String,
        /// Treat target as a symbol name instead of a query
        #[arg(long)]
        symbol: bool,
        /// Max primary results
        #[arg(long, default_value = "5")]
        limit: usize,
        /// Include caller code in the bundle
        #[arg(long)]
        callers: bool,
        /// Include callee/dependency code in the bundle
        #[arg(long)]
        callees: bool,
        /// Output format: md (default), json, or ingest (gitingest-style flat-text bundle)
        #[arg(long, value_parser = ["md", "json", "ingest"], default_value = "md")]
        format: String,
        /// Shorthand for `--format json` (cannot be combined with an explicit --format)
        #[arg(long, conflicts_with = "format")]
        json: bool,
    },
    /// Show project index status
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Run MCP server on stdio
    Mcp {
        /// Run the MCP server directly in this process instead of using the shared daemon
        #[arg(long)]
        direct: bool,
        /// Override the daemon runtime directory
        #[arg(long, hide = true)]
        runtime_dir: Option<PathBuf>,
        /// Default project root to assume when a tool call omits `project`.
        /// Set by `codesage install` for agents without a CodeSage plugin so
        /// their tool calls route to this project automatically.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Manage the shared MCP daemon (run in foreground, check status, stop)
    Daemon {
        /// Action: omit to run the daemon in the foreground (default), or
        /// pass `status` / `stop`.
        #[command(subcommand)]
        action: Option<DaemonAction>,

        /// Override the daemon runtime directory
        #[arg(long, hide = true, global = true)]
        runtime_dir: Option<PathBuf>,
    },
    /// Install git hooks for automatic reindexing
    InstallHooks {
        /// Also install a pre-commit hook that runs the repo's
        /// scripts/leak-check.sh. Opt-in: the script is repo-controlled
        /// code, so it never auto-wires on a fresh clone.
        #[arg(long)]
        with_leak_check: bool,
    },
    /// Register CodeSage as an MCP server in another agent (codex, opencode)
    Install {
        /// Agent to register with: `codex`, `opencode`, or `all`
        target: String,
        /// Register globally (user-level) instead of project-local
        #[arg(long)]
        global: bool,
    },
    /// Remove CodeSage's MCP registration from an agent (codex, opencode)
    Uninstall {
        /// Agent to remove from: `codex`, `opencode`, or `all`
        target: String,
        /// Target the global (user-level) registration instead of project-local
        #[arg(long)]
        global: bool,
    },
    /// Drop orphaned model-specific vec tables (keeps only the active model)
    Cleanup {
        /// Preview what would be dropped without making changes
        #[arg(long)]
        dry_run: bool,
    },
    /// Report what this project contains that indexing cannot see
    Coverage {
        /// Emit machine-readable JSON instead of human-readable output
        #[arg(long)]
        json: bool,
        /// Show at most this many uncovered extensions (0 = all)
        #[arg(long, default_value_t = 15)]
        top: usize,
    },
    /// Diagnose CodeSage installation: binary, CUDA, models, DB, hooks, MCP registration
    Doctor {
        /// Emit machine-readable JSON instead of human-readable output
        #[arg(long)]
        json: bool,
    },
    /// Index git history: per-file churn, fix counts, and historical co-change pairs
    GitIndex {
        /// Emit JSON stats instead of human-readable
        #[arg(long)]
        json: bool,
        /// Force a full rescan even if incremental state exists
        #[arg(long, conflicts_with = "incremental")]
        full: bool,
        /// Force incremental mode (fails open to full if no valid prior state). Default
        /// is auto: incremental if state is valid, else full.
        #[arg(long)]
        incremental: bool,
        /// Wait up to SECS for the project index lock instead of skipping
        /// immediately when another indexer holds it (0 = skip, the default)
        #[arg(long, value_name = "SECS", default_value_t = 0)]
        lock_wait: u64,
    },
    /// Top files that historically change together with the given file (V2b)
    Coupling {
        /// Repo-relative file path (e.g. src/auth/login.php)
        file: String,
        /// Max results
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Assess risk for changing a file: churn, fix-ratio, blast radius, coupling, test gap (V2b)
    Risk {
        /// Repo-relative file path (e.g. src/auth/login.php)
        file: String,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Find functions structurally similar to a named one (near-clone detection)
    Similar {
        /// Function/method name to find clones of
        symbol: String,
        /// Minimum Jaccard similarity in [0, 1]
        #[arg(long, default_value_t = 0.85)]
        min_jaccard: f32,
        /// Max results
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Aggregate risk across a patch (multiple files). Reads file paths from stdin (one per line)
    /// or from positional args. (V2b slice 2)
    RiskDiff {
        /// Repo-relative file paths. If empty, read newline-separated paths from stdin.
        files: Vec<String>,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Per-file risk for N files (no patch-level aggregation). Reads file paths from stdin or
    /// positional args. Use when you have a list of files and want individual scores;
    /// for patch aggregation use `risk-diff`.
    RiskBatch {
        /// Repo-relative file paths. If empty, read newline-separated paths from stdin.
        files: Vec<String>,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Tests that should run after editing the given files: sibling tests + co-change history.
    /// Reads file paths from stdin (one per line) or from positional args. (V2b slice 2)
    TestsFor {
        /// Repo-relative file paths. If empty, read newline-separated paths from stdin.
        files: Vec<String>,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Predict likely review objections for a patch (missing tests, risky files,
    /// blast radius, cycles, trust boundaries, feature-test gaps) before committing
    Rehearse {
        /// Repo-relative file paths. If empty, read stdin (when piped) or fall
        /// back to the working-tree changes vs HEAD.
        files: Vec<String>,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Trust-boundary tags derived from a file's imports/includes/calls (network, filesystem,
    /// secrets, process-exec, etc.). Composes into `assess_risk`. Empty when the file
    /// matches no boundary rule or has never been derived (run `codesage index`).
    TrustBoundaries {
        /// Repo-relative file path
        file: String,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Run the feature mapper: scan the repo for behavior-keyed slices
    /// (CLI commands, routes, libraries, test suites) across PHP, C, C++,
    /// Rust, Python, JS/TS, Go. Persists results into `.codesage/index.db`.
    Map {
        /// Emit JSON stats
        #[arg(long)]
        json: bool,
    },
    /// List features in the project, optionally filtered.
    FeaturesList {
        /// Filter by feature kind (cli-command, route, library, test-suite, etc.)
        #[arg(long)]
        kind: Option<String>,
        /// Filter by language (rust, php, c, cpp, python, javascript, typescript, go)
        #[arg(long)]
        lang: Option<String>,
        /// Filter by tag substring (e.g. "framework:laravel", "library")
        #[arg(long)]
        tag: Option<String>,
        /// Keep only features whose entry/owned/context files changed since
        /// this git ref (e.g. "main", "HEAD~5"). Uses `git diff <ref>...HEAD`.
        #[arg(long)]
        since: Option<String>,
        /// Limit (0 = no limit)
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Show full feature record by id (including files and trust boundaries).
    FeatureShow {
        /// Feature id (e.g. feat_abc123)
        id: String,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Find features that include the given file path in any role.
    FeatureFor {
        /// Repo-relative file path
        file: String,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Curated code bundle for one feature_id: chunks of owned/entry files,
    /// tests and context as related, plus the entry symbol's definition.
    /// Same shape as `export` but anchored on the feature's curated file list.
    FeatureBundle {
        /// Feature id (e.g. feat_abc123)
        id: String,
        /// Include caller chunks for the entry symbol
        #[arg(long)]
        include_callers: bool,
        /// Include callee chunks reached from the entry symbol
        #[arg(long)]
        include_callees: bool,
        /// Max chunks per section (primary, related)
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Snapshot the project's structural state as a session baseline.
    /// Persists to .codesage/sessions/<session-id>.json. Pair with
    /// `session-end` using the same id to detect regressions.
    SessionStart {
        /// Session id (alphanumerics, '-', '_', '.', max 128 chars).
        #[arg(long, default_value = "default")]
        session_id: String,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Diff the current structural state against a session_start snapshot.
    /// Reports new cycles, removed/added files, and per-file risk regressions.
    /// Exits non-zero when the session fails the gate (new cycles or
    /// max risk regression >= 0.10).
    SessionEnd {
        /// Session id matching the prior `session-start`.
        #[arg(long, default_value = "default")]
        session_id: String,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage the live filesystem watcher for a project (auto-reindex on edit).
    ///
    /// The daemon auto-starts a watcher per project on first use; these
    /// subcommands are for explicit control and inspection.
    Watch {
        #[command(subcommand)]
        action: WatchAction,
    },
}

#[derive(Subcommand)]
enum WatchAction {
    /// Run a watcher in the foreground for a project (own embedder; logs to
    /// stderr). Useful for debugging or when not using the daemon.
    Run {
        /// Project root to watch (defaults to the current project)
        project: Option<PathBuf>,
        /// Debounce window in milliseconds (default 1000, env REINDEX_DEBOUNCE)
        #[arg(long)]
        reindex_debounce: Option<u64>,
    },
    /// Report whether a watcher is active for a project.
    Status {
        /// Project root (defaults to the current project)
        project: Option<PathBuf>,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Stop the active watcher for a project and disable auto-restart.
    Stop {
        /// Project root (defaults to the current project)
        project: Option<PathBuf>,
    },
    /// Re-enable auto-start for a project previously stopped.
    Start {
        /// Project root (defaults to the current project)
        project: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Run the daemon in the foreground (default if no action is given)
    Run,
    /// Print the running daemon's pid + socket, or "not running"
    Status,
    /// Send SIGTERM to the running daemon and wait for it to exit
    Stop,
}

pub(crate) fn find_project_root() -> Result<PathBuf> {
    find_project_root_opt().ok_or_else(|| {
        anyhow::anyhow!("not a codesage project (no .codesage/ found). Run 'codesage init' first.")
    })
}

pub(crate) fn find_project_root_opt() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(PROJECT_DIR).is_dir() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub(crate) fn db_path(root: &Path) -> PathBuf {
    root.join(PROJECT_DIR).join(DB_FILE)
}

pub(crate) fn open_db(root: &Path) -> Result<Database> {
    Database::open(&db_path(root)).context("failed to open index database")
}

/// Read-only handle for paths that must not mutate the project. See
/// [`Database::open_read_only`]: no chmod, no migrations.
pub(crate) fn open_db_read_only(root: &Path) -> Result<Database> {
    Database::open_read_only(&db_path(root)).context("failed to open index database read-only")
}

pub(crate) fn open_db_for_model(root: &Path, model: &str, dim: usize) -> Result<Database> {
    Database::open_for_model(&db_path(root), model, dim).context("failed to open index database")
}

pub(crate) fn open_db_for_model_rebuild(root: &Path, model: &str, dim: usize) -> Result<Database> {
    Database::open_for_model_rebuild(&db_path(root), model, dim)
        .context("failed to open index database")
}

pub(crate) fn open_context_db_for_existing_model(root: &Path, model: &str) -> Result<Database> {
    Database::open_for_existing_model(&db_path(root), model)
        .context("failed to open index database")
}

/// Try to acquire the project indexing lock, polling for up to `wait`
/// (`Duration::ZERO` = single non-blocking attempt). On contention past
/// the window, prints the standard "another indexer is running …
/// {action}" line and returns `Ok(None)` so the caller can skip work
/// cleanly:
/// `let Some(_lock) = acquire_index_lock(&root, "skipping", wait)? else { return Ok(()) };`.
pub(crate) fn acquire_index_lock(
    root: &Path,
    action: &str,
    wait: Duration,
) -> Result<Option<lockfile::IndexLock>> {
    match lockfile::acquire_with_wait(root, wait)? {
        lockfile::LockOutcome::Acquired(lock) => Ok(Some(lock)),
        lockfile::LockOutcome::AlreadyHeld => {
            eprintln!(
                "another codesage indexer is running on {} — {action}",
                root.display()
            );
            Ok(None)
        }
    }
}

pub(crate) fn load_project_config(root: &Path) -> Result<ProjectConfig> {
    let config_path = root.join(PROJECT_DIR).join("config.toml");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectConfig::default());
        }
        Err(e) => {
            return Err(anyhow::Error::from(e))
                .with_context(|| format!("reading {}", config_path.display()));
        }
    };
    toml::from_str(&content).with_context(|| format!("parsing {}", config_path.display()))
}

pub(crate) fn get_exclude_patterns(config: &ProjectConfig) -> Vec<String> {
    let mut patterns: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .collect();
    let user = get_user_exclude_patterns(config);
    if !user.is_empty() {
        patterns.extend(user);
    }
    patterns
}

pub(crate) fn get_user_exclude_patterns(config: &ProjectConfig) -> Vec<String> {
    config
        .index
        .as_ref()
        .and_then(|i| i.exclude_patterns.clone())
        .unwrap_or_default()
}

/// Load config, construct embedder, open DB for its model, and optionally load a reranker.
/// Shared by `cmd_search` and `cmd_export`.
pub(crate) fn load_query_stack(
    root: &Path,
) -> Result<(
    Database,
    Embedder,
    Option<codesage_embed::reranker::Reranker>,
)> {
    let config = load_project_config(root)?;
    let emb_config = config.embedding.unwrap_or_default();
    let embedder = Embedder::new(&emb_config)?;
    let db = open_db_for_model(root, &emb_config.model, embedder.dim())?;
    let reranker = emb_config
        .reranker
        .as_ref()
        .map(|model| codesage_embed::reranker::Reranker::new(model, &emb_config.device))
        .transpose()?;
    Ok((db, embedder, reranker))
}

pub(crate) fn load_symbol_context_db(root: &Path) -> Result<Database> {
    let config = load_project_config(root)?;
    let emb_config = config.embedding.unwrap_or_default();
    open_context_db_for_existing_model(root, &emb_config.model)
}

pub(crate) fn load_index_embedder(
    no_semantic: bool,
    emb_config: &EmbeddingConfig,
) -> Result<Option<Embedder>> {
    if no_semantic {
        Ok(None)
    } else {
        Ok(Some(Embedder::new(emb_config)?))
    }
}

fn print_version_info() {
    let version = env!("CARGO_PKG_VERSION");
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let vendor = if cfg!(target_vendor = "apple") {
        "apple"
    } else if cfg!(target_vendor = "pc") {
        "pc"
    } else {
        "unknown"
    };
    let target = format!("{arch}-{vendor}-{os}");

    let features = [
        "cpu",
        #[cfg(feature = "cuda")]
        "cuda",
        #[cfg(target_vendor = "apple")]
        "coreml",
    ];

    let configured = find_project_root_opt()
        .and_then(|root| {
            let config = load_project_config(&root).ok()?;
            config.embedding.map(|e| e.device)
        })
        .unwrap_or_else(|| "none (no project config)".to_string());

    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };

    println!("codesage {version} ({profile})");
    println!("  target: {target}");
    println!("  features compiled: {}", features.join(", "));
    println!("  device configured: {configured}");
    flush_stdio();
}

fn main() {
    init_tracing();

    // Handle -V / --version before clap so it works without a subcommand and
    // can include project-local device config from .codesage/config.toml.
    // Use args_os — std::env::args() panics on non-UTF-8 argv (see cli_args test).
    let wants_version = std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == "-V" || a == "--version");
    if wants_version {
        print_version_info();
        std::process::exit(0);
    }

    // Resolve ONNX Runtime + NVIDIA library locations now, while we are still
    // single-threaded. The discovery code calls `std::env::set_var` for
    // `LD_LIBRARY_PATH` / `ORT_DYLIB_PATH`, which is `unsafe` under Rust 2024
    // because concurrent `getenv` from another thread is UB. Doing it here
    // (before clap, before any tokio runtime, before any thread spawn)
    // keeps the writes race-free even though the calls themselves remain
    // marked unsafe.
    //
    // Skipped for the `codesage mcp` stdio shim (no `--direct` flag): the
    // shim only proxies bytes between stdin and the daemon's Unix socket;
    // it never constructs an Embedder or Reranker, so eagerly preloading
    // ORT + the full CUDA/cuDNN stack just to immediately `tokio::io::copy`
    // pinned ~200 MB RSS per shim and ~1.9 GB virtual into every Claude
    // Code subagent. The `Once::call_once` fallback inside `Embedder::new`
    // / `Reranker::new` covers any future shim codepath that grew to
    // actually run inference (none today).
    // Only commands that build an Embedder/Reranker need the ORT + CUDA preload.
    // Structural/metadata commands (status, risk, find-symbol, …) must skip it:
    // `preload_cuda_libs` dlopen's the CUDA/cuDNN stack, whose native constructors
    // can hard-abort (SIGABRT) under a restricted sandbox — e.g. a Codex review
    // that runs `codesage status` as its `.codesage/index.db` onboarding probe.
    // The `Once::call_once` fallback in Embedder::new / Reranker::new still covers
    // any embedder codepath that reaches inference without a main-thread preload.
    if !is_shim_invocation() && uses_embedder() {
        codesage_embed::model::init_for_main();
    }
    let cli = Cli::parse();

    let result = run(cli);

    flush_stdio();

    let code = match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };

    // Leave the process without running any teardown. ORT's session/arena
    // teardown interacts with sqlite-vec's extension destructors in a way that
    // intermittently aborts with "corrupted double-linked list" (a glibc
    // heap-corruption diagnostic). Results are always correct and already
    // flushed; only the teardown faults.
    //
    // `std::process::exit` is NOT enough, which is why this used to still
    // abort. It skips Rust `Drop` glue, but it is a normal `exit(3)`: it still
    // runs libc `atexit` handlers and the C++ static destructors ORT registers
    // through `__cxa_atexit`, and those destructors are where the fault lives.
    // `_exit(2)` bypasses that table and goes straight to the kernel.
    //
    // Safe here because nothing is left to do: stdout and stderr were flushed
    // explicitly above, `run()` has already returned so its `Database` and
    // `Session` values were dropped normally, SQLite commits durably per
    // transaction rather than at exit, and no production path registers an
    // atexit hook or relies on a tempfile destructor. The OS reclaims memory
    // and file descriptors regardless.
    //
    // The MCP server path (`codesage mcp`) loops indefinitely and never
    // reaches this exit; when it terminates via signal, no teardown runs
    // either.
    flush_stdio();
    // SAFETY: `_exit` is async-signal-safe and always succeeds. Every buffer
    // this process owns has been flushed on the two lines above and in the
    // earlier `flush_stdio()` call.
    unsafe { libc::_exit(code) }
}

pub(crate) fn flush_stdio() {
    // Flush stdio explicitly so explicit `process::exit` calls don't drop
    // buffered output.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}

/// Cheap pre-clap detector for the `codesage mcp` stdio-shim case.
///
/// Returns true iff argv (ignoring the program name and any leading global
/// flags consumed by clap) names the `mcp` subcommand without `--direct`.
/// Matches clap's resolution loosely on purpose — false negatives here just
/// mean a non-shim invocation pays the unnecessary ORT/CUDA preload cost,
/// which is the pre-fix status quo. False positives would skip the preload
/// for a real model-loading codepath; the only way to hit one is to pass
/// `--direct` as part of a sub-subcommand, and `mcp` has no sub-subcommands.
fn is_shim_invocation() -> bool {
    // args_os(), not args(): the latter panics mid-iteration on a non-UTF-8
    // argument, and this runs before clap on EVERY invocation. Non-UTF-8 paths
    // are legal on Linux and `codesage install` writes `--project <root>`
    // verbatim into agent configs, so a non-UTF-8 root would abort startup with
    // a raw panic for every subcommand.
    is_shim_argv(std::env::args_os().skip(1))
}

fn is_shim_argv<I>(args: I) -> bool
where
    I: IntoIterator,
    I::Item: AsRef<std::ffi::OsStr>,
{
    let mut saw_mcp = false;
    let mut saw_direct = false;
    for a in args {
        let a = a.as_ref();
        if a == "--" {
            break;
        }
        if !saw_mcp {
            // Skip clap-style global flags that may precede the subcommand.
            // Today there are none, but a future `-v` / `--verbose` should
            // not flip this off accidentally. A non-UTF-8 arg (`to_str()` None)
            // is not a flag and not `mcp`, so it falls through to the non-shim
            // return below — the documented safe direction (only costs an
            // unnecessary ORT preload).
            if a.to_str().is_some_and(|s| s.starts_with('-')) {
                continue;
            }
            if a == "mcp" {
                saw_mcp = true;
                continue;
            }
            // First positional that isn't `mcp`: definitely not the shim.
            return false;
        }
        if a == "--direct" {
            saw_direct = true;
        }
    }
    saw_mcp && !saw_direct
}

/// Subcommands that construct an `Embedder`/`Reranker` (directly or via the
/// in-process server) and therefore need the single-threaded ORT + CUDA preload
/// in `main`. Every other command is structural/metadata-only (graph, git, SQL)
/// and must not pay the preload, whose CUDA dlopen can abort under a restricted
/// sandbox. `mcp` is listed for the `--direct` server; the bare `mcp` shim is
/// already excluded earlier via `is_shim_invocation`.
const EMBEDDER_COMMANDS: &[&str] = &["search", "index", "export", "watch", "daemon", "mcp"];

fn uses_embedder() -> bool {
    argv_uses_embedder(std::env::args_os().skip(1))
}

fn argv_uses_embedder<I>(args: I) -> bool
where
    I: IntoIterator,
    I::Item: AsRef<std::ffi::OsStr>,
{
    for a in args {
        let a = a.as_ref();
        if a == "--" {
            break;
        }
        // Skip clap-style global flags that may precede the subcommand.
        if a.to_str().is_some_and(|s| s.starts_with('-')) {
            continue;
        }
        // First positional token is the subcommand. A non-UTF-8 token is never a
        // known embedder command, so it falls through to the safe direction
        // (skip preload) — structural commands never embed, and the Embedder::new
        // fallback covers anything that does.
        return a.to_str().is_some_and(|s| EMBEDDER_COMMANDS.contains(&s));
    }
    false
}

fn run(cli: Cli) -> Result<()> {
    use commands::{features, hooks, index, query, risk, runtime, session};

    match cli.command {
        Commands::Init { force } => runtime::cmd_init(force),
        Commands::Index {
            full,
            no_semantic,
            no_features,
            verbose,
            batch_size,
            lock_wait,
        } => index::cmd_index(
            full,
            no_semantic,
            no_features,
            verbose,
            batch_size,
            Duration::from_secs(lock_wait),
        ),
        Commands::FindSymbol { name, kind, json } => {
            query::cmd_find_symbol(&name, kind.as_deref(), json)
        }
        Commands::FindReferences { name, kind, json } => {
            query::cmd_find_references(&name, kind.as_deref(), json)
        }
        Commands::Dependencies { file, json } => query::cmd_dependencies(&file, json),
        Commands::Search {
            query,
            limit,
            offset,
            language,
            path,
            json,
        } => commands::query::cmd_search(&query, limit, offset, language.as_deref(), path, json),
        Commands::Brief {
            file,
            json,
            session,
        } => query::cmd_brief(&file, json, session.as_deref()),
        Commands::Trace {
            from,
            to,
            max_depth,
            json,
        } => query::cmd_trace(&from, &to, max_depth, json),
        Commands::Impact {
            target,
            file,
            symbol,
            depth,
            source_only,
            forward,
            siblings,
            limit,
            summary_only,
            json,
        } => query::cmd_impact(
            &target,
            file,
            symbol,
            depth,
            source_only,
            forward,
            siblings,
            limit,
            summary_only,
            json,
        ),
        Commands::Overview { json } => session::cmd_overview(json),
        Commands::Export {
            target,
            symbol,
            limit,
            callers,
            callees,
            format,
            json,
        } => query::cmd_export(
            &target,
            symbol,
            limit,
            callers,
            callees,
            query::export_format(&format, json),
        ),
        Commands::Status { json } => index::cmd_status(json),
        Commands::Mcp {
            direct,
            runtime_dir,
            project,
        } => runtime::cmd_mcp(direct, runtime_dir, project),
        Commands::Daemon {
            action,
            runtime_dir,
        } => match action.unwrap_or(DaemonAction::Run) {
            DaemonAction::Run => runtime::cmd_daemon(runtime_dir),
            DaemonAction::Status => runtime::cmd_daemon_status(runtime_dir),
            DaemonAction::Stop => runtime::cmd_daemon_stop(runtime_dir),
        },
        Commands::InstallHooks { with_leak_check } => hooks::cmd_install_hooks(with_leak_check),
        Commands::Install { target, global } => runtime::cmd_install(&target, global),
        Commands::Uninstall { target, global } => runtime::cmd_uninstall(&target, global),
        Commands::Cleanup { dry_run } => index::cmd_cleanup(dry_run),
        Commands::Coverage { json, top } => coverage::run(json, top),
        Commands::Doctor { json } => doctor::run(json),
        Commands::GitIndex {
            json,
            full,
            incremental,
            lock_wait,
        } => risk::cmd_git_index(json, full, incremental, Duration::from_secs(lock_wait)),
        Commands::Coupling { file, limit, json } => risk::cmd_coupling(&file, limit, json),
        Commands::Risk { file, json } => risk::cmd_risk(&file, json),
        Commands::Similar {
            symbol,
            min_jaccard,
            limit,
            json,
        } => query::cmd_similar(&symbol, min_jaccard, limit, json),
        Commands::RiskDiff { files, json } => risk::cmd_risk_diff(files, json),
        Commands::RiskBatch { files, json } => risk::cmd_risk_batch(files, json),
        Commands::TestsFor { files, json } => risk::cmd_tests_for(files, json),
        Commands::Rehearse { files, json } => risk::cmd_rehearse(files, json),
        Commands::TrustBoundaries { file, json } => features::cmd_trust_boundaries(&file, json),
        Commands::Map { json } => index::cmd_map(json),
        Commands::FeaturesList {
            kind,
            lang,
            tag,
            since,
            limit,
            json,
        } => features::cmd_features_list(
            kind.as_deref(),
            lang.as_deref(),
            tag.as_deref(),
            since.as_deref(),
            limit,
            json,
        ),
        Commands::FeatureShow { id, json } => features::cmd_feature_show(&id, json),
        Commands::FeatureFor { file, json } => features::cmd_feature_for(&file, json),
        Commands::FeatureBundle {
            id,
            include_callers,
            include_callees,
            limit,
            json,
        } => features::cmd_feature_bundle(&id, include_callers, include_callees, limit, json),
        Commands::SessionStart { session_id, json } => {
            session::cmd_session_start(&session_id, json)
        }
        Commands::SessionEnd { session_id, json } => session::cmd_session_end(&session_id, json),
        Commands::Watch { action } => match action {
            WatchAction::Run {
                project,
                reindex_debounce,
            } => runtime::cmd_watch_run(project, reindex_debounce),
            WatchAction::Status { project, json } => runtime::cmd_watch_status(project, json),
            WatchAction::Stop { project } => runtime::cmd_watch_stop(project),
            WatchAction::Start { project } => runtime::cmd_watch_start(project),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_embed::config::IndexConfig;

    #[test]
    fn exclude_patterns_no_user_config_returns_defaults() {
        let cfg = ProjectConfig::default();
        let patterns = get_exclude_patterns(&cfg);
        assert_eq!(patterns.len(), DEFAULT_EXCLUDE_PATTERNS.len());
        assert!(patterns.iter().any(|p| p == "**/node_modules/**"));
    }

    #[test]
    fn exclude_patterns_user_config_extends_defaults() {
        let cfg = ProjectConfig {
            project: None,
            embedding: None,
            index: Some(IndexConfig {
                exclude_patterns: Some(vec!["**/my-custom/**".to_string()]),
                watch: None,
            }),
        };
        let patterns = get_exclude_patterns(&cfg);
        assert_eq!(patterns.len(), DEFAULT_EXCLUDE_PATTERNS.len() + 1);
        assert!(
            patterns.iter().any(|p| p == "**/node_modules/**"),
            "defaults preserved"
        );
        assert!(
            patterns.iter().any(|p| p == "**/my-custom/**"),
            "user pattern added"
        );
    }

    #[test]
    fn exclude_patterns_empty_user_list_still_returns_defaults() {
        let cfg = ProjectConfig {
            project: None,
            embedding: None,
            index: Some(IndexConfig {
                exclude_patterns: Some(vec![]),
                watch: None,
            }),
        };
        let patterns = get_exclude_patterns(&cfg);
        assert_eq!(patterns.len(), DEFAULT_EXCLUDE_PATTERNS.len());
    }

    #[test]
    fn user_exclude_patterns_returns_only_configured_patterns() {
        let cfg = ProjectConfig {
            project: None,
            embedding: None,
            index: Some(IndexConfig {
                exclude_patterns: Some(vec!["skip/**".to_string()]),
                watch: None,
            }),
        };

        assert_eq!(get_user_exclude_patterns(&cfg), vec!["skip/**"]);
    }

    #[cfg(not(feature = "cuda"))]
    #[test]
    fn index_embedder_setup_errors_when_gpu_requested_without_cuda() {
        // Must be an allowlisted model name: the validated-model gate runs
        // before the cuda-feature guard, and this test targets the latter.
        // No download happens — the guard bails before any hf-hub call.
        let cfg = EmbeddingConfig {
            model: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            device: "gpu".to_string(),
            ..EmbeddingConfig::default()
        };

        let err = match load_index_embedder(false, &cfg) {
            Ok(_) => panic!("expected gpu setup to fail without cuda feature"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("without cuda feature"),
            "unexpected error: {err:#}"
        );
    }

    #[cfg(not(feature = "cuda"))]
    #[test]
    fn index_embedder_setup_skips_model_when_no_semantic() {
        let cfg = EmbeddingConfig {
            model: "codesage-test/does-not-exist".to_string(),
            device: "gpu".to_string(),
            ..EmbeddingConfig::default()
        };

        assert!(load_index_embedder(true, &cfg).unwrap().is_none());
    }

    #[test]
    fn symbol_context_db_does_not_load_embedding_model() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let codesage_dir = root.join(PROJECT_DIR);
        std::fs::create_dir_all(&codesage_dir).unwrap();
        let model = "codesage-test/does-not-exist";
        std::fs::write(
            codesage_dir.join("config.toml"),
            format!("[embedding]\nmodel = \"{model}\"\ndevice = \"gpu\"\n"),
        )
        .unwrap();
        let db_path = codesage_dir.join(DB_FILE);
        Database::open_for_model(&db_path, model, codesage_storage::db::DEFAULT_EMBEDDING_DIM)
            .unwrap();

        let db = load_symbol_context_db(root).unwrap();

        assert_eq!(
            db.chunk_table_name(),
            "chunks_codesage_test_does_not_exist_384"
        );
    }

    #[test]
    fn load_project_config_rejects_zero_embedding_batch_size() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let codesage_dir = root.join(PROJECT_DIR);
        std::fs::create_dir_all(&codesage_dir).unwrap();
        std::fs::write(
            codesage_dir.join("config.toml"),
            "[embedding]\nmodel = \"codesage-test/model\"\ndevice = \"cpu\"\nbatch_size = 0\n",
        )
        .unwrap();

        let err = load_project_config(root).unwrap_err();
        let rendered = format!("{err:#}");

        assert!(
            rendered.contains("nonzero usize"),
            "unexpected error: {rendered}"
        );
    }

    // ---------- shim invocation detection (ORT preload skip gate) ----------

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn shim_detector_recognizes_bare_mcp() {
        assert!(is_shim_argv(argv(&["mcp"])));
    }

    #[test]
    fn shim_detector_recognizes_mcp_with_runtime_dir_override() {
        // The shim still accepts `--runtime-dir <path>` after the subcommand.
        assert!(is_shim_argv(argv(&["mcp", "--runtime-dir", "/tmp/foo"])));
    }

    #[test]
    fn shim_detector_rejects_mcp_direct() {
        // `--direct` switches to the in-process MCP server which DOES load
        // models — preload must still happen.
        assert!(!is_shim_argv(argv(&["mcp", "--direct"])));
        assert!(!is_shim_argv(argv(&[
            "mcp",
            "--direct",
            "--runtime-dir",
            "/tmp/foo"
        ])));
    }

    #[test]
    fn shim_detector_rejects_other_subcommands() {
        for cmd in [
            "daemon",
            "index",
            "search",
            "find-symbol",
            "doctor",
            "git-index",
            "init",
        ] {
            assert!(!is_shim_argv(argv(&[cmd])), "expected non-shim for {cmd}");
        }
    }

    #[test]
    fn shim_detector_rejects_empty_argv() {
        // No subcommand at all → not the shim (clap will error after).
        assert!(!is_shim_argv(argv(&[])));
    }

    #[cfg(unix)]
    #[test]
    fn shim_detector_handles_non_utf8_argv() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        // A non-UTF-8 project path (legal on Linux) must not panic. `codesage
        // mcp --project <non-utf8>` is still the shim.
        let bad = OsString::from_vec(vec![b'/', 0xff, 0xfe, b'p']);
        let args: Vec<OsString> = vec![
            OsString::from("mcp"),
            OsString::from("--project"),
            bad.clone(),
        ];
        assert!(is_shim_argv(args));
        // A non-UTF-8 first positional is treated as non-shim (no panic).
        assert!(!is_shim_argv(vec![bad]));
    }

    // ---------- embedder-preload gate (CUDA dlopen skip) ----------

    #[test]
    fn embedder_gate_preloads_for_embedding_commands() {
        for cmd in ["search", "index", "export", "watch", "daemon", "mcp"] {
            assert!(
                argv_uses_embedder(argv(&[cmd])),
                "expected preload for {cmd}"
            );
        }
    }

    #[test]
    fn embedder_gate_skips_structural_commands() {
        // These never construct an Embedder; preloading the CUDA stack for them
        // is pure waste and aborts under a restricted sandbox (the `status` case).
        for cmd in [
            "status",
            "risk",
            "risk-diff",
            "risk-batch",
            "tests-for",
            "coupling",
            "find-symbol",
            "find-references",
            "dependencies",
            "impact",
            "trust-boundaries",
            "map",
            "features-list",
            "feature-show",
            "feature-for",
            "feature-bundle",
            "overview",
            "doctor",
            "init",
        ] {
            assert!(!argv_uses_embedder(argv(&[cmd])), "expected skip for {cmd}");
        }
    }

    #[test]
    fn embedder_gate_skips_leading_global_flags() {
        // A future global flag before the subcommand must not flip the gate.
        assert!(argv_uses_embedder(argv(&["--verbose", "search"])));
        assert!(!argv_uses_embedder(argv(&["--verbose", "status"])));
    }

    #[test]
    fn embedder_gate_skips_empty_and_unknown() {
        assert!(!argv_uses_embedder(argv(&[])));
        assert!(!argv_uses_embedder(argv(&["definitely-not-a-command"])));
    }

    // ---------- clap arg contracts ----------

    #[test]
    fn export_rejects_unknown_format() {
        let err = match Cli::try_parse_from(["codesage", "export", "foo", "--format", "bogus"]) {
            Err(e) => e,
            Ok(_) => panic!("an unrecognized --format must be rejected at parse time"),
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn export_accepts_known_formats() {
        for fmt in ["md", "json", "ingest"] {
            Cli::try_parse_from(["codesage", "export", "foo", "--format", fmt])
                .unwrap_or_else(|e| panic!("--format {fmt} should parse, got {e}"));
        }
    }

    #[test]
    fn export_json_flag_parses_as_format_alias() {
        let cli = Cli::try_parse_from(["codesage", "export", "foo", "--json"]).unwrap();
        let (format, json) = match cli.command {
            Commands::Export { format, json, .. } => (format, json),
            _ => panic!("expected `export` to parse"),
        };
        assert!(json, "--json must set the flag");
        assert_eq!(format, "md", "format keeps its default; dispatch resolves");
        assert_eq!(
            commands::query::export_format(&format, json),
            "json",
            "--json must resolve to the json format at dispatch"
        );
    }

    #[test]
    fn export_json_flag_conflicts_with_explicit_format() {
        // `--json` is a shorthand, not a third channel: combining it with an
        // explicit --format is ambiguous and must be rejected at parse time.
        let err =
            match Cli::try_parse_from(["codesage", "export", "foo", "--json", "--format", "md"]) {
                Err(e) => e,
                Ok(_) => panic!("--json with an explicit --format must be rejected"),
            };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn status_accepts_json_flag() {
        let cli = Cli::try_parse_from(["codesage", "status", "--json"]).unwrap();
        assert!(matches!(cli.command, Commands::Status { json: true }));
        let cli = Cli::try_parse_from(["codesage", "status"]).unwrap();
        assert!(matches!(cli.command, Commands::Status { json: false }));
    }

    #[test]
    fn watch_run_defaults_project_to_none() {
        // No default_value on `project`, so a bare `watch run` yields None and
        // falls through to find_project_root() — matching watch status/stop/start.
        let cli = Cli::try_parse_from(["codesage", "watch", "run"]).unwrap();
        let project = match cli.command {
            Commands::Watch {
                action: WatchAction::Run { project, .. },
            } => project,
            _ => panic!("expected `watch run` to parse"),
        };
        assert!(
            project.is_none(),
            "bare `watch run` must leave project unset, got {project:?}"
        );
    }
}
