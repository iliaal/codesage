mod daemon;
mod doctor;
mod drift;
#[cfg(target_os = "android")]
mod flock_override;
mod installer;
mod lockfile;
mod mcp;
mod overview;
mod rehearsal;
mod statewatcher;
mod util;

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use codesage_embed::config::{EmbeddingConfig, ProjectConfig};
use codesage_embed::model::Embedder;
use codesage_graph::{
    assess_risk, export_context, export_context_for_symbol, find_coupling, find_references,
    find_symbol, full_index, impact_analysis, incremental_index, list_dependencies, search,
    semantic_full_index, semantic_incremental_index,
};
use codesage_parser::discover::DEFAULT_EXCLUDE_PATTERNS;
use codesage_protocol::{
    ContextBundle, ExportRequest, FileCategory, FindReferencesRequest, FindSymbolRequest,
    ImpactRequest, ImpactTarget, Language, ReferenceKind, SearchRequest, SymbolKind,
};
use codesage_storage::Database;

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
    Init,
    /// Index the project (incremental by default). Use --full to reindex everything,
    /// --no-semantic to skip embeddings, --verbose for per-phase progress via tracing.
    Index {
        /// Force a full reindex
        #[arg(long)]
        full: bool,
        /// Skip semantic indexing (embeddings)
        #[arg(long)]
        no_semantic: bool,
        /// Emit structured progress logs (set RUST_LOG=codesage=info)
        #[arg(long, short)]
        verbose: bool,
        /// Embedding batch size for this index run
        #[arg(long, value_parser = parse_positive_usize)]
        batch_size: Option<NonZeroUsize>,
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
        #[arg(long, default_value = "md")]
        format: String,
    },
    /// Show project index status
    Status,
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
    InstallHooks,
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
        /// Project root to watch
        #[arg(default_value = ".")]
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

fn find_project_root() -> Result<PathBuf> {
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

fn db_path(root: &Path) -> PathBuf {
    root.join(PROJECT_DIR).join(DB_FILE)
}

fn open_db(root: &Path) -> Result<Database> {
    Database::open(&db_path(root)).context("failed to open index database")
}

fn open_db_for_model(root: &Path, model: &str, dim: usize) -> Result<Database> {
    Database::open_for_model(&db_path(root), model, dim).context("failed to open index database")
}

fn open_db_for_model_rebuild(root: &Path, model: &str, dim: usize) -> Result<Database> {
    Database::open_for_model_rebuild(&db_path(root), model, dim)
        .context("failed to open index database")
}

fn open_context_db_for_existing_model(root: &Path, model: &str) -> Result<Database> {
    Database::open_for_existing_model(&db_path(root), model)
        .context("failed to open index database")
}

/// Try to acquire the project indexing lock. On contention, prints the
/// standard "another indexer is running … {action}" line and returns
/// `Ok(None)` so the caller can skip work cleanly:
/// `let Some(_lock) = acquire_index_lock(&root, "skipping")? else { return Ok(()) };`.
fn acquire_index_lock(root: &Path, action: &str) -> Result<Option<lockfile::IndexLock>> {
    match lockfile::try_acquire(root)? {
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

fn get_user_exclude_patterns(config: &ProjectConfig) -> Vec<String> {
    config
        .index
        .as_ref()
        .and_then(|i| i.exclude_patterns.clone())
        .unwrap_or_default()
}

fn toml_basic_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch <= '\u{1f}' || ch == '\u{7f}' => {
                use std::fmt::Write as _;
                write!(out, "\\u{:04X}", ch as u32).expect("writing to String cannot fail");
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Load config, construct embedder, open DB for its model, and optionally load a reranker.
/// Shared by `cmd_search` and `cmd_export`.
fn load_query_stack(
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

fn load_symbol_context_db(root: &Path) -> Result<Database> {
    let config = load_project_config(root)?;
    let emb_config = config.embedding.unwrap_or_default();
    open_context_db_for_existing_model(root, &emb_config.model)
}

fn load_index_embedder(
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
    if !is_shim_invocation() {
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

    // Skip Drop glue. ORT Session teardown interacts with sqlite-vec's
    // extension destructors in a way that intermittently aborts at
    // process exit with "corrupted double-linked list" (glibc heap-
    // corruption diagnostic). The crash was observed at ~1.1% rate in
    // the §2.10 semble-corpus benchmark run (2 SIGABRT out of 177 read-
    // only `codesage search` queries) — the query results were always
    // correct; only the teardown faulted. For a CLI command the OS
    // reclaims memory on exit and every write path commits explicitly
    // via execute_batch, so there's nothing useful for Drop to do.
    // Explicit exit avoids the race entirely. The MCP server path
    // (`codesage mcp`) loops indefinitely and never reaches this exit;
    // when it terminates via signal, the same skip applies.
    std::process::exit(code);
}

fn flush_stdio() {
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

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Init => cmd_init(),
        Commands::Index {
            full,
            no_semantic,
            verbose,
            batch_size,
        } => cmd_index(full, no_semantic, verbose, batch_size),
        Commands::FindSymbol { name, kind, json } => cmd_find_symbol(&name, kind.as_deref(), json),
        Commands::FindReferences { name, kind, json } => {
            cmd_find_references(&name, kind.as_deref(), json)
        }
        Commands::Dependencies { file, json } => cmd_dependencies(&file, json),
        Commands::Search {
            query,
            limit,
            offset,
            language,
            path,
            json,
        } => cmd_search(&query, limit, offset, language.as_deref(), path, json),
        Commands::Impact {
            target,
            file,
            symbol,
            depth,
            source_only,
            json,
        } => cmd_impact(&target, file, symbol, depth, source_only, json),
        Commands::Overview { json } => cmd_overview(json),
        Commands::Export {
            target,
            symbol,
            limit,
            callers,
            callees,
            format,
        } => cmd_export(&target, symbol, limit, callers, callees, &format),
        Commands::Status => cmd_status(),
        Commands::Mcp {
            direct,
            runtime_dir,
            project,
        } => cmd_mcp(direct, runtime_dir, project),
        Commands::Daemon {
            action,
            runtime_dir,
        } => match action.unwrap_or(DaemonAction::Run) {
            DaemonAction::Run => cmd_daemon(runtime_dir),
            DaemonAction::Status => cmd_daemon_status(runtime_dir),
            DaemonAction::Stop => cmd_daemon_stop(runtime_dir),
        },
        Commands::InstallHooks => cmd_install_hooks(),
        Commands::Install { target, global } => cmd_install(&target, global),
        Commands::Uninstall { target, global } => cmd_uninstall(&target, global),
        Commands::Cleanup { dry_run } => cmd_cleanup(dry_run),
        Commands::Doctor { json } => doctor::run(json),
        Commands::GitIndex {
            json,
            full,
            incremental,
        } => cmd_git_index(json, full, incremental),
        Commands::Coupling { file, limit, json } => cmd_coupling(&file, limit, json),
        Commands::Risk { file, json } => cmd_risk(&file, json),
        Commands::RiskDiff { files, json } => cmd_risk_diff(files, json),
        Commands::RiskBatch { files, json } => cmd_risk_batch(files, json),
        Commands::TestsFor { files, json } => cmd_tests_for(files, json),
        Commands::Rehearse { files, json } => cmd_rehearse(files, json),
        Commands::TrustBoundaries { file, json } => cmd_trust_boundaries(&file, json),
        Commands::Map { json } => cmd_map(json),
        Commands::FeaturesList {
            kind,
            lang,
            tag,
            since,
            limit,
            json,
        } => cmd_features_list(
            kind.as_deref(),
            lang.as_deref(),
            tag.as_deref(),
            since.as_deref(),
            limit,
            json,
        ),
        Commands::FeatureShow { id, json } => cmd_feature_show(&id, json),
        Commands::FeatureFor { file, json } => cmd_feature_for(&file, json),
        Commands::FeatureBundle {
            id,
            include_callers,
            include_callees,
            limit,
            json,
        } => cmd_feature_bundle(&id, include_callers, include_callees, limit, json),
        Commands::SessionStart { session_id, json } => cmd_session_start(&session_id, json),
        Commands::SessionEnd { session_id, json } => cmd_session_end(&session_id, json),
        Commands::Watch { action } => match action {
            WatchAction::Run {
                project,
                reindex_debounce,
            } => cmd_watch_run(project, reindex_debounce),
            WatchAction::Status { project, json } => cmd_watch_status(project, json),
            WatchAction::Stop { project } => cmd_watch_stop(project),
            WatchAction::Start { project } => cmd_watch_start(project),
        },
    }
}

fn cmd_mcp(direct: bool, runtime_dir: Option<PathBuf>, project: Option<PathBuf>) -> Result<()> {
    // Canonicalize the default project to an absolute path so injected
    // tool-call args resolve regardless of the agent's cwd.
    let default_project = match project {
        Some(p) => {
            let canon = std::fs::canonicalize(&p)
                .with_context(|| format!("resolving --project path {}", p.display()))?;
            Some(
                canon
                    .to_str()
                    .with_context(|| {
                        format!("--project path is not valid UTF-8: {}", canon.display())
                    })?
                    .to_owned(),
            )
        }
        None => None,
    };
    let rt = tokio::runtime::Runtime::new()?;
    if direct {
        rt.block_on(mcp::run_mcp_server(default_project))
    } else {
        rt.block_on(daemon::run_mcp_shim(runtime_dir, default_project))
    }
}

fn cmd_daemon(runtime_dir: Option<PathBuf>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(daemon::run_daemon(runtime_dir))
}

fn cmd_daemon_status(runtime_dir: Option<PathBuf>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(daemon::run_daemon_status(runtime_dir))
}

fn cmd_daemon_stop(runtime_dir: Option<PathBuf>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(daemon::run_daemon_stop(runtime_dir))
}

/// Resolve the target list from a CLI argument (`all` or a single id).
fn resolve_install_targets(target: &str) -> Result<Vec<Box<dyn installer::AgentTarget>>> {
    if target == "all" {
        return Ok(installer::all_targets());
    }
    match installer::target_by_id(target) {
        Some(t) => Ok(vec![t]),
        None => {
            let ids: Vec<&str> = installer::all_targets().iter().map(|t| t.id()).collect();
            bail!(
                "unknown agent '{target}'. Known: {}, or 'all'",
                ids.join(", ")
            );
        }
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set; cannot resolve agent config paths"))
}

fn canonical_project_utf8() -> Result<(PathBuf, String)> {
    let root = find_project_root()?;
    let canon = std::fs::canonicalize(&root).unwrap_or(root);
    let utf8 = canon
        .to_str()
        .with_context(|| format!("project path is not valid UTF-8: {}", canon.display()))?
        .to_owned();
    Ok((canon, utf8))
}

fn cmd_install(target: &str, global: bool) -> Result<()> {
    let (project, project_utf8) = canonical_project_utf8()?;
    let home = home_dir()?;
    let targets = resolve_install_targets(target)?;
    let ctx = installer::InstallCtx {
        home: &home,
        project: &project,
        project_utf8: &project_utf8,
        global,
    };
    for t in &targets {
        let path = t.config_path(&ctx);
        match t.install(&ctx)? {
            installer::InstallOutcome::Wrote => {
                println!("registered {} → {}", t.display_name(), path.display());
            }
            installer::InstallOutcome::Unchanged => {
                println!(
                    "{} already up to date ({})",
                    t.display_name(),
                    path.display()
                );
            }
        }
    }
    println!(
        "\nCodeSage MCP server registered for project: {}",
        project.display()
    );
    Ok(())
}

fn cmd_uninstall(target: &str, global: bool) -> Result<()> {
    let (project, project_utf8) = canonical_project_utf8()?;
    let home = home_dir()?;
    let targets = resolve_install_targets(target)?;
    let ctx = installer::InstallCtx {
        home: &home,
        project: &project,
        project_utf8: &project_utf8,
        global,
    };
    for t in &targets {
        let path = t.config_path(&ctx);
        match t.uninstall(&ctx)? {
            installer::UninstallOutcome::Removed => {
                println!("removed {} ({})", t.display_name(), path.display());
            }
            installer::UninstallOutcome::NotConfigured => {
                println!("{} was not configured", t.display_name());
            }
        }
    }
    Ok(())
}

fn cmd_install_hooks() -> Result<()> {
    let root = find_project_root()?;
    if !root.join(".git").exists() {
        bail!("not a git repository (no .git at project root)");
    }

    let (hooks_dir, is_husky) = resolve_hooks_dir(&root)?;
    std::fs::create_dir_all(&hooks_dir)?;

    let codesage_bin =
        std::env::current_exe().context("resolving current_exe for git hook body")?;
    let codesage_path = codesage_bin
        .to_str()
        .with_context(|| {
            format!(
                "codesage binary path is not valid UTF-8: {}",
                codesage_bin.display()
            )
        })?
        .to_owned();

    // Background indexers run niced + ionice'd so they can't soak the foreground.
    // `nice` is portable; `ionice` is Linux-only (util-linux), gated on `command -v`
    // so the hook stays a no-op on macOS / *BSD instead of failing.
    let hook_body = generate_post_commit_hook_body(&codesage_path);

    // post-rewrite fires on amend/rebase. It reshapes history, so the stored last_sha may
    // no longer be an ancestor of HEAD — incremental mode detects this and falls back to
    // full automatically, so we can safely reuse the same body here.
    let hook_names = ["post-commit", "post-merge", "post-checkout", "post-rewrite"];
    let mut installed: Vec<PathBuf> = Vec::new();
    for name in &hook_names {
        let path = hooks_dir.join(name);
        if path.exists() {
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            if !existing.contains("codesage install-hooks") {
                println!(
                    "skip: {} already exists and is not a codesage hook",
                    path.display()
                );
                continue;
            }
        }

        std::fs::write(&path, &hook_body)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }

        println!("installed: {}", path.display());
        installed.push(path);
    }

    if is_husky && !installed.is_empty() {
        exclude_husky_hook_paths(&root, &installed)?;
    }

    install_leak_check_hook(&root, &hooks_dir, is_husky, &mut installed)?;

    Ok(())
}

/// Generate the post-commit/post-merge/post-checkout/post-rewrite hook body
/// that runs structural+semantic index then git-history index sequentially
/// in one background subshell. Both subcommands take the same project
/// lock; if launched in parallel one would silently skip on lock
/// contention, with no log visibility because stdout/stderr are
/// redirected to /dev/null. Sequencing makes both passes always run.
fn generate_post_commit_hook_body(bin: &str) -> String {
    let bin = shell_single_quote(bin);
    format!(
        "#!/bin/sh\n\
         # installed by codesage install-hooks\n\
         root=\"$(git rev-parse --show-toplevel 2>/dev/null)\" || exit 0\n\
         NICE=\"nice -n 19\"\n\
         IONICE=\"\"\n\
         command -v ionice >/dev/null 2>&1 && IONICE=\"ionice -c 3\"\n\
         # Run structural+semantic index then git-history index sequentially\n\
         # in one background subshell. Both subcommands take the same\n\
         # project lock and would silently skip on contention if launched in\n\
         # parallel — losing whichever lost the race, with no log visibility\n\
         # because stdout/stderr are redirected to /dev/null.\n\
         [ -d \"$root/.codesage\" ] && ( cd \"$root\" && \\\n\
           $IONICE $NICE {bin} index && \\\n\
           $IONICE $NICE {bin} git-index --incremental ) >/dev/null 2>&1 &\n\
         exit 0\n",
    )
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Install a pre-commit leak-check hook if the repo ships `scripts/leak-check.sh`.
/// Keeps the hook a thin wrapper that invokes the repo's own script so the pattern
/// list and script logic can be iterated without re-running install-hooks.
fn install_leak_check_hook(
    root: &std::path::Path,
    hooks_dir: &std::path::Path,
    is_husky: bool,
    installed: &mut Vec<PathBuf>,
) -> Result<()> {
    let script = root.join("scripts/leak-check.sh");
    if !script.exists() {
        return Ok(());
    }

    let path = hooks_dir.join("pre-commit");
    if path.exists() {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if !existing.contains("codesage install-hooks") {
            println!(
                "skip: {} already exists and is not a codesage hook",
                path.display()
            );
            return Ok(());
        }
    }

    let body = "#!/bin/sh\n\
                # installed by codesage install-hooks\n\
                root=\"$(git rev-parse --show-toplevel 2>/dev/null)\" || exit 0\n\
                script=\"$root/scripts/leak-check.sh\"\n\
                [ -x \"$script\" ] || exit 0\n\
                exec \"$script\"\n";
    std::fs::write(&path, body)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    println!("installed: {} (leak-check)", path.display());
    installed.push(path.clone());

    if is_husky {
        exclude_husky_hook_paths(root, std::slice::from_ref(&path))?;
    }

    Ok(())
}

fn resolve_hooks_dir(root: &std::path::Path) -> Result<(PathBuf, bool)> {
    let configured = std::process::Command::new("git")
        .arg("config")
        .arg("--get")
        .arg("core.hooksPath")
        .current_dir(root)
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            } else {
                None
            }
        });

    match configured {
        None => {
            let common = git_common_dir(root)
                .ok_or_else(|| anyhow::anyhow!("unable to resolve git common dir"))?;
            Ok((common.join("hooks"), false))
        }
        Some(raw) => {
            let path = std::path::Path::new(&raw);
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            // A `core.hooksPath` that resolves to the default `<git_common>/hooks`
            // is a no-op redundancy; treat it like an unset value rather than
            // refusing. Seen in the wild on PHP-extension repos that share a
            // config template.
            if let Some(common) = git_common_dir(root) {
                let default_hooks = common.join("hooks");
                if util::paths_resolve_same(&resolved, &default_hooks) {
                    return Ok((default_hooks, false));
                }
            }
            if resolved.join("h").is_file() || resolved.join("husky.sh").is_file() {
                let user_dir = resolved
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("husky hooks dir has no parent"))?
                    .to_path_buf();
                Ok((user_dir, true))
            } else {
                bail!(
                    "core.hooksPath is set to {} but it does not look like a Husky setup; \
                     refusing to install hooks. Install manually or clear core.hooksPath.",
                    resolved.display()
                );
            }
        }
    }
}

use util::{format_bytes, git_common_dir, init_tracing};

fn exclude_husky_hook_paths(root: &std::path::Path, hooks: &[PathBuf]) -> Result<()> {
    let Some(exclude) = git_local_exclude_path(root) else {
        return Ok(());
    };
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let mut to_add: Vec<String> = Vec::new();
    for hook in hooks {
        let Ok(rel) = hook.strip_prefix(root) else {
            continue;
        };
        let line = format!("/{}", rel.display());
        if !existing.lines().any(|l| l.trim() == line.trim()) {
            to_add.push(line);
        }
    }
    if to_add.is_empty() {
        return Ok(());
    }
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&exclude)?;
    use std::io::Write;
    writeln!(f, "\n# codesage husky hooks")?;
    for line in &to_add {
        writeln!(f, "{line}")?;
    }
    println!(
        "    added {} husky hook path(s) to .git/info/exclude",
        to_add.len()
    );
    Ok(())
}

fn cmd_init() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let project_dir = cwd.join(PROJECT_DIR);

    match std::fs::symlink_metadata(&project_dir) {
        Ok(meta) if meta.is_dir() => {
            println!("Already initialized in {}", cwd.display());
            return Ok(());
        }
        Ok(_) => {
            bail!("{} exists but is not a directory", project_dir.display());
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("stat {}", project_dir.display())),
    }

    std::fs::create_dir_all(&project_dir)?;

    let dir_name = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());

    std::fs::write(
        project_dir.join("config.toml"),
        format!(
            "[project]\nname = {}\n\n\
             [embedding]\nmodel = \"jinaai/jina-embeddings-v2-base-code\"\ndevice = \"gpu\"\nreranker = \"cross-encoder/ms-marco-MiniLM-L6-v2\"\n\
             # Optional. Defaults to 64 on non-Apple targets and 10 on Apple targets.\n\
             # batch_size = 64\n\n\
             [index]\n\
             # Built-in defaults always apply (vendored deps, build outputs, lock files,\n\
             # caches, IDE state). Test files are indexed structurally and semantically,\n\
             # then demoted at search time. See DEFAULT_EXCLUDE_PATTERNS in\n\
             # crates/parser/src/discover.rs. Patterns listed here ADD to the defaults.\n\
             exclude_patterns = []\n",
            toml_basic_string(&dir_name),
        ),
    )?;

    if cwd.join(".git").exists()
        && let Some(exclude) = git_local_exclude_path(&cwd)
    {
        let content = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !content.contains(".codesage") {
            if let Some(parent) = exclude.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&exclude)?;
            use std::io::Write;
            writeln!(f, "\n# codesage index\n/.codesage/")?;
        }
    }

    println!("Initialized CodeSage in {}", cwd.display());
    Ok(())
}

fn git_local_exclude_path(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    Some(git_common_dir(cwd)?.join("info").join("exclude"))
}

fn cmd_index(
    full: bool,
    no_semantic: bool,
    verbose: bool,
    batch_size_override: Option<NonZeroUsize>,
) -> Result<()> {
    let root = find_project_root()?;
    // Acquire the project-level indexing lock before loading embedders or
    // touching the DB. Skips work cleanly (exit 0) if another codesage
    // indexer is already running on this project — the concurrency-audit
    // finding from recommendations doc §2.4 said the previous behavior was
    // "one process wins with rc=0, loser dies with SQLITE_BUSY", which
    // looks like a failure in hook logs even though no data is at risk.
    let Some(_lock) = acquire_index_lock(&root, "skipping")? else {
        return Ok(());
    };
    let config = load_project_config(&root)?;
    let excludes = get_exclude_patterns(&config);

    let mut emb_config = config.embedding.unwrap_or_default();
    if let Some(n) = batch_size_override {
        emb_config.set_batch_size_override(n);
    }

    if verbose {
        let batch_size = emb_config
            .effective_batch_size()
            .map_err(anyhow::Error::msg)?;
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

    let mut embedder = load_index_embedder(no_semantic, &emb_config)?;

    let db = match embedder.as_ref() {
        Some(e) if full => open_db_for_model_rebuild(&root, &emb_config.model, e.dim())?,
        Some(e) => open_db_for_model(&root, &emb_config.model, e.dim())?,
        None => open_db(&root)?,
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
            files_removed = stats.files_removed,
            symbols = stats.symbols_found,
            refs = stats.references_found,
            "structural index complete"
        );
    } else {
        println!(
            "Structural: {} files ({} skipped, {} failed, {} removed), {} symbols, {} references",
            stats.files_indexed,
            stats.files_skipped,
            stats.files_failed,
            stats.files_removed,
            stats.symbols_found,
            stats.references_found
        );
    }

    // Targeted trust-boundary backfill. `files_pending_boundary_derivation`
    // returns files that have never been derived (or were indexed before
    // the marker column existed); the structural indexer only derives
    // inline for files it parses this pass, so the catch-up here picks
    // up the rest without reprocessing rule-clean files.
    match db.files_pending_boundary_derivation() {
        Ok(pending) if !pending.is_empty() => {
            let n_pending = pending.len();
            if verbose {
                tracing::info!(pending = n_pending, "backfilling trust boundaries");
            }
            match codesage_features::derive_for_files(&db, &pending) {
                Ok(n) => {
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
    if verbose {
        tracing::info!("mapping features");
    }
    match codesage_features::map_features(&root, &db, &excludes) {
        Ok(map_stats) => {
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

    if let Some(embedder) = embedder.as_mut() {
        let sem_stats = if full {
            semantic_full_index(&root, &db, embedder, &excludes, verbose)?
        } else {
            semantic_incremental_index(&root, &db, embedder, &excludes, verbose)?
        };
        if verbose {
            tracing::info!(
                files_processed = sem_stats.files_processed,
                files_skipped = sem_stats.files_skipped,
                files_failed = sem_stats.files_failed,
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
        }
    }

    // Stamp the HEAD SHA we just indexed against. Skipped in non-git dirs.
    // Failures here only degrade drift telemetry, so they warn rather than
    // propagate — the index itself is already durable on disk.
    if let Some(sha) = drift::git_head_sha(&root)
        && let Err(e) = db.set_structural_index_state(&sha)
    {
        tracing::warn!(error = %e, "failed to stamp structural_index_state");
    }

    Ok(())
}

fn cmd_find_symbol(name: &str, kind_str: Option<&str>, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;

    let kind = kind_str.and_then(SymbolKind::parse);
    let results = find_symbol(
        &db,
        &FindSymbolRequest {
            name: name.to_string(),
            kind,
        },
    )?;

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else if results.is_empty() {
        println!("No symbols found for '{name}'");
    } else {
        for s in &results {
            println!(
                "{} {} -- {}:{}",
                s.kind, s.qualified_name, s.file_path, s.line_start
            );
        }
    }
    Ok(())
}

fn cmd_find_references(name: &str, kind_str: Option<&str>, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;

    let kind = kind_str.and_then(ReferenceKind::parse);
    let results = find_references(
        &db,
        &FindReferencesRequest {
            symbol_name: name.to_string(),
            kind,
        },
    )?;

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else if results.is_empty() {
        println!("No references found for '{name}'");
    } else {
        for r in &results {
            let ctx = r.from_symbol.as_deref().unwrap_or("top-level");
            println!(
                "{} {} -- {}:{} (in {})",
                r.kind, r.to_name, r.from_file, r.line, ctx
            );
        }
    }
    Ok(())
}

fn cmd_search(
    query: &str,
    limit: usize,
    offset: usize,
    language: Option<&str>,
    paths: Option<Vec<String>>,
    json: bool,
) -> Result<()> {
    let root = find_project_root()?;
    let (db, mut embedder, mut reranker) = load_query_stack(&root)?;

    let languages = language.and_then(|l| Language::parse(l).map(|lang| vec![lang]));

    let req = SearchRequest {
        query: query.to_string(),
        limit: Some(limit),
        offset: Some(offset),
        languages,
        paths,
    };

    let query_embedding = embedder.embed_one(&req.query)?;
    let rerank_fn: Option<codesage_graph::RerankFn<'_>> = reranker.as_mut().map(|r| {
        Box::new(move |q: &str, docs: &[&str]| r.score_pairs(q, docs))
            as Box<dyn FnMut(&str, &[&str]) -> Result<Vec<f32>>>
    });
    let results = search(&db, &query_embedding, rerank_fn, &req)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else if results.is_empty() {
        println!("No results found for '{query}'");
    } else {
        for r in &results {
            let preview: String = r.content.chars().take(120).collect();
            let preview = preview.replace('\n', " ");
            println!(
                "{:.1}% {}:{}-{} ({}) {}",
                r.score * 100.0,
                r.file_path,
                r.start_line,
                r.end_line,
                r.language,
                preview
            );
        }
    }
    Ok(())
}

fn cmd_dependencies(file: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;

    let deps = list_dependencies(&db, file)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&deps)?);
    } else {
        println!("File: {}", deps.file_path);
        if deps.imports.is_empty() {
            println!("\nImports: (none)");
        } else {
            println!("\nImports:");
            for imp in &deps.imports {
                println!("  {imp}");
            }
        }
        if deps.imported_by.is_empty() {
            println!("\nImported by: (none)");
        } else {
            println!("\nImported by:");
            for by in &deps.imported_by {
                println!("  {by}");
            }
        }
    }
    Ok(())
}

fn cmd_git_index(json: bool, full: bool, incremental: bool) -> Result<()> {
    let root = find_project_root()?;
    // Same lock as `codesage index`: if a structural index is in flight,
    // the git-history pass would race it and hit SQLITE_BUSY. Skipping
    // here lets the hook-driven scheduler converge on a single indexer
    // at a time without the user seeing an error.
    let Some(_lock) = acquire_index_lock(&root, "skipping")? else {
        return Ok(());
    };
    let db = open_db(&root)?;
    let config = load_project_config(&root)?;
    let excludes = get_user_exclude_patterns(&config);
    let mode = if full {
        codesage_graph::IndexMode::Full
    } else if incremental {
        codesage_graph::IndexMode::Incremental
    } else {
        codesage_graph::IndexMode::Auto
    };
    let stats = codesage_graph::git_history_index_with_options(&db, &root, &excludes, mode)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!(
            "Git history indexed ({mode:?}): commits_scanned={} files_tracked={} co_change_pairs={}",
            stats.commits_scanned, stats.files_tracked, stats.co_change_pairs
        );
    }
    Ok(())
}

fn cmd_coupling(file: &str, limit: usize, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let report = find_coupling(&db, file, limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if report.coupled.is_empty() {
        let hint = report
            .note
            .as_deref()
            .unwrap_or("no co-change history; run `codesage git-index` or check the path");
        println!("No co-change history for {file}: {hint}");
    } else {
        println!(
            "Files that historically change with {file} ({} commits tracked):",
            report.file_commits
        );
        for e in &report.coupled {
            println!("  {:>6.2}  {:>4}x  {}", e.weight, e.count, e.file);
        }
    }
    Ok(())
}

fn cmd_risk(file: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let assessment = assess_risk(&db, file)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&assessment)?);
    } else {
        println!(
            "Risk: {} (score: {:.2}/1.00)",
            assessment.file, assessment.score
        );
        println!(
            "  churn={:.2} (percentile {:.0}%) | fix={}/{} ({:.0}%) | dependents={} | coupled={} | test_gap={}",
            assessment.churn_score,
            assessment.churn_percentile * 100.0,
            assessment.fix_count,
            assessment.total_commits,
            assessment.fix_ratio * 100.0,
            assessment.dependent_files,
            assessment.coupled_files,
            assessment.test_gap,
        );
        if !assessment.trust_boundaries.is_empty() {
            let names: Vec<&str> = assessment
                .trust_boundaries
                .iter()
                .map(|b| b.as_str())
                .collect();
            println!("  trust_boundaries: {}", names.join(", "));
        }
        if !assessment.notes.is_empty() {
            println!("  Notes:");
            for n in &assessment.notes {
                println!("    - {n}");
            }
        }
        if !assessment.top_coupled.is_empty() {
            println!("  Top coupled:");
            for c in assessment.top_coupled.iter().take(5) {
                println!("    {:>5.2}  {}", c.weight, c.file);
            }
        }
        if !assessment.top_symbols.is_empty() {
            println!("  Top symbols:");
            for s in &assessment.top_symbols {
                println!("    L{:<5} {} ({}) — {}", s.line, s.name, s.kind, s.why);
            }
        }
    }
    Ok(())
}

/// Resolve a file-list argument: positional args if non-empty, else newline-separated
/// from stdin. Used by `risk-diff` and `tests-for` so they compose with `git diff
/// --name-only` and similar pipelines.
fn resolve_file_list(files: Vec<String>) -> Result<Vec<String>> {
    if !files.is_empty() {
        return Ok(files);
    }
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

fn cmd_risk_diff(files: Vec<String>, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let files = resolve_file_list(files)?;
    if files.is_empty() {
        bail!("no file paths provided (pass as args or pipe via stdin)");
    }
    let assessment = codesage_graph::assess_risk_diff(&db, &files)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&assessment)?);
    } else {
        println!(
            "Patch risk: {} file(s) | max={:.2} mean={:.2}",
            assessment.files.len(),
            assessment.max_score,
            assessment.mean_score
        );
        if let Some(top) = &assessment.max_risk_file {
            println!("  highest-risk file: {top}");
        }
        for label in [
            ("hotspot", &assessment.hotspot_files),
            ("fix-heavy", &assessment.fix_heavy_files),
            ("test gap", &assessment.test_gap_files),
            ("wide blast radius", &assessment.wide_blast_files),
        ] {
            if !label.1.is_empty() {
                println!("  {} ({}):", label.0, label.1.len());
                for f in label.1 {
                    println!("    - {f}");
                }
            }
        }
        if !assessment.summary_notes.is_empty() {
            println!("  Notes:");
            for n in &assessment.summary_notes {
                println!("    - {n}");
            }
        }
    }
    Ok(())
}

/// Resolve the patch file list for `rehearse`: explicit args, else piped stdin,
/// else the working-tree changes vs HEAD. Lets the command run both in a
/// pipeline (`git diff --name-only | codesage rehearse`) and bare in a dirty
/// working tree.
fn resolve_patch_files(root: &Path, files: Vec<String>) -> Result<Vec<String>> {
    if !files.is_empty() {
        return Ok(files);
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return resolve_file_list(Vec::new());
    }
    working_tree_changes(root)
}

/// Files changed in the working tree relative to HEAD (tracked modifications,
/// staged or not). Empty on a clean tree or when git is unavailable.
fn working_tree_changes(root: &Path) -> Result<Vec<String>> {
    let out = std::process::Command::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .current_dir(root)
        .output()?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

fn cmd_rehearse(files: Vec<String>, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let files = resolve_patch_files(&root, files)?;
    if files.is_empty() {
        bail!(
            "no changed files (pass paths as args, pipe via stdin, or make working-tree changes)"
        );
    }
    let rehearsal = crate::rehearsal::build_review_rehearsal(&root, &db, &files)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rehearsal)?);
        return Ok(());
    }
    println!(
        "Review rehearsal: {} file(s), {} objection(s)",
        rehearsal.files.len(),
        rehearsal.objections.len()
    );
    for o in &rehearsal.objections {
        println!("  [{}] {} — {}", o.severity.as_str(), o.category, o.title);
        for e in &o.evidence {
            println!("      {e}");
        }
    }
    if !rehearsal.summary_notes.is_empty() {
        println!("Summary:");
        for n in &rehearsal.summary_notes {
            println!("  - {n}");
        }
    }
    Ok(())
}

fn cmd_map(json: bool) -> Result<()> {
    let root = find_project_root()?;
    // `map_features` writes the feature tables in multiple transactions and runs
    // a GC pass; take the same project writer lock that cmd_index / cmd_git_index
    // / cmd_cleanup hold so a manual `codesage map` doesn't race the background
    // hook-driven indexer (which maps features itself) into SQLITE_BUSY or a
    // partial multi-transaction state. Skip if an indexer already holds it.
    let Some(_lock) = acquire_index_lock(&root, "skipping map")? else {
        return Ok(());
    };
    let db = open_db(&root)?;
    let config = load_project_config(&root)?;
    let excludes = get_exclude_patterns(&config);
    let stats = codesage_features::map_features(&root, &db, &excludes)?;
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

fn cmd_features_list(
    kind: Option<&str>,
    lang: Option<&str>,
    tag: Option<&str>,
    since: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<()> {
    use codesage_protocol::{FeatureKind, Language};
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let kind = match kind {
        None => None,
        Some(k) => Some(
            FeatureKind::parse(k).ok_or_else(|| anyhow::anyhow!("unknown feature kind: {k}"))?,
        ),
    };
    let language = match lang {
        None => None,
        Some(l) => {
            Some(Language::parse(l).ok_or_else(|| anyhow::anyhow!("unknown language: {l}"))?)
        }
    };
    // With `--since`, fetch unbounded then cap after the changed-file
    // intersection — the SQL LIMIT runs before our filter, so a default
    // limit would truncate candidates the diff filter hasn't seen yet.
    let query_limit = if since.is_some() { 0 } else { limit };
    let mut features = db.list_features(kind, language, tag, query_limit)?;
    if let Some(git_ref) = since {
        let changed = codesage_graph::changed_files_since(&root, git_ref)?;
        features.retain(|f| codesage_graph::feature_touched_since(&f.files, &changed));
        if limit > 0 && features.len() > limit {
            features.truncate(limit);
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&codesage_protocol::FeatureListResults {
                results: features,
            })?
        );
    } else if features.is_empty() {
        println!("No features matched.");
    } else {
        println!("Features ({}):", features.len());
        for f in &features {
            let disc = f
                .entry_command
                .as_deref()
                .or(f.entry_route.as_deref())
                .or(f.entry_symbol.as_deref())
                .unwrap_or("");
            println!(
                "  {:<22} {:<14} {:<10} {:<12} {}",
                f.feature_id,
                f.kind.as_str(),
                f.language.as_str(),
                disc,
                f.title
            );
        }
    }
    Ok(())
}

fn cmd_feature_show(id: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let feature = match db.load_feature(id)? {
        Some(f) => f,
        None => bail!("no feature with id `{id}` in this project"),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&feature)?);
    } else {
        println!("{} ({})", feature.title, feature.feature_id);
        println!("  kind: {}", feature.kind.as_str());
        println!("  source: {}", feature.source);
        println!("  language: {}", feature.language.as_str());
        println!("  confidence: {}", feature.confidence.as_str());
        println!("  entry: {}", feature.entry_path);
        if let Some(s) = &feature.entry_symbol {
            println!("  entry_symbol: {s}");
        }
        if let Some(r) = &feature.entry_route {
            println!("  entry_route: {r}");
        }
        if let Some(c) = &feature.entry_command {
            println!("  entry_command: {c}");
        }
        if let Some(c) = &feature.test_command {
            println!("  test_command: {c}");
        }
        if !feature.tags.is_empty() {
            println!("  tags: {}", feature.tags.join(", "));
        }
        if !feature.trust_boundaries.is_empty() {
            let names: Vec<&str> = feature
                .trust_boundaries
                .iter()
                .map(|b| b.as_str())
                .collect();
            println!("  trust_boundaries: {}", names.join(", "));
        }
        println!("  files ({}):", feature.files.len());
        for f in &feature.files {
            let reason = f.reason.as_deref().unwrap_or("");
            println!("    {:<8} {} ({reason})", f.role.as_str(), f.path);
        }
    }
    Ok(())
}

fn cmd_feature_bundle(
    id: &str,
    include_callers: bool,
    include_callees: bool,
    limit: usize,
    json: bool,
) -> Result<()> {
    let root = find_project_root()?;
    // Open against the configured embedding model so `primary` / `related`
    // resolve real chunks. The default-model `open_db` points at the
    // MiniLM 384-dim chunk table and returns empty content on projects
    // configured for a different model (e.g. php-src uses jina v2 768-dim).
    let db = load_symbol_context_db(&root)?;
    let bundle = codesage_graph::feature_bundle(&db, id, include_callers, include_callees, limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&bundle)?);
    } else {
        println!("{}", bundle.target_description);
        println!("  primary ({}):", bundle.primary.len());
        for r in &bundle.primary {
            println!(
                "    {}:{}-{} ({:.0} chars)",
                r.file_path,
                r.start_line,
                r.end_line,
                r.content.chars().count()
            );
        }
        if !bundle.related.is_empty() {
            println!("  related ({}):", bundle.related.len());
            for r in &bundle.related {
                println!(
                    "    {}:{}-{} ({:.0} chars)",
                    r.file_path,
                    r.start_line,
                    r.end_line,
                    r.content.chars().count()
                );
            }
        }
        if !bundle.symbol_definitions.is_empty() {
            println!("  symbols ({}):", bundle.symbol_definitions.len());
            for s in &bundle.symbol_definitions {
                println!(
                    "    {} ({}) @ {}:{}",
                    s.qualified_name,
                    s.kind.as_str(),
                    s.file_path,
                    s.line_start
                );
            }
        }
    }
    Ok(())
}

fn cmd_feature_for(file: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let features = db.features_for_file(file)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&codesage_protocol::FeatureListResults {
                results: features,
            })?
        );
    } else if features.is_empty() {
        println!("No mapped feature owns or contexts `{file}`.");
    } else {
        println!("Features for {file}:");
        for f in &features {
            println!("  {} {} {}", f.feature_id, f.kind.as_str(), f.title);
        }
    }
    Ok(())
}

fn cmd_trust_boundaries(file: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let tags = db.trust_boundaries_for_file_path(file)?;
    if json {
        let names: Vec<&str> = tags.iter().map(|b| b.as_str()).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "file": file,
                "trust_boundaries": names,
            }))?
        );
    } else if tags.is_empty() {
        println!(
            "Trust boundaries: {file} -> (none; file may not be indexed or has no recognized boundary signal)"
        );
    } else {
        println!("Trust boundaries: {file}");
        for t in &tags {
            println!("  - {}", t.as_str());
        }
    }
    Ok(())
}

fn cmd_risk_batch(files: Vec<String>, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let files = resolve_file_list(files)?;
    if files.is_empty() {
        bail!("no file paths provided (pass as args or pipe via stdin)");
    }
    let assessment = codesage_graph::assess_risk_batch(&db, &files)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&assessment)?);
    } else {
        println!("Per-file risk: {} file(s)", assessment.files.len());
        for f in &assessment.files {
            println!("  {:>5.2}  {}", f.score, f.file);
        }
        if !assessment.legend.is_empty() {
            println!("  Legend:");
            for (code, full) in &assessment.legend {
                println!("    {code} = {full}");
            }
        }
    }
    Ok(())
}

fn cmd_tests_for(files: Vec<String>, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let files = resolve_file_list(files)?;
    if files.is_empty() {
        bail!("no file paths provided (pass as args or pipe via stdin)");
    }
    let recs = codesage_graph::recommend_tests(&db, &files)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&recs)?);
    } else {
        if !recs.primary.is_empty() {
            println!("Primary tests (sibling convention):");
            for f in &recs.primary {
                println!("  {f}");
            }
        }
        if !recs.coupled.is_empty() {
            println!("Coupled tests (co-change history):");
            for c in &recs.coupled {
                println!(
                    "  {:>5.2}  {:>4}x  {}  (couples with {})",
                    c.weight, c.count, c.file, c.source
                );
            }
        }
        if recs.primary.is_empty() && recs.coupled.is_empty() {
            println!("No test files found for the given paths.");
        }
        if !recs.notes.is_empty() {
            for n in &recs.notes {
                println!("# {n}");
            }
        }
    }
    Ok(())
}

fn cmd_session_start(session_id: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let snap = codesage_graph::session_start(&root, &db, session_id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snap)?);
    } else {
        let snapshot_path = root
            .join(PROJECT_DIR)
            .join("sessions")
            .join(format!("{session_id}.json"));
        println!("Session baseline saved: {}", snapshot_path.display());
        println!(
            "  files={}  symbols={}  cycles={}  top_risk_files={}",
            snap.file_count,
            snap.symbol_count,
            snap.cycles.len(),
            snap.top_risk_files.len()
        );
        if let Some(head) = &snap.git_head {
            println!("  git HEAD: {head}");
        }
    }
    Ok(())
}

fn resolve_watch_root(project: Option<PathBuf>) -> Result<PathBuf> {
    match project {
        Some(p) => std::fs::canonicalize(&p)
            .with_context(|| format!("resolving project path {}", p.display())),
        None => find_project_root(),
    }
}

fn cmd_watch_run(project: Option<PathBuf>, debounce_ms: Option<u64>) -> Result<()> {
    let root = resolve_watch_root(project)?;
    let config = load_project_config(&root)?;
    let excludes = get_exclude_patterns(&config);
    let emb_config = config.embedding.clone().unwrap_or_default();
    let debounce = debounce_ms.unwrap_or_else(statewatcher::resolve_debounce_ms);

    // An explicit foreground run overrides a prior `watch stop`.
    let _ = std::fs::remove_file(statewatcher::watch_disabled_path(&root));

    let shutdown = statewatcher::register_shutdown_flag();

    let embedder: Option<statewatcher::EmbedderProvider> = if emb_config.model.is_empty() {
        None
    } else {
        let cfg = emb_config.clone();
        Some(std::sync::Arc::new(move || {
            Embedder::new(&cfg).map(|e| std::sync::Arc::new(parking_lot::Mutex::new(e)))
        }))
    };

    let watcher_config = statewatcher::StateWatcherConfig {
        project_root: root.clone(),
        db_path: db_path(&root),
        embed_config: emb_config,
        exclude_patterns: excludes,
        debounce_ms: debounce,
        // Foreground watchers run until Ctrl-C; no idle self-exit.
        idle_timeout: std::time::Duration::ZERO,
        mode: statewatcher::WatcherMode::Foreground,
        embedder,
        shutdown,
    };

    eprintln!(
        "Watching {} for file changes (debounce: {}ms). Ctrl-C to stop.",
        root.display(),
        debounce
    );
    statewatcher::run_statewatcher(watcher_config)
}

fn cmd_watch_status(project: Option<PathBuf>, json: bool) -> Result<()> {
    let root = resolve_watch_root(project)?;
    let status = statewatcher::read_status(&root);
    let disabled = statewatcher::watch_disabled_path(&root).exists();

    if json {
        let obj = serde_json::json!({
            "project": root.display().to_string(),
            "active": status.is_some(),
            "disabled": disabled,
            "status": status,
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
    } else {
        match status {
            Some(s) => println!(
                "watcher active for {} (mode: {:?}, pid: {})",
                root.display(),
                s.mode,
                s.pid
            ),
            None => println!(
                "watcher not active for {}{}",
                root.display(),
                if disabled { " (disabled)" } else { "" }
            ),
        }
    }
    Ok(())
}

fn cmd_watch_stop(project: Option<PathBuf>) -> Result<()> {
    let root = resolve_watch_root(project)?;
    // The marker both stops any running watcher (its loop polls for it) and
    // suppresses auto-restart on the next tool call.
    let marker = statewatcher::watch_disabled_path(&root);
    std::fs::write(&marker, "").with_context(|| format!("writing {}", marker.display()))?;
    println!("watcher stopped and disabled for {}", root.display());
    Ok(())
}

fn cmd_watch_start(project: Option<PathBuf>) -> Result<()> {
    let root = resolve_watch_root(project)?;
    let marker = statewatcher::watch_disabled_path(&root);
    if marker.exists() {
        std::fs::remove_file(&marker).with_context(|| format!("removing {}", marker.display()))?;
    }
    println!(
        "watcher enabled for {}; the daemon starts it on the next tool call, \
         or run `codesage watch run` for a foreground instance",
        root.display()
    );
    Ok(())
}

fn cmd_session_end(session_id: &str, json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let diff = codesage_graph::session_end(&root, &db, session_id)?;
    let pass = diff.pass;
    if json {
        println!("{}", serde_json::to_string_pretty(&diff)?);
    } else {
        let verdict = if diff.pass { "PASS" } else { "FAIL" };
        println!(
            "Session {}: {} ({}s)",
            diff.session_id, verdict, diff.duration_seconds
        );
        println!(
            "  files: {} → {}  ({} added, {} removed)",
            diff.file_count_before,
            diff.file_count_after,
            diff.new_files.len(),
            diff.removed_files.len(),
        );
        println!(
            "  symbols: {} → {}",
            diff.symbol_count_before, diff.symbol_count_after
        );
        if !diff.new_cycles.is_empty() {
            println!("  NEW cycles ({}):", diff.new_cycles.len());
            for c in diff.new_cycles.iter().take(5) {
                println!("    - {} files: {}", c.len(), c.join(", "));
            }
            if diff.new_cycles.len() > 5 {
                println!("    (+{} more)", diff.new_cycles.len() - 5);
            }
        }
        if !diff.resolved_cycles.is_empty() {
            println!("  resolved cycles: {}", diff.resolved_cycles.len());
        }
        if !diff.risk_regressions.is_empty() {
            println!(
                "  risk regressions ({}, max delta {:.2}):",
                diff.risk_regressions.len(),
                diff.max_risk_regression
            );
            for r in diff.risk_regressions.iter().take(10) {
                println!(
                    "    {:>5.2} → {:>5.2}  (Δ{:+.2})  {}",
                    r.before, r.after, r.delta, r.file
                );
            }
        }
        if !diff.summary_notes.is_empty() {
            println!("  Notes:");
            for n in &diff.summary_notes {
                println!("    - {n}");
            }
        }
    }
    if !pass {
        flush_stdio();
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_status() -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;

    println!("Project root: {}", root.display());
    println!(
        "Database: {}",
        root.join(PROJECT_DIR).join(DB_FILE).display()
    );
    println!("Files:      {}", db.file_count()?);
    println!("Symbols:    {}", db.symbol_count()?);
    println!("References: {}", db.reference_count()?);
    println!("Chunks:     {}", db.total_chunk_count()?);
    println!("Drift:      {}", drift::check_drift(&root, &db).summary());
    print_semantic_status(&root)?;
    Ok(())
}

fn print_semantic_status(root: &Path) -> Result<()> {
    let config = load_project_config(root)?;
    let model = config.embedding.unwrap_or_default().model;
    let db = open_context_db_for_existing_model(root, &model)?;
    if db.chunk_table_name().is_empty() {
        println!("Semantic:   missing for model {model} (run `codesage index`)");
        return Ok(());
    }

    let Some(freshness) = db.semantic_freshness()? else {
        println!("Semantic:   unavailable for model {model}");
        return Ok(());
    };
    if freshness.is_fresh() {
        println!(
            "Semantic:   fresh for model {model} ({} files)",
            freshness.indexed_files
        );
    } else {
        println!(
            "Semantic:   stale for model {model} ({} stale, {} missing; run `codesage index`)",
            freshness.stale_files, freshness.missing_files
        );
    }
    Ok(())
}

fn cmd_cleanup(dry_run: bool) -> Result<()> {
    let root = find_project_root()?;
    // Cleanup drops orphan vec tables (from prior model switches) — also
    // a writer-style operation that races with in-flight indexers. Same
    // lock coordination.
    let Some(_lock) = acquire_index_lock(&root, "skipping cleanup")? else {
        return Ok(());
    };
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
    Ok(())
}

fn cmd_impact(
    target: &str,
    is_file: bool,
    is_symbol: bool,
    depth: usize,
    source_only: bool,
    json: bool,
) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;

    // Pass Some(true) only when the user explicitly set --file; an unset false
    // would force Symbol classification and break the heuristic fallback.
    let hint = if is_file {
        Some(true)
    } else if is_symbol {
        Some(false)
    } else {
        None
    };
    let req = ImpactRequest {
        target: ImpactTarget::from_hint(target.to_string(), hint),
        depth,
        source_only,
    };

    let entries = impact_analysis(&db, &req)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    if entries.is_empty() {
        println!("No impact detected for '{target}'.");
        return Ok(());
    }

    println!(
        "Impact of '{}' (depth={}, {} files affected):",
        target,
        depth,
        entries.len()
    );
    for e in &entries {
        let cat = match e.category {
            FileCategory::Source => "src",
            FileCategory::Test => "test",
            FileCategory::Config => "cfg",
        };
        println!(
            "  [{cat}] d={} {} ({} refs)",
            e.distance,
            e.file_path,
            e.reasons.len()
        );
        for r in e.reasons.iter().take(3) {
            println!("    via {} @ line {} ({})", r.via_symbol, r.line, r.kind);
        }
    }
    Ok(())
}

fn cmd_overview(json: bool) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;
    let overview = crate::overview::build_project_overview(&root, &db)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&overview)?);
        return Ok(());
    }

    println!("Project: {}", overview.project_root);
    println!(
        "Index: {} files, {} symbols | {}",
        overview.file_count, overview.symbol_count, overview.freshness.structural_summary
    );
    if overview.freshness.semantic_indexed {
        println!(
            "Semantic: {} files with chunks",
            overview.freshness.semantic_indexed_files
        );
    } else {
        println!("Semantic: not indexed");
    }

    if !overview.languages.is_empty() {
        let langs: Vec<String> = overview
            .languages
            .iter()
            .map(|l| format!("{} ({})", l.language.as_str(), l.file_count))
            .collect();
        println!("Languages: {}", langs.join(", "));
    }

    if overview.feature_count > 0 {
        let kinds: Vec<String> = overview
            .feature_summary
            .iter()
            .map(|k| format!("{} {}", k.count, k.kind.as_str()))
            .collect();
        println!(
            "Features: {} total ({})",
            overview.feature_count,
            kinds.join(", ")
        );
    }

    if !overview.top_risk_files.is_empty() {
        println!("Top risk:");
        for r in &overview.top_risk_files {
            println!("  {:.3}  {}", r.score, r.file);
        }
    }

    if !overview.trust_boundary_clusters.is_empty() {
        let tb: Vec<String> = overview
            .trust_boundary_clusters
            .iter()
            .map(|c| format!("{} ({})", c.boundary.as_str(), c.file_count))
            .collect();
        println!("Trust boundaries: {}", tb.join(", "));
    }

    if !overview.entrypoints.is_empty() {
        println!("Entrypoints (sample):");
        for e in &overview.entrypoints {
            println!("  [{}] {} — {}", e.kind.as_str(), e.title, e.entry_path);
        }
    }

    if !overview.suggested_next_calls.is_empty() {
        println!("Suggested next calls:");
        for c in &overview.suggested_next_calls {
            println!("  {} → {}  ({})", c.intent, c.tool, c.why);
        }
    }

    Ok(())
}

fn cmd_export(
    target: &str,
    is_symbol: bool,
    limit: usize,
    callers: bool,
    callees: bool,
    format: &str,
) -> Result<()> {
    let root = find_project_root()?;
    let req = ExportRequest::from_target(target.to_string(), is_symbol, limit, callers, callees);

    let bundle = if is_symbol {
        let db = load_symbol_context_db(&root)?;
        export_context_for_symbol(&db, target, &req)?
    } else {
        let (db, mut embedder, mut reranker) = load_query_stack(&root)?;
        let query_embedding = embedder.embed_one(req.query.as_deref().unwrap_or_default())?;
        let rerank_fn: Option<codesage_graph::RerankFn<'_>> = reranker.as_mut().map(|r| {
            Box::new(move |q: &str, docs: &[&str]| r.score_pairs(q, docs))
                as Box<dyn FnMut(&str, &[&str]) -> Result<Vec<f32>>>
        });
        export_context(&db, &query_embedding, rerank_fn, &req)?
    };

    match format {
        "json" => println!("{}", serde_json::to_string_pretty(&bundle)?),
        "ingest" => print_bundle_ingest(&bundle, target, is_symbol),
        _ => print_bundle_markdown(&bundle),
    }
    Ok(())
}

/// Flat-text envelope inspired by gitingest: one self-contained artifact agents can paste
/// into another LLM session without re-templating. Token count is a chars/4 approximation.
fn print_bundle_ingest(bundle: &ContextBundle, target: &str, is_symbol: bool) {
    let target_label = if is_symbol {
        format!("symbol={target}")
    } else {
        format!("query=\"{target}\"")
    };

    let mut all_results: Vec<&codesage_protocol::SearchResult> = bundle.primary.iter().collect();
    all_results.extend(bundle.related.iter());
    let total_chars: usize = all_results.iter().map(|r| r.content.len()).sum();
    let approx_tokens = total_chars / 4;

    let unique_files: Vec<&String> = {
        let mut seen = std::collections::BTreeSet::new();
        let mut order = Vec::new();
        for r in &all_results {
            if seen.insert(r.file_path.as_str()) {
                order.push(&r.file_path);
            }
        }
        order
    };

    println!("=== CodeSage context bundle ===");
    println!("Target: {target_label}");
    println!("Description: {}", bundle.target_description);
    println!(
        "Counts: {} chunks across {} files ({} primary, {} related)",
        all_results.len(),
        unique_files.len(),
        bundle.primary.len(),
        bundle.related.len()
    );
    println!(
        "Approx tokens: ~{} (chars/4 estimate; replace with real tokenizer for billing)",
        approx_tokens
    );
    if !bundle.symbol_definitions.is_empty() {
        println!("Symbol definitions: {}", bundle.symbol_definitions.len());
    }
    println!();

    println!("=== File tree ===");
    for line in render_file_tree(&unique_files) {
        println!("{line}");
    }
    println!();

    if !bundle.symbol_definitions.is_empty() {
        println!("=== Symbol definitions ===");
        for s in &bundle.symbol_definitions {
            println!(
                "- {} ({}): {}:{} qualified={}",
                s.name,
                s.kind.as_str(),
                s.file_path,
                s.line_start,
                s.qualified_name
            );
        }
        println!();
    }

    println!("=== Files ===");
    println!();
    for r in &all_results {
        let symbols = if r.symbols.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = r
                .symbols
                .iter()
                .map(|s| format!("{}({})", s.name, s.kind))
                .collect();
            format!(" symbols=[{}]", names.join(", "))
        };
        println!(
            "=== {}:{}-{} lang={}{} ===",
            r.file_path, r.start_line, r.end_line, r.language, symbols
        );
        println!("{}", r.content.trim_end());
        println!();
    }
}

/// Render a list of file paths as an ASCII tree. Files appear in sorted order under each dir.
fn render_file_tree(paths: &[&String]) -> Vec<String> {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Node {
        children: BTreeMap<String, Node>,
        is_file: bool,
    }

    let mut root = Node::default();
    for p in paths {
        let mut cur = &mut root;
        let parts: Vec<&str> = p.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            cur = cur.children.entry(part.to_string()).or_default();
            if i == parts.len() - 1 {
                cur.is_file = true;
            }
        }
    }

    let mut out = Vec::new();
    fn walk(node: &Node, prefix: &str, out: &mut Vec<String>) {
        let entries: Vec<(&String, &Node)> = node.children.iter().collect();
        let n = entries.len();
        for (i, (name, child)) in entries.iter().enumerate() {
            let last = i == n - 1;
            let connector = if last { "└── " } else { "├── " };
            let label = if child.is_file && child.children.is_empty() {
                name.to_string()
            } else {
                format!("{name}/")
            };
            out.push(format!("{prefix}{connector}{label}"));
            let next_prefix = format!("{prefix}{}", if last { "    " } else { "│   " });
            walk(child, &next_prefix, out);
        }
    }
    walk(&root, "", &mut out);
    out
}

fn print_bundle_markdown(bundle: &ContextBundle) {
    println!("# Context: {}", bundle.target_description);
    println!();

    if !bundle.primary.is_empty() {
        println!("## Primary matches ({})\n", bundle.primary.len());
        for r in &bundle.primary {
            print_result_block(r);
        }
    }

    if !bundle.related.is_empty() {
        println!("## Related code ({})\n", bundle.related.len());
        for r in &bundle.related {
            print_result_block(r);
        }
    }

    if !bundle.symbol_definitions.is_empty() {
        println!(
            "## Symbol definitions ({})\n",
            bundle.symbol_definitions.len()
        );
        for s in &bundle.symbol_definitions {
            println!(
                "- **{}** ({}) — `{}:{}` ({})",
                s.name,
                s.kind.as_str(),
                s.file_path,
                s.line_start,
                s.qualified_name
            );
        }
        println!();
    }
}

fn print_result_block(r: &codesage_protocol::SearchResult) {
    println!(
        "### `{}:{}-{}` ({})",
        r.file_path, r.start_line, r.end_line, r.language
    );
    if !r.symbols.is_empty() {
        let syms: Vec<String> = r
            .symbols
            .iter()
            .map(|s| format!("{} ({})", s.name, s.kind))
            .collect();
        println!("**Symbols:** {}", syms.join(", "));
    }
    println!();
    println!("```{}", r.language);
    println!("{}", r.content);
    println!("```");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_embed::config::IndexConfig;

    fn paths_owned(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    fn paths_refs(owned: &[String]) -> Vec<&String> {
        owned.iter().collect()
    }

    #[test]
    fn render_file_tree_empty() {
        let out = render_file_tree(&[]);
        assert!(out.is_empty());
    }

    // ---------- post-commit hook body contract ----------

    #[test]
    fn post_commit_hook_runs_index_and_git_index_sequentially() {
        // Regression for the validated finding "hook race between
        // index and git-index": prior body launched both subcommands
        // with `&` in parallel; whichever lost the project-lock race
        // skipped silently because the hook redirects to /dev/null.
        // The body must chain them with `&&` inside ONE background
        // subshell so both passes always run after every commit.
        let body = generate_post_commit_hook_body("/usr/local/bin/codesage");
        assert!(
            body.contains("'/usr/local/bin/codesage' index &&"),
            "expected `index` to be chained with `&&`, got:\n{body}"
        );
        assert!(
            body.contains("'/usr/local/bin/codesage' git-index --incremental"),
            "expected `git-index --incremental` invocation, got:\n{body}"
        );
        // Only one background `&` should appear at the top level —
        // we sequence inside one subshell, then background the whole
        // group. Two `&` (one per command) would re-introduce the race.
        let backgrounded_lines: Vec<&str> = body
            .lines()
            .filter(|l| l.trim_end().ends_with(" &"))
            .collect();
        assert_eq!(
            backgrounded_lines.len(),
            1,
            "expected exactly one background `&` line, got {} lines:\n{}",
            backgrounded_lines.len(),
            backgrounded_lines.join("\n")
        );
    }

    #[test]
    fn post_commit_hook_shell_quotes_codesage_path() {
        let body = generate_post_commit_hook_body("/tmp/cod\"e$age`bin's/codesage");

        assert!(
            body.contains("'/tmp/cod\"e$age`bin'\"'\"'s/codesage' index &&"),
            "expected shell-quoted binary path, got:\n{body}"
        );
        assert!(
            !body.contains("\"/tmp/cod\"e$age`bin's/codesage\""),
            "double-quoted binary path would still allow shell expansion:\n{body}"
        );
    }

    #[test]
    fn shell_single_quote_escapes_single_quotes() {
        assert_eq!(
            shell_single_quote("/tmp/a'b/codesage"),
            "'/tmp/a'\"'\"'b/codesage'"
        );
        assert_eq!(shell_single_quote("/tmp/codesage"), "'/tmp/codesage'");
    }

    #[test]
    fn post_commit_hook_skips_when_no_codesage_dir() {
        let body = generate_post_commit_hook_body("/x");
        assert!(
            body.contains("[ -d \"$root/.codesage\" ]"),
            "expected guard on .codesage directory presence"
        );
    }

    #[test]
    fn render_file_tree_single_file() {
        let owned = paths_owned(&["foo.rs"]);
        let out = render_file_tree(&paths_refs(&owned));
        assert_eq!(out, vec!["└── foo.rs"]);
    }

    #[test]
    fn render_file_tree_nested() {
        let owned = paths_owned(&[
            "src/auth/login.php",
            "src/auth/session.php",
            "src/handlers/webhook.php",
        ]);
        let out = render_file_tree(&paths_refs(&owned));
        assert_eq!(
            out,
            vec![
                "└── src/",
                "    ├── auth/",
                "    │   ├── login.php",
                "    │   └── session.php",
                "    └── handlers/",
                "        └── webhook.php",
            ]
        );
    }

    #[test]
    fn render_file_tree_multiple_top_level() {
        let owned = paths_owned(&["a.rs", "b.rs", "c.rs"]);
        let out = render_file_tree(&paths_refs(&owned));
        assert_eq!(out, vec!["├── a.rs", "├── b.rs", "└── c.rs"]);
    }

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

    #[test]
    fn toml_basic_string_escapes_project_names() {
        assert_eq!(toml_basic_string("plain"), "\"plain\"");
        assert_eq!(
            toml_basic_string("quote\"and\\slash"),
            "\"quote\\\"and\\\\slash\""
        );
        assert_eq!(toml_basic_string("line\nfeed"), "\"line\\nfeed\"");
        assert_eq!(toml_basic_string("del\u{7f}"), "\"del\\u007F\"");
    }

    #[cfg(not(feature = "cuda"))]
    #[test]
    fn index_embedder_setup_errors_when_gpu_requested_without_cuda() {
        let cfg = EmbeddingConfig {
            model: "codesage-test/does-not-matter".to_string(),
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
}
