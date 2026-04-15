mod doctor;
mod mcp;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use codesage_embed::config::ProjectConfig;
use codesage_embed::model::Embedder;
use codesage_graph::{
    assess_risk, export_context, find_coupling, find_references, find_symbol, full_index,
    impact_analysis, incremental_index, list_dependencies, search, semantic_full_index,
    semantic_incremental_index,
};
use codesage_parser::discover::DEFAULT_EXCLUDE_PATTERNS;
use codesage_protocol::{
    ContextBundle, ExportRequest, FileCategory, FindReferencesRequest, FindSymbolRequest,
    ImpactRequest, ImpactTarget, Language, ReferenceKind, SearchRequest, SymbolKind,
};
use codesage_storage::Database;

pub(crate) const PROJECT_DIR: &str = ".codesage";
pub(crate) const DB_FILE: &str = "index.db";

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
    /// Index the project (incremental by default)
    Index {
        /// Force a full reindex
        #[arg(long)]
        full: bool,
        /// Skip semantic indexing (embeddings)
        #[arg(long)]
        no_semantic: bool,
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
        #[arg(long)]
        file: bool,
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
    Mcp,
    /// Install git hooks for automatic reindexing
    InstallHooks,
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
    /// Tests that should run after editing the given files: sibling tests + co-change history.
    /// Reads file paths from stdin (one per line) or from positional args. (V2b slice 2)
    TestsFor {
        /// Repo-relative file paths. If empty, read newline-separated paths from stdin.
        files: Vec<String>,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
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

fn open_db(root: &Path) -> Result<Database> {
    let db_path = root.join(PROJECT_DIR).join(DB_FILE);
    Database::open(&db_path).context("failed to open index database")
}

fn open_db_for_model(root: &Path, model: &str, dim: usize) -> Result<Database> {
    let db_path = root.join(PROJECT_DIR).join(DB_FILE);
    Database::open_for_model(&db_path, model, dim).context("failed to open index database")
}

pub(crate) fn load_project_config(root: &Path) -> ProjectConfig {
    let config_path = root.join(PROJECT_DIR).join("config.toml");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return ProjectConfig::default(),
    };
    toml::from_str(&content).unwrap_or_default()
}

fn get_exclude_patterns(config: &ProjectConfig) -> Vec<String> {
    let mut patterns: Vec<String> = DEFAULT_EXCLUDE_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(user) = config
        .index
        .as_ref()
        .and_then(|i| i.exclude_patterns.clone())
    {
        patterns.extend(user);
    }
    patterns
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
    let config = load_project_config(root);
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

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init => cmd_init(),
        Commands::Index { full, no_semantic } => cmd_index(full, no_semantic),
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
            depth,
            source_only,
            json,
        } => cmd_impact(&target, file, depth, source_only, json),
        Commands::Export {
            target,
            symbol,
            limit,
            callers,
            callees,
            format,
        } => cmd_export(&target, symbol, limit, callers, callees, &format),
        Commands::Status => cmd_status(),
        Commands::Mcp => cmd_mcp(),
        Commands::InstallHooks => cmd_install_hooks(),
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
        Commands::TestsFor { files, json } => cmd_tests_for(files, json),
    }
}

fn cmd_mcp() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(mcp::run_mcp_server())
}

fn cmd_install_hooks() -> Result<()> {
    let root = find_project_root()?;
    if !root.join(".git").exists() {
        bail!("not a git repository (no .git at project root)");
    }

    let (hooks_dir, is_husky) = resolve_hooks_dir(&root)?;
    std::fs::create_dir_all(&hooks_dir)?;

    let codesage_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("codesage"));
    let codesage_path = codesage_bin.display();

    let hook_body = format!(
        "#!/bin/sh\n\
         # installed by codesage install-hooks\n\
         root=\"$(git rev-parse --show-toplevel 2>/dev/null)\" || exit 0\n\
         [ -d \"$root/.codesage\" ] && ( cd \"$root\" && \"{bin}\" index ) >/dev/null 2>&1 &\n\
         [ -d \"$root/.codesage\" ] && ( cd \"$root\" && \"{bin}\" git-index --incremental ) >/dev/null 2>&1 &\n\
         exit 0\n",
        bin = codesage_path,
    );

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

fn git_common_dir(cwd: &std::path::Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("--git-common-dir")
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8(out.stdout).ok()?;
    let dir = dir.trim();
    if dir.is_empty() {
        return None;
    }
    let path = std::path::Path::new(dir);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

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

    if project_dir.exists() {
        println!("Already initialized in {}", cwd.display());
        return Ok(());
    }

    std::fs::create_dir_all(&project_dir)?;

    let dir_name = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());

    std::fs::write(
        project_dir.join("config.toml"),
        format!(
            "[project]\nname = \"{dir_name}\"\n\n\
             [embedding]\nmodel = \"sentence-transformers/all-MiniLM-L6-v2\"\ndevice = \"cpu\"\nreranker = \"cross-encoder/ms-marco-MiniLM-L6-v2\"\n\n\
             [index]\n\
             # Built-in defaults always apply (tests, vendored deps, build outputs, lock files,\n\
             # caches, IDE state). See DEFAULT_EXCLUDE_PATTERNS in crates/parser/src/discover.rs.\n\
             # Patterns listed here ADD to the defaults; they do not replace them.\n\
             exclude_patterns = []\n",
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

fn cmd_index(full: bool, no_semantic: bool) -> Result<()> {
    let root = find_project_root()?;
    let config = load_project_config(&root);
    let excludes = get_exclude_patterns(&config);

    let emb_config = config.embedding.unwrap_or_default();
    let embedder_result = if !no_semantic {
        Some(Embedder::new(&emb_config))
    } else {
        None
    };

    let (model_name, dim) = match &embedder_result {
        Some(Ok(e)) => (emb_config.model.as_str(), e.dim()),
        _ => ("default", codesage_storage::db::DEFAULT_EMBEDDING_DIM),
    };
    let db = open_db_for_model(&root, model_name, dim)?;

    let stats = if full {
        full_index(&root, &db, &excludes)?
    } else {
        incremental_index(&root, &db, &excludes)?
    };

    println!(
        "Structural: {} files ({} skipped, {} removed), {} symbols, {} references",
        stats.files_indexed,
        stats.files_skipped,
        stats.files_removed,
        stats.symbols_found,
        stats.references_found
    );

    if let Some(embedder_result) = embedder_result {
        match embedder_result {
            Ok(mut embedder) => {
                let sem_stats = if full {
                    semantic_full_index(&root, &db, &mut embedder, &excludes)?
                } else {
                    semantic_incremental_index(&root, &db, &mut embedder, &excludes)?
                };
                println!(
                    "Semantic: {} files ({} skipped, {} removed), {} chunks",
                    sem_stats.files_processed,
                    sem_stats.files_skipped,
                    sem_stats.files_removed,
                    sem_stats.chunks_created
                );
            }
            Err(e) => {
                eprintln!("Semantic indexing skipped: {e}");
            }
        }
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

    let results = search(&db, &mut embedder, reranker.as_mut(), &req)?;

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
    let db = open_db(&root)?;
    let mode = if full {
        codesage_graph::IndexMode::Full
    } else if incremental {
        codesage_graph::IndexMode::Incremental
    } else {
        codesage_graph::IndexMode::Auto
    };
    let stats = codesage_graph::git_history_index_with_options(&db, &root, &[], mode)?;
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
    let entries = find_coupling(&db, file, limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else if entries.is_empty() {
        println!(
            "No co-change history for {file}. Run `codesage git-index` if you haven't, or check the path."
        );
    } else {
        println!("Files that historically change with {file}:");
        for e in &entries {
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
    println!("Chunks:     {}", db.chunk_count()?);
    Ok(())
}

fn cmd_cleanup(dry_run: bool) -> Result<()> {
    let root = find_project_root()?;
    let config = load_project_config(&root);
    let emb_config = config.embedding.unwrap_or_default();

    let embedder = Embedder::new(&emb_config)?;
    let active_dim = embedder.dim();
    let active_table = codesage_storage::schema::model_table_name(&emb_config.model, active_dim);

    let db = open_db_for_model(&root, &emb_config.model, active_dim)?;

    let db_path = root.join(PROJECT_DIR).join(DB_FILE);
    let size_before = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    let tables = db.list_vec_tables()?;
    let mut dropped = 0;

    println!("Active model:  {}", emb_config.model);
    println!("Active table:  {active_table}");
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
                    eprintln!("  FAIL drop {table}: {e}");
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

fn format_bytes(n: u64) -> String {
    if n >= 1 << 30 {
        format!("{:.2} GiB", n as f64 / (1u64 << 30) as f64)
    } else if n >= 1 << 20 {
        format!("{:.2} MiB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.2} KiB", n as f64 / (1u64 << 10) as f64)
    } else {
        format!("{n} B")
    }
}

fn cmd_impact(
    target: &str,
    is_file: bool,
    depth: usize,
    source_only: bool,
    json: bool,
) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db(&root)?;

    let looks_like_file = is_file || target.contains('/') || target.contains('.');
    let impact_target = if looks_like_file {
        ImpactTarget::File {
            path: target.to_string(),
        }
    } else {
        ImpactTarget::Symbol {
            name: target.to_string(),
        }
    };

    let req = ImpactRequest {
        target: impact_target,
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

fn cmd_export(
    target: &str,
    is_symbol: bool,
    limit: usize,
    callers: bool,
    callees: bool,
    format: &str,
) -> Result<()> {
    let root = find_project_root()?;
    let (db, mut embedder, mut reranker) = load_query_stack(&root)?;

    let req = ExportRequest {
        query: if is_symbol {
            None
        } else {
            Some(target.to_string())
        },
        symbol: if is_symbol {
            Some(target.to_string())
        } else {
            None
        },
        limit,
        include_callers: callers,
        include_callees: callees,
    };

    let bundle = export_context(&db, &mut embedder, reranker.as_mut(), &req)?;

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
            }),
        };
        let patterns = get_exclude_patterns(&cfg);
        assert_eq!(patterns.len(), DEFAULT_EXCLUDE_PATTERNS.len());
    }
}
