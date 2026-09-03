//! Runtime and integration commands: `mcp`, `daemon`, `watch`, `install`,
//! `uninstall`, `init`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use codesage_embed::model::Embedder;

use crate::{
    PROJECT_DIR, daemon, db_path, find_project_root, find_project_root_opt, get_exclude_patterns,
    installer, load_project_config, mcp, statewatcher,
};

pub(crate) fn cmd_mcp(
    direct: bool,
    runtime_dir: Option<PathBuf>,
    project: Option<PathBuf>,
) -> Result<()> {
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

pub(crate) fn cmd_daemon(runtime_dir: Option<PathBuf>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(daemon::run_daemon(runtime_dir))
}

pub(crate) fn cmd_daemon_status(runtime_dir: Option<PathBuf>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(daemon::run_daemon_status(runtime_dir))
}

pub(crate) fn cmd_daemon_stop(runtime_dir: Option<PathBuf>) -> Result<()> {
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

/// Project root for install/uninstall. Project-local mode requires an
/// onboarded project; global mode resolves opportunistically so `install
/// --global` (and global `uninstall`) work outside onboarded projects —
/// the registration then carries no `--project` default and the server
/// resolves the project per call instead.
fn resolve_install_project(global: bool) -> Result<(Option<PathBuf>, Option<String>)> {
    if !global {
        return canonical_project_utf8().map(|(p, s)| (Some(p), Some(s)));
    }
    let Some(root) = find_project_root_opt() else {
        return Ok((None, None));
    };
    let canon = std::fs::canonicalize(&root).unwrap_or(root);
    let utf8 = canon
        .to_str()
        .with_context(|| format!("project path is not valid UTF-8: {}", canon.display()))?
        .to_owned();
    Ok((Some(canon), Some(utf8)))
}

pub(crate) fn cmd_install(target: &str, global: bool) -> Result<()> {
    let (project, project_utf8) = resolve_install_project(global)?;
    let home = home_dir()?;
    let targets = resolve_install_targets(target)?;
    let ctx = installer::InstallCtx {
        home: &home,
        project: project.as_deref(),
        project_utf8: project_utf8.as_deref(),
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
    match &project {
        Some(p) => println!(
            "\nCodeSage MCP server registered for project: {}",
            p.display()
        ),
        None => println!(
            "\nCodeSage MCP server registered globally with no project default \
             (outside any onboarded project); pass a project per call or run inside one"
        ),
    }
    Ok(())
}

pub(crate) fn cmd_uninstall(target: &str, global: bool) -> Result<()> {
    let (project, project_utf8) = resolve_install_project(global)?;
    let home = home_dir()?;
    let targets = resolve_install_targets(target)?;
    let ctx = installer::InstallCtx {
        home: &home,
        project: project.as_deref(),
        project_utf8: project_utf8.as_deref(),
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

/// Directories under `$HOME` that hold credentials or key material. Indexing
/// one would embed secrets into a same-UID-readable index, so `init` refuses
/// them without `--force`.
const SENSITIVE_HOME_DIRS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".docker",
    ".password-store",
];

/// Why a directory is refused as an indexing root, or `None` if acceptable.
///
/// Both sides are canonicalized so a symlinked `$HOME` or credential dir
/// doesn't dodge a lexical comparison. Foot-gun protection, not a security
/// boundary — `--force` bypasses it by design.
fn init_root_refusal(cwd: &std::path::Path) -> Option<String> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if cwd.parent().is_none() {
        return Some("the filesystem root".to_string());
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(std::path::PathBuf::from)?;
    let home = home.canonicalize().unwrap_or(home);
    if cwd == home {
        return Some("your home directory".to_string());
    }
    for dir in SENSITIVE_HOME_DIRS {
        let sensitive = home.join(dir);
        let sensitive = sensitive.canonicalize().unwrap_or(sensitive);
        if cwd.starts_with(&sensitive) {
            return Some(format!("~/{dir}, which holds credentials"));
        }
    }
    None
}

pub(crate) fn cmd_init(force: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;

    if let Some(reason) = init_root_refusal(&cwd) {
        if force {
            eprintln!("warning: initializing in {reason} (--force)");
        } else {
            bail!(
                "refusing to initialize in {reason}: indexing here would embed \
                 everything readable below it into a queryable index. Run from \
                 the project root instead, or pass --force to override."
            );
        }
    }

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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(&project_dir, std::fs::Permissions::from_mode(0o700))?;
    }

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

    // Keep the index, session/review state, and other generated artifacts out
    // of version control. An in-tree .gitignore is shared across the team —
    // unlike a per-clone .git/info/exclude — so teammates who never run
    // `codesage init` (e.g. onboarded via the plugin) still don't commit
    // session snapshots or the index db. `*` + `!.gitignore` ignores the whole
    // directory but lets this file itself be committed.
    std::fs::write(
        project_dir.join(".gitignore"),
        "# CodeSage local index and session state — not for version control.\n*\n!.gitignore\n",
    )?;

    println!("Initialized CodeSage in {}", cwd.display());
    Ok(())
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

fn resolve_watch_root(project: Option<PathBuf>) -> Result<PathBuf> {
    match project {
        Some(p) => std::fs::canonicalize(&p)
            .with_context(|| format!("resolving project path {}", p.display())),
        None => find_project_root(),
    }
}

pub(crate) fn cmd_watch_run(project: Option<PathBuf>, debounce_ms: Option<u64>) -> Result<()> {
    let root = resolve_watch_root(project)?;
    let config = load_project_config(&root)?;
    let excludes = get_exclude_patterns(&config);
    let emb_config = config.embedding.clone().unwrap_or_default();
    // The env path (`resolve_debounce_ms`) already warns through tracing when
    // it clamps; an explicit flag previously skipped the floor entirely, so
    // a zero `--reindex-debounce` re-indexed hot. Floor it here and say so
    // on stderr, where the operator will actually see it.
    let debounce = match debounce_ms {
        Some(ms) => {
            let floored = statewatcher::floor_debounce_ms(ms);
            if floored != ms {
                eprintln!(
                    "warning: --reindex-debounce {ms}ms is below the {}ms floor; using {floored}ms",
                    statewatcher::MIN_DEBOUNCE_MS,
                );
            }
            floored
        }
        None => statewatcher::resolve_debounce_ms(),
    };

    // An explicit foreground run overrides a prior `watch stop`. A clone can
    // ship `watch.disabled` as a directory, which no removal clears: surface
    // that instead of announcing startup and then exiting.
    clear_watch_disabled_marker(&root)?;

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
        backpressure: true,
    };

    eprintln!(
        "Watching {} for file changes (debounce: {}ms). Ctrl-C to stop.",
        root.display(),
        debounce
    );
    statewatcher::run_statewatcher(watcher_config)
}

/// Clear a `watch.disabled` marker, tolerating only its absence.
///
/// Remove unconditionally rather than gating on `exists()`, which follows
/// links: a dangling marker symlink reads as absent there, so it would survive
/// `watch start` and then block every later `watch stop`.
fn clear_watch_disabled_marker(root: &std::path::Path) -> Result<()> {
    let marker = statewatcher::watch_disabled_path(root);
    match crate::fsguard::remove_state_file(&marker) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(anyhow::Error::from(e)).with_context(|| format!("removing {}", marker.display()))
        }
    }
}

pub(crate) fn cmd_watch_status(project: Option<PathBuf>, json: bool) -> Result<()> {
    let root = resolve_watch_root(project)?;
    let status = statewatcher::read_status(&root);
    // lstat, not `exists()`: a dangling marker symlink must read the same way
    // here as it does to `watch_enabled()`.
    let disabled = statewatcher::watch_disabled_marker_present(&root);

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
            Some(s) => {
                // `stale_parked` is already in the JSON object above; the
                // human line must not hide it — parked paths are stale, and
                // a status that reads "active" with no hint would lie about
                // their visibility.
                let parked_note = match s.stale_parked {
                    0 => String::new(),
                    1 => ", 1 path parked after repeated failures".to_string(),
                    n => format!(", {n} paths parked after repeated failures"),
                };
                println!(
                    "watcher active for {} (mode: {:?}, pid: {}{parked_note})",
                    root.display(),
                    s.mode,
                    s.pid
                );
            }
            None => println!(
                "watcher not active for {}{}",
                root.display(),
                if disabled { " (disabled)" } else { "" }
            ),
        }
    }
    Ok(())
}

pub(crate) fn cmd_watch_stop(project: Option<PathBuf>) -> Result<()> {
    let root = resolve_watch_root(project)?;
    // The marker both stops any running watcher (its loop polls for it) and
    // suppresses auto-restart on the next tool call.
    let marker = statewatcher::watch_disabled_path(&root);
    // O_NOFOLLOW, not `fs::write`: a repo-planted `watch.disabled` symlink would
    // otherwise let this O_CREAT|O_TRUNC truncate an arbitrary host file.
    crate::fsguard::create_no_follow(&marker)
        .with_context(|| format!("writing {}", marker.display()))?;
    println!("watcher stopped and disabled for {}", root.display());
    Ok(())
}

pub(crate) fn cmd_watch_start(project: Option<PathBuf>) -> Result<()> {
    let root = resolve_watch_root(project)?;
    clear_watch_disabled_marker(&root)?;
    println!(
        "watcher enabled for {}; the daemon starts it on the next tool call, \
         or run `codesage watch run` for a foreground instance",
        root.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn init_refuses_dangerous_roots() {
        assert!(init_root_refusal(std::path::Path::new("/")).is_some());
        if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
            assert!(init_root_refusal(&home).is_some());
            assert!(init_root_refusal(&home.join(".ssh")).is_some());
            assert!(init_root_refusal(&home.join(".aws/config-dir")).is_some());
            assert!(init_root_refusal(&home.join("projects/app")).is_none());
            // A directory that merely shares a name prefix with a sensitive
            // dir must not be refused (.sshfs is not .ssh).
            assert!(init_root_refusal(&home.join(".sshfs")).is_none());
        }
        assert!(init_root_refusal(std::path::Path::new("/tmp/some/project")).is_none());
    }
}
