pub(crate) mod docs;

use std::path::{Path, PathBuf};

use anyhow::Result;
use codesage_storage::Database;

use codesage_graph::drift::{DriftKind, check_drift};

use crate::{DB_FILE, PROJECT_DIR, find_project_root_opt, load_project_config};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, serde::Serialize)]
struct Check {
    name: &'static str,
    status: Status,
    message: String,
    /// Carried only by the `hook_health` check.
    #[serde(skip_serializing_if = "Option::is_none")]
    hook_health: Option<codesage_protocol::HookHealth>,
}

impl Check {
    fn new(name: &'static str, status: Status, message: String) -> Self {
        Self {
            name,
            status,
            message,
            hook_health: None,
        }
    }
}

const REQUIRED_HOOKS: &[&str] = &["post-commit", "post-merge", "post-checkout", "post-rewrite"];

pub fn run(json: bool) -> Result<()> {
    let mut checks = Vec::new();

    checks.push(check_binary());
    checks.push(check_queries());

    let project = find_project_root_opt();

    if let Some(root) = &project {
        checks.push(check_config(root));
        checks.push(check_db(root));
        checks.push(check_disk(root));
        checks.push(check_hooks(root));
        checks.push(check_hook_health(root));
        checks.push(check_index_drift(root));
        checks.push(check_semantic_freshness(root));
    } else {
        checks.push(Check::new(
            "project",
            Status::Skip,
            "not in a codesage project (run `codesage init` first)".to_string(),
        ));
    }

    checks.push(check_cuda(project.as_deref()));
    checks.push(check_coreml(project.as_deref()));
    checks.push(check_models(project.as_deref()));
    checks.push(check_mcp());

    if json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        print_text(&checks);
    }

    let any_fail = checks.iter().any(|c| matches!(c.status, Status::Fail));
    if any_fail {
        std::process::exit(1);
    }

    Ok(())
}

fn print_text(checks: &[Check]) {
    for c in checks {
        let prefix = match c.status {
            Status::Pass => "[PASS]",
            Status::Warn => "[WARN]",
            Status::Fail => "[FAIL]",
            Status::Skip => "[SKIP]",
        };
        println!("{prefix} {}: {}", c.name, c.message);
    }
}

fn check_binary() -> Check {
    let version = env!("CARGO_PKG_VERSION");
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let cuda = if cfg!(feature = "cuda") {
        "cuda"
    } else {
        "no-cuda"
    };
    Check::new(
        "binary",
        Status::Pass,
        format!("codesage {version} ({profile}, {cuda})"),
    )
}

/// Detect grammar/query mismatches before indexing would panic.
fn check_queries() -> Check {
    match codesage_parser::validate::validate_all_queries() {
        Ok(()) => Check::new(
            "queries",
            Status::Pass,
            "embedded tree-sitter queries valid against their grammars".to_string(),
        ),
        Err(e) => Check::new("queries", Status::Fail, format!("{e:#}")),
    }
}

fn check_config(root: &Path) -> Check {
    let config_path = root.join(PROJECT_DIR).join("config.toml");
    if !config_path.exists() {
        return Check::new(
            "config",
            Status::Fail,
            format!("missing {}", config_path.display()),
        );
    }
    let config = match load_project_config(root) {
        Ok(c) => c,
        Err(e) => {
            return Check::new("config", Status::Fail, format!("{e:#}"));
        }
    };
    let emb = config.embedding.unwrap_or_default();
    let reranker = emb
        .reranker
        .as_deref()
        .map(|r| format!(" reranker={r}"))
        .unwrap_or_default();
    Check::new(
        "config",
        Status::Pass,
        format!("model={} device={}{reranker}", emb.model, emb.device),
    )
}

fn check_db(root: &Path) -> Check {
    let db_path = root.join(PROJECT_DIR).join(DB_FILE);
    if !db_path.exists() {
        return Check::new(
            "db",
            Status::Warn,
            format!("missing {} (run `codesage index`)", db_path.display()),
        );
    }
    // Inspection must also work on read-only checkouts without chmod or migrations.
    match Database::open_read_only(&db_path) {
        Ok(db) => {
            let f = db.file_count().unwrap_or(0);
            let s = db.symbol_count().unwrap_or(0);
            let r = db.reference_count().unwrap_or(0);
            Check::new(
                "db",
                Status::Pass,
                format!("schema OK; files={f} symbols={s} refs={r}"),
            )
        }
        Err(e) => Check::new("db", Status::Fail, format!("failed to open: {e}")),
    }
}

fn check_disk(root: &Path) -> Check {
    let db_path = root.join(PROJECT_DIR).join(DB_FILE);
    // lstat protects only the leaf; reject a symlinked .codesage parent separately.
    let size = crate::fsguard::reject_symlinked_project_dir(&db_path)
        .ok()
        .and_then(|()| std::fs::symlink_metadata(&db_path).ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .unwrap_or(0);
    Check::new(
        "disk",
        Status::Pass,
        format!("index.db {}", format_bytes(size)),
    )
}

fn check_coreml(project: Option<&Path>) -> Check {
    let want_coreml = project
        .and_then(|root| {
            let config = load_project_config(root).ok()?;
            config
                .embedding
                .map(|e| codesage_embed::config::wants_coreml(&e.device))
        })
        .unwrap_or(false);

    if !want_coreml {
        return Check::new(
            "coreml",
            Status::Pass,
            "config requests non-CoreML device; CoreML not required".to_string(),
        );
    }

    if !cfg!(target_vendor = "apple") {
        return Check::new(
            "coreml",
            Status::Fail,
            "config wants device=coreml but this binary is not running on Apple hardware"
                .to_string(),
        );
    }

    Check::new("coreml", Status::Pass, "device=coreml on Apple hardware; ORT statically linked at build time with CoreML EP; first session may compile CoreML submodels (slow once per process)"
            .to_string())
}

fn check_cuda(project: Option<&Path>) -> Check {
    let want_gpu = project
        .and_then(|root| {
            let config = load_project_config(root).ok()?;
            config
                .embedding
                .map(|e| codesage_embed::config::wants_cuda(&e.device))
        })
        .unwrap_or(false);
    let built_with_cuda = cfg!(feature = "cuda");

    if !want_gpu {
        return Check::new(
            "cuda",
            Status::Pass,
            "config requests CPU; CUDA not required".to_string(),
        );
    }
    if !built_with_cuda {
        return Check::new("cuda", Status::Fail, "config wants device=gpu but binary built WITHOUT cuda feature; rebuild with `cargo build --release --features cuda`".to_string());
    }
    match codesage_embed::nvidia_lib_dirs() {
        dirs if dirs.is_empty() => Check::new(
            "cuda",
            Status::Warn,
            "nvidia libs not found; set CODESAGE_NVIDIA_LIBS, or install the `nvidia-*-cu12` pip \
             packages (cudnn, cublas, cuda-runtime, cufft, curand, cuda-nvrtc). First \
             GPU session will likely fail to register the CUDA provider."
                .to_string(),
        ),
        dirs => Check::new(
            "cuda",
            Status::Pass,
            format!(
                "cuda feature compiled; {} nvidia lib dir(s) discovered",
                dirs.len()
            ),
        ),
    }
}

fn check_models(project: Option<&Path>) -> Check {
    let cache = hf_cache_dir();
    let (embed_model, rerank_model): (String, Option<String>) = project
        .and_then(|root| {
            let c = load_project_config(root).ok()?;
            let e = c.embedding.unwrap_or_default();
            Some((e.model, e.reranker))
        })
        .unwrap_or_else(|| {
            (
                "sentence-transformers/all-MiniLM-L6-v2".to_string(),
                Some("cross-encoder/ms-marco-MiniLM-L6-v2".to_string()),
            )
        });

    // Disallowed models fail before download, so cache-miss advice would be misleading.
    let allow_any = codesage_embed::model::allow_any_model_from_env();
    let mut disallowed = Vec::new();
    if codesage_embed::model::validate_model_allowed(&embed_model, allow_any).is_err() {
        disallowed.push(embed_model.clone());
    }
    if let Some(m) = &rerank_model
        && codesage_embed::model::validate_model_allowed(m, allow_any).is_err()
    {
        disallowed.push(m.clone());
    }
    if !disallowed.is_empty() {
        return Check::new(
            "models",
            Status::Fail,
            format!(
                "{} not on the validated-model allowlist; will ERROR at first load, not download. \
                 Set CODESAGE_ALLOW_ANY_MODEL=1 to run a model you trust.",
                disallowed.join(", ")
            ),
        );
    }

    let embed_present = model_in_cache(&cache, &embed_model);
    let rerank_present = rerank_model
        .as_ref()
        .map(|m| model_in_cache(&cache, m))
        .unwrap_or(true);

    let status = if embed_present && rerank_present {
        Status::Pass
    } else {
        Status::Warn
    };
    let mut parts = vec![format!(
        "{}: {}",
        embed_model,
        if embed_present {
            "cached"
        } else {
            "MISSING (will download on first use)"
        }
    )];
    if let Some(m) = &rerank_model {
        parts.push(format!(
            "{}: {}",
            m,
            if rerank_present {
                "cached"
            } else {
                "MISSING (will download on first use)"
            }
        ));
    }
    Check::new("models", status, parts.join(" | "))
}

fn check_hooks(root: &Path) -> Check {
    use crate::commands::hooks::{HooksLayout, classify_hooks_path, read_hooks_path};
    if git_common_dir(root).is_none() {
        return Check::new("hooks", Status::Skip, "not a git repository".to_string());
    }
    let configured = read_hooks_path(root);
    let (hooks_dir, kind, husky_runtime_missing) =
        match classify_hooks_path(root, configured.as_deref()) {
            Ok(HooksLayout::Git(dir)) => (dir, "git", None),
            Ok(HooksLayout::Husky {
                user_dir,
                runtime_dir,
                runtime_present,
            }) => (user_dir, "husky", (!runtime_present).then_some(runtime_dir)),
            Err(e) => {
                return Check::new("hooks", Status::Warn, format!("{e:#}"));
            }
        };

    let mut installed = Vec::new();
    let mut foreign = Vec::new();
    let mut missing = Vec::new();
    let mut dead_binaries: Vec<String> = Vec::new();
    // A marker alone cannot identify the binary the hook invokes.
    let mut unparseable: Vec<&str> = Vec::new();
    for name in REQUIRED_HOOKS {
        let p = hooks_dir.join(name);
        match std::fs::read_to_string(&p) {
            Ok(body) if body.contains("codesage install-hooks") => {
                installed.push(*name);
                match hook_embedded_binary(&body) {
                    Some(bin)
                        if !dead_binaries.contains(&bin)
                            && !is_executable_file(Path::new(&bin)) =>
                    {
                        dead_binaries.push(bin);
                    }
                    None => unparseable.push(*name),
                    _ => {}
                }
            }
            // Foreign hooks require chaining; reinstalling will not overwrite them.
            Ok(_) => foreign.push(*name),
            Err(_) => missing.push(*name),
        }
    }

    // Git runs no Husky hooks until package installation generates .husky/_.
    if let Some(runtime_dir) = husky_runtime_missing
        && !installed.is_empty()
    {
        return Check::new(
            "hooks",
            Status::Warn,
            format!(
                "husky: {} hook(s) installed in {} but husky's runtime dir {} does not exist; \
                 git runs no hooks until a package-manager install regenerates it",
                installed.len(),
                hooks_dir.display(),
                runtime_dir.display()
            ),
        );
    }

    if !dead_binaries.is_empty() {
        return Check::new(
            "hooks",
            Status::Warn,
            format!(
                "{kind}: installed hooks invoke {} which is missing or not executable (re-run `codesage install-hooks`)",
                dead_binaries.join(", ")
            ),
        );
    }

    if !unparseable.is_empty() {
        return Check::new(
            "hooks",
            Status::Warn,
            format!(
                "{kind}: installed hook(s) [{}] carry the marker but no parseable binary path \
                 (re-run `codesage install-hooks` to refresh them)",
                unparseable.join(",")
            ),
        );
    }

    if installed.len() == REQUIRED_HOOKS.len() {
        Check::new(
            "hooks",
            Status::Pass,
            format!(
                "{kind}: all {} installed at {}",
                REQUIRED_HOOKS.len(),
                hooks_dir.display()
            ),
        )
    } else if !foreign.is_empty() {
        let mut parts = Vec::new();
        if !installed.is_empty() {
            parts.push(format!("installed=[{}]", installed.join(",")));
        }
        parts.push(format!(
            "foreign=[{}] — existing non-codesage hook(s); codesage is not wired there. \
             Chain `codesage index --lock-wait 60` and `codesage git-index --incremental \
             --lock-wait 60` into them, or move them aside and re-run `codesage install-hooks`",
            foreign.join(",")
        ));
        if !missing.is_empty() {
            parts.push(format!(
                "missing=[{}] (run `codesage install-hooks`)",
                missing.join(",")
            ));
        }
        Check::new(
            "hooks",
            Status::Warn,
            format!("{kind}: {}", parts.join(" ")),
        )
    } else if installed.is_empty() {
        Check::new(
            "hooks",
            Status::Warn,
            format!(
                "{kind}: no codesage hooks at {} (run `codesage install-hooks`)",
                hooks_dir.display()
            ),
        )
    } else {
        Check::new(
            "hooks",
            Status::Warn,
            format!(
                "{kind}: installed=[{}] missing=[{}]",
                installed.join(","),
                missing.join(",")
            ),
        )
    }
}

/// Lock and log state of the indexing hooks. Reaps a lock no hook run can
/// own (dead or reused pid, or pidless and past `STALE_LOCK_SECS`): doctor
/// is an operator command, so unlike `project_overview` it may clear the way
/// for the next hook fire. A live hook run's lock is only reported.
fn check_hook_health(root: &Path) -> Check {
    use codesage_graph::hook_health::{
        self, LOCK_ABSENT, LOCK_HELD_DEAD, LOCK_HELD_LIVE, STALE_LOCK_SECS,
    };
    let Some(mut health) = hook_health::inspect(root) else {
        return Check::new(
            "hook_health",
            Status::Skip,
            "not a git repository".to_string(),
        );
    };
    let mut status = Status::Pass;
    let mut parts = Vec::new();
    let stale_min = STALE_LOCK_SECS / 60;

    let since = health
        .lock
        .since
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let age = health.lock.age_secs.map(format_age).unwrap_or_default();
    let stale = health.lock.age_secs.is_some_and(|a| a >= STALE_LOCK_SECS);
    let pid = health.lock.pid.unwrap_or(0);
    let state = health.lock.state.clone();
    let mut reap = |parts: &mut Vec<String>, status: &mut Status, held: &str, why: &str| {
        match hook_health::reap_orphan_lock(root) {
            Ok(Some(_)) => {
                health.lock.reaped = true;
                parts.push(format!(
                    "lock: {held} since {since}{age} ({why}; reaped, the next hook fire indexes again)"
                ));
            }
            Ok(None) => parts.push(format!(
                "lock: {held} since {since}{age} ({why}; another process is reaping it or already released it)"
            )),
            Err(e) => {
                *status = Status::Warn;
                parts.push(format!(
                    "lock: {held} since {since}{age} ({why}); reap failed: {e:#}"
                ));
            }
        }
    };
    match state.as_str() {
        s if s == LOCK_ABSENT => parts.push("lock: absent".to_string()),
        s if s == LOCK_HELD_LIVE && stale => {
            status = Status::Warn;
            let minutes = health.lock.age_secs.unwrap_or(0) / 60;
            parts.push(format!(
                "lock: held by live pid {pid} since {since}{age} (hook run still running for {minutes} minutes; a first full pass can take this long, not reaped)"
            ));
        }
        s if s == LOCK_HELD_LIVE => parts.push(format!(
            "lock: held by live pid {pid} since {since}{age} (index in progress)"
        )),
        s if s == LOCK_HELD_DEAD => reap(
            &mut parts,
            &mut status,
            &format!("held by dead pid {pid}"),
            "pid is dead or belongs to an unrelated process",
        ),
        _ if stale => {
            status = Status::Warn;
            reap(
                &mut parts,
                &mut status,
                "held without pid",
                &format!("older than {stale_min} minutes, written by a pre-pid hook"),
            );
        }
        _ => parts.push(format!(
            "lock: held without pid since {since}{age} (pre-upgrade hook or just started; reaped by age after {stale_min} minutes)"
        )),
    }

    match (&health.last_run, health.last_exit) {
        (Some(run), Some(0)) => parts.push(format!("last run {run} exit=0")),
        (Some(run), Some(code)) => {
            status = Status::Warn;
            parts.push(format!("last run {run} exit={code}"));
        }
        (Some(run), None) => parts.push(format!(
            "last run {run} exit=unknown (no exit line: still running, killed, or pre-upgrade hook)"
        )),
        (None, _) => parts.push("last run: none logged".to_string()),
    }

    if health.installed_hooks.is_empty() {
        parts.push("installed hooks: none".to_string());
    } else {
        parts.push(format!(
            "installed hooks: {}",
            health.installed_hooks.join(",")
        ));
    }

    Check {
        name: "hook_health",
        status,
        message: parts.join("; "),
        hook_health: Some(health),
    }
}

fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!(" ({secs}s ago)")
    } else if secs < 3600 {
        format!(" ({}m ago)", secs / 60)
    } else if secs < 86_400 {
        format!(" ({}h{:02}m ago)", secs / 3600, (secs % 3600) / 60)
    } else {
        format!(" ({}d{:02}h ago)", secs / 86_400, (secs % 86_400) / 3600)
    }
}

/// Parse the installer's quoted binary token; unrecognized hook bodies yield None.
fn hook_embedded_binary(body: &str) -> Option<String> {
    for line in body.lines() {
        let Some(idx) = line.find(" index --lock-wait") else {
            continue;
        };
        let before = &line[..idx];
        let start = match before.find("$NICE ") {
            Some(p) => p + "$NICE ".len(),
            None => before.find('\'')?,
        };
        let token = before[start..].trim();
        if !(token.len() >= 2 && token.starts_with('\'') && token.ends_with('\'')) {
            return None;
        }
        // Reverse shell_single_quote's embedded-quote escape.
        let inner = &token[1..token.len() - 1];
        return Some(inner.replace("'\"'\"'", "'"));
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn check_index_drift(root: &Path) -> Check {
    let db_path = root.join(PROJECT_DIR).join(DB_FILE);
    if !db_path.exists() {
        return Check::new(
            "index-drift",
            Status::Skip,
            "no index.db yet (run `codesage index`)".to_string(),
        );
    }
    // A newer binary may have migrated the DB; inspection must not attempt migrations.
    let db = match Database::open_existing_read(&db_path) {
        Ok(db) => db,
        Err(e) => {
            return Check::new(
                "index-drift",
                Status::Fail,
                format!("failed to open index: {e}"),
            );
        }
    };
    let report = check_drift(root, &db);
    let status = match report.kind {
        DriftKind::Fresh => Status::Pass,
        DriftKind::NotGit | DriftKind::NeverIndexed => Status::Skip,
        DriftKind::BehindHead | DriftKind::UnrelatedAncestor => Status::Warn,
        DriftKind::Unknown => Status::Warn,
    };
    Check::new("index-drift", status, report.summary())
}

/// Report fingerprint mismatch; empty tables have no vectors to attest.
fn fingerprint_gate(db: &Database, expected: &str) -> Option<Check> {
    match db.require_semantic_fingerprint(expected) {
        Ok(()) => None,
        Err(e) => Some(Check::new("semantic", Status::Fail, format!("{e:#}"))),
    }
}

fn check_semantic_freshness(root: &Path) -> Check {
    let db_path = root.join(PROJECT_DIR).join(DB_FILE);
    if !db_path.exists() {
        return Check::new(
            "semantic",
            Status::Skip,
            "no index.db yet (run `codesage index`)".to_string(),
        );
    }

    let config = match load_project_config(root) {
        Ok(config) => config,
        Err(e) => {
            return Check::new("semantic", Status::Fail, format!("{e:#}"));
        }
    };
    let emb_config = config.embedding.unwrap_or_default();
    let model = emb_config.model.clone();
    let db = match Database::open_for_existing_model(&db_path, &model) {
        Ok(db) => db,
        Err(e) => {
            return Check::new(
                "semantic",
                Status::Fail,
                format!("failed to open semantic index: {e}"),
            );
        }
    };
    if db.chunk_table_name().is_empty() {
        return Check::new(
            "semantic",
            Status::Warn,
            format!("no semantic chunks for model {model}; run `codesage index`"),
        );
    }
    // Check cached model identity without loading/downloading a model.
    // Unavailable artifacts skip this gate; the models check reports cache misses.
    if let Ok(Some(dim)) = db.recorded_semantic_dim()
        && let Ok(Some(expected)) = codesage_graph::resolve_semantic_fingerprint(
            &db,
            &emb_config,
            dim,
            codesage_graph::ArtifactLookup::CachedOnly,
        )
        && let Some(check) = fingerprint_gate(&db, expected.as_str())
    {
        return check;
    }

    match db.semantic_freshness() {
        Ok(Some(freshness)) if freshness.is_fresh() => Check::new(
            "semantic",
            Status::Pass,
            format!(
                "fresh for model {model}; {} files tracked",
                freshness.indexed_files
            ),
        ),
        Ok(Some(freshness)) => Check::new(
            "semantic",
            Status::Warn,
            format!(
                "{} stale file(s), {} missing file(s) for model {model}; run `codesage index`",
                freshness.stale_files, freshness.missing_files
            ),
        ),
        Ok(None) => Check::new(
            "semantic",
            Status::Warn,
            format!("semantic freshness unavailable for model {model}"),
        ),
        Err(e) => Check::new(
            "semantic",
            Status::Fail,
            format!("failed to check semantic freshness: {e}"),
        ),
    }
}

fn check_mcp() -> Check {
    let out = std::process::Command::new("claude")
        .arg("mcp")
        .arg("list")
        .output();
    let Ok(out) = out else {
        return Check::new("mcp", Status::Skip, "claude CLI not in PATH".to_string());
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let registered = stdout.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("codesage:") || l.starts_with("codesage ")
    });
    if registered {
        Check::new(
            "mcp",
            Status::Pass,
            "codesage registered with Claude Code".to_string(),
        )
    } else {
        Check::new("mcp", Status::Warn, "codesage NOT registered; run `claude mcp add --scope user codesage -- codesage mcp` (or use the codesage-tools plugin)".to_string())
    }
}

use crate::util::git_common_dir;

/// Match hf-hub 0.5.0 Cache::from_env: HF_HOME/hub or ~/.cache/huggingface/hub.
/// HUGGINGFACE_HUB_CACHE is ignored by the loader and must not affect this verdict.
fn hf_cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("HF_HOME") {
        return PathBuf::from(p).join("hub");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cache/huggingface/hub");
    }
    PathBuf::from(".cache/huggingface/hub")
}

fn model_in_cache(cache: &Path, model: &str) -> bool {
    let dir = cache.join(format!("models--{}", model.replace('/', "--")));
    dir.is_dir()
}

use crate::util::format_bytes;

#[cfg(test)]
mod tests {
    use super::*;

    fn init_git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // Hermetic: no user or system git config (init templates, hooksPath).
        for args in [
            &["init"][..],
            &["config", "core.hooksPath", ".git/hooks"][..],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }
        dir
    }

    #[test]
    fn check_hooks_warns_when_husky_runtime_dir_is_missing_then_passes_once_generated() {
        let dir = init_git_repo();
        let status = std::process::Command::new("git")
            .args(["config", "core.hooksPath", ".husky/_"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let husky = dir.path().join(".husky");
        std::fs::create_dir(&husky).unwrap();
        let body = crate::commands::hooks::generate_post_commit_hook_body("/bin/sh");
        for hook in REQUIRED_HOOKS {
            std::fs::write(husky.join(hook), &body).unwrap();
        }

        let check = check_hooks(dir.path());
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("runtime dir"), "{}", check.message);
        assert!(check.message.contains(".husky"), "{}", check.message);

        std::fs::create_dir_all(husky.join("_")).unwrap();
        std::fs::write(husky.join("_").join("h"), "#!/bin/sh\n").unwrap();
        let check = check_hooks(dir.path());
        assert_eq!(check.status, Status::Pass, "{}", check.message);
    }

    fn write_codesage_hook(root: &Path, name: &str) {
        // Marker-only fixtures trigger the unparseable-binary warning.
        let hooks = root.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let body = crate::commands::hooks::generate_post_commit_hook_body("/bin/sh");
        std::fs::write(hooks.join(name), body).unwrap();
    }

    fn init_codesage_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let codesage_dir = dir.path().join(PROJECT_DIR);
        std::fs::create_dir_all(&codesage_dir).unwrap();
        std::fs::write(
            codesage_dir.join("config.toml"),
            "[embedding]\nmodel = \"codesage-test/model\"\ndevice = \"cpu\"\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn check_hook_health_skips_outside_git_and_reports_absent_lock_inside() {
        let dir = tempfile::tempdir().unwrap();
        let check = check_hook_health(dir.path());
        assert_eq!(check.status, Status::Skip, "{}", check.message);
        assert!(check.hook_health.is_none());

        let dir = init_git_repo();
        write_codesage_hook(dir.path(), "post-commit");
        let check = check_hook_health(dir.path());
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("lock: absent"), "{}", check.message);
        assert!(
            check.message.contains("last run: none logged"),
            "{}",
            check.message
        );
        let health = check.hook_health.as_ref().expect("hook_health payload");
        assert_eq!(health.installed_hooks, vec!["post-commit".to_string()]);
        assert_eq!(health.lock.state, "absent");
        let json = serde_json::to_value(&check).unwrap();
        assert!(json["hook_health"]["lock"]["state"] == "absent", "{json}");
        let plain = serde_json::to_value(check_hooks(dir.path())).unwrap();
        assert!(
            plain.get("hook_health").is_none(),
            "other checks must not carry the field: {plain}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_hook_health_reaps_a_dead_pid_lock_and_says_so() {
        let dir = init_git_repo();
        let lockdir = dir.path().join(".codesage/hook-index.lock");
        std::fs::create_dir_all(&lockdir).unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        std::fs::write(lockdir.join("pid"), format!("{dead}\n")).unwrap();
        std::fs::write(
            dir.path().join(".codesage/hooks.log"),
            "[Sun Sep 14 07:09:37 UTC 2026] post-merge hook start pid=42\n",
        )
        .unwrap();

        let check = check_hook_health(dir.path());
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(
            check
                .message
                .contains(&format!("held by dead pid {dead} since ")),
            "{}",
            check.message
        );
        assert!(check.message.contains("; reaped,"), "{}", check.message);
        assert!(
            check
                .message
                .contains("last run Sun Sep 14 07:09:37 UTC 2026 exit=unknown"),
            "{}",
            check.message
        );
        let health = check.hook_health.unwrap();
        assert_eq!(health.lock.state, "held_dead");
        assert_eq!(health.lock.pid, Some(dead));
        assert!(health.lock.reaped);
        assert_eq!(health.last_exit, None);
        assert!(!lockdir.exists(), "doctor must remove the dead lock");
    }

    #[cfg(unix)]
    #[test]
    fn check_hook_health_leaves_a_live_pid_lock_alone() {
        let dir = init_git_repo();
        let lockdir = dir.path().join(".codesage/hook-index.lock");
        std::fs::create_dir_all(&lockdir).unwrap();
        let me = std::process::id();
        std::fs::write(lockdir.join("pid"), me.to_string()).unwrap();
        std::fs::write(
            dir.path().join(".codesage/hooks.log"),
            "[Sun Sep 14 07:09:37 UTC 2026] post-merge hook start pid=42\n\
             [Sun Sep 14 07:09:40 UTC 2026] post-merge hook exit=7 pid=42\n",
        )
        .unwrap();

        let check = check_hook_health(dir.path());
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check
                .message
                .contains(&format!("held by live pid {me} since ")),
            "{}",
            check.message
        );
        assert!(check.message.contains(" exit=7"), "{}", check.message);
        let health = check.hook_health.unwrap();
        assert_eq!(health.lock.state, "held_live");
        assert_eq!(health.last_exit, Some(7));
        assert!(!health.lock.reaped);
        assert!(
            lockdir.join("pid").is_file(),
            "live lock must survive doctor"
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_hook_health_warns_on_a_live_pid_lock_past_the_ceiling_without_reaping() {
        use codesage_graph::hook_health::STALE_LOCK_SECS;
        let dir = init_git_repo();
        let lockdir = dir.path().join(".codesage/hook-index.lock");
        std::fs::create_dir_all(&lockdir).unwrap();
        let me = std::process::id();
        std::fs::write(lockdir.join("pid"), me.to_string()).unwrap();
        let old =
            std::time::SystemTime::now() - std::time::Duration::from_secs(STALE_LOCK_SECS + 60);
        std::fs::File::open(&lockdir)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let check = check_hook_health(dir.path());
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check
                .message
                .contains(&format!("held by live pid {me} since ")),
            "{}",
            check.message
        );
        assert!(
            check.message.contains("still running for") && check.message.contains("not reaped"),
            "{}",
            check.message
        );
        let health = check.hook_health.unwrap();
        assert_eq!(health.lock.state, "held_live");
        assert!(!health.lock.reaped);
        assert!(
            lockdir.join("pid").is_file(),
            "a live hook run's lock is never reaped"
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_hook_health_treats_a_reused_pid_as_dead() {
        let dir = init_git_repo();
        let lockdir = dir.path().join(".codesage/hook-index.lock");
        std::fs::create_dir_all(&lockdir).unwrap();
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let reused = sleeper.id();
        std::fs::write(lockdir.join("pid"), reused.to_string()).unwrap();

        let check = check_hook_health(dir.path());
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        assert!(
            check
                .message
                .contains(&format!("held by dead pid {reused} since "))
                && check.message.contains("unrelated process"),
            "{}",
            check.message
        );
        let health = check.hook_health.unwrap();
        assert_eq!(health.lock.state, "held_dead");
        assert!(health.lock.reaped);
        assert!(
            !lockdir.exists(),
            "a live `sleep` is no hook run: the lock is reaped"
        );
    }

    #[test]
    fn check_hook_health_reports_a_pidless_lock_by_age() {
        let dir = init_git_repo();
        let lockdir = dir.path().join(".codesage/hook-index.lock");
        std::fs::create_dir_all(&lockdir).unwrap();

        let check = check_hook_health(dir.path());
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(
            check.message.contains("held without pid since "),
            "{}",
            check.message
        );
        assert!(
            lockdir.is_dir(),
            "a pidless lock is the hook's to reap by age"
        );

        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 60);
        std::fs::File::open(&lockdir)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let check = check_hook_health(dir.path());
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check.message.contains("older than 30 minutes"),
            "{}",
            check.message
        );
        assert!(check.message.contains("; reaped,"), "{}", check.message);
        let health = check.hook_health.unwrap();
        assert_eq!(health.lock.state, "held_no_pid");
        assert!(health.lock.age_secs.is_some_and(|a| a >= 31 * 60));
        assert!(health.lock.reaped);
        assert!(
            !lockdir.exists(),
            "doctor reaps a pidless lock past the ceiling"
        );
    }

    #[test]
    fn check_hooks_requires_post_rewrite() {
        let dir = init_git_repo();
        for hook in ["post-commit", "post-merge", "post-checkout"] {
            write_codesage_hook(dir.path(), hook);
        }

        let check = check_hooks(dir.path());

        assert_eq!(check.status, Status::Warn);
        assert!(
            check.message.contains("post-rewrite"),
            "message should name missing post-rewrite hook: {}",
            check.message
        );
    }

    #[test]
    fn check_hooks_passes_when_all_required_hooks_are_present() {
        let dir = init_git_repo();
        for hook in REQUIRED_HOOKS {
            write_codesage_hook(dir.path(), hook);
        }

        let check = check_hooks(dir.path());

        assert_eq!(check.status, Status::Pass);
    }

    fn write_hook_body(root: &Path, name: &str, body: &str) {
        let hooks = root.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join(name), body).unwrap();
    }

    #[test]
    fn check_hooks_distinguishes_foreign_hook_from_missing() {
        let dir = init_git_repo();
        write_hook_body(
            dir.path(),
            "post-commit",
            "#!/bin/sh\n# someone else's hook\nexit 0\n",
        );

        let check = check_hooks(dir.path());

        assert_eq!(check.status, Status::Warn);
        assert!(
            check.message.contains("foreign=[post-commit]"),
            "foreign slot must be named as foreign: {}",
            check.message
        );
        assert!(
            check
                .message
                .contains("missing=[post-merge,post-checkout,post-rewrite]"),
            "absent hooks must still be listed as missing: {}",
            check.message
        );
        assert!(
            check.message.contains("chain") || check.message.contains("Chain"),
            "foreign warning must carry the chaining remediation: {}",
            check.message
        );
    }

    #[test]
    fn check_hooks_all_foreign_is_not_reported_as_no_hooks() {
        let dir = init_git_repo();
        for hook in REQUIRED_HOOKS {
            write_hook_body(dir.path(), hook, "#!/bin/sh\n# husky or friends\nexit 0\n");
        }

        let check = check_hooks(dir.path());

        assert_eq!(check.status, Status::Warn);
        assert!(
            check
                .message
                .contains("foreign=[post-commit,post-merge,post-checkout,post-rewrite]"),
            "all four slots are foreign, message: {}",
            check.message
        );
        assert!(
            !check.message.contains("no codesage hooks at"),
            "foreign-held slots must not read as plain missing: {}",
            check.message
        );
        assert!(
            !check.message.contains("missing=["),
            "nothing is missing when every slot exists: {}",
            check.message
        );
    }

    #[test]
    fn check_hooks_warns_when_embedded_binary_is_gone() {
        let dir = init_git_repo();
        let body =
            crate::commands::hooks::generate_post_commit_hook_body("/nonexistent/path/codesage");
        for hook in REQUIRED_HOOKS {
            write_hook_body(dir.path(), hook, &body);
        }

        let check = check_hooks(dir.path());

        assert_eq!(check.status, Status::Warn);
        assert!(
            check.message.contains("/nonexistent/path/codesage"),
            "message should name the dead binary path: {}",
            check.message
        );
        assert!(
            check.message.contains("not executable"),
            "dead-binary warning must be distinct from a missing-hook warning: {}",
            check.message
        );
    }

    #[test]
    fn check_hooks_passes_when_embedded_binary_is_executable() {
        let dir = init_git_repo();
        let body = crate::commands::hooks::generate_post_commit_hook_body("/bin/sh");
        for hook in REQUIRED_HOOKS {
            write_hook_body(dir.path(), hook, &body);
        }

        let check = check_hooks(dir.path());

        assert_eq!(check.status, Status::Pass, "message: {}", check.message);
    }

    #[test]
    fn check_hooks_warns_when_installed_hook_has_no_parseable_binary() {
        let dir = init_git_repo();
        for hook in REQUIRED_HOOKS {
            write_hook_body(
                dir.path(),
                hook,
                "#!/bin/sh\n# installed by codesage install-hooks\ntrue\n",
            );
        }

        let check = check_hooks(dir.path());

        assert_eq!(check.status, Status::Warn);
        assert!(
            check.message.contains("no parseable binary path"),
            "message should name the unparseable hooks: {}",
            check.message
        );
        assert!(
            check.message.contains("install-hooks"),
            "message should carry the remediation: {}",
            check.message
        );
    }

    #[test]
    fn hook_embedded_binary_unquotes_shell_escaped_paths() {
        let body = crate::commands::hooks::generate_post_commit_hook_body("/tmp/a'b/codesage");
        assert_eq!(
            hook_embedded_binary(&body).as_deref(),
            Some("/tmp/a'b/codesage")
        );
        assert_eq!(
            hook_embedded_binary("#!/bin/sh\n# installed by codesage install-hooks\n"),
            None
        );
    }

    #[test]
    fn semantic_fingerprint_gate_fails_naming_full_rebuild_on_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let db = Database::open_for_model(&path, "fp/model", 4).unwrap();
        assert!(fingerprint_gate(&db, "fp-b").is_none());
        db.insert_chunks(
            "a.rs",
            "rust",
            &[("fn a() {}", 1, 1, &[0.0, 0.0, 0.0, 1.0])],
        )
        .unwrap();
        db.record_semantic_fingerprint("fp-a").unwrap();
        assert!(fingerprint_gate(&db, "fp-a").is_none());
        let check = fingerprint_gate(&db, "fp-b").expect("mismatch must fail");
        assert_eq!(check.name, "semantic");
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.message.contains("index --full"),
            "mismatch must name the rebuild: {}",
            check.message
        );
    }

    #[test]
    fn check_models_fails_on_non_allowlisted_model() {
        // Skip when the operator has disabled model allowlist enforcement.
        if codesage_embed::model::allow_any_model_from_env() {
            return;
        }
        let dir = init_codesage_project(); // model = codesage-test/model (not allowlisted)
        let check = check_models(Some(dir.path()));
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.message.contains("CODESAGE_ALLOW_ANY_MODEL"),
            "message should name the override env var: {}",
            check.message
        );
    }

    #[test]
    fn semantic_freshness_warns_when_hashes_are_stale() {
        let dir = init_codesage_project();
        let db_path = dir.path().join(PROJECT_DIR).join(DB_FILE);
        let db = Database::open_for_model(
            &db_path,
            "codesage-test/model",
            codesage_storage::db::DEFAULT_EMBEDDING_DIM,
        )
        .unwrap();
        db.upsert_file(&codesage_protocol::FileInfo {
            path: "src/lib.rs".to_string(),
            language: codesage_protocol::Language::Rust,
            content_hash: "new".to_string(),
        })
        .unwrap();
        db.upsert_semantic_file_hash("src/lib.rs", "old").unwrap();

        let check = check_semantic_freshness(dir.path());

        assert_eq!(check.status, Status::Warn);
        assert!(
            check.message.contains("1 stale"),
            "message should name stale semantic files: {}",
            check.message
        );
    }
}
