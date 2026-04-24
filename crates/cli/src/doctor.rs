use std::path::{Path, PathBuf};

use anyhow::Result;
use codesage_storage::Database;

use crate::drift::{DriftKind, check_drift};
use crate::{DB_FILE, PROJECT_DIR, find_project_root_opt, load_project_config};

#[derive(Debug, Clone, Copy, serde::Serialize)]
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
}

pub fn run(json: bool) -> Result<()> {
    let mut checks = Vec::new();

    checks.push(check_binary());

    let project = find_project_root_opt();

    if let Some(root) = &project {
        checks.push(check_config(root));
        checks.push(check_db(root));
        checks.push(check_disk(root));
        checks.push(check_hooks(root));
        checks.push(check_index_drift(root));
    } else {
        checks.push(Check {
            name: "project",
            status: Status::Skip,
            message: "not in a codesage project (run `codesage init` first)".to_string(),
        });
    }

    checks.push(check_cuda(project.as_deref()));
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
    Check {
        name: "binary",
        status: Status::Pass,
        message: format!("codesage {version} ({profile}, {cuda})"),
    }
}

fn check_config(root: &Path) -> Check {
    let config_path = root.join(PROJECT_DIR).join("config.toml");
    if !config_path.exists() {
        return Check {
            name: "config",
            status: Status::Fail,
            message: format!("missing {}", config_path.display()),
        };
    }
    let config = match load_project_config(root) {
        Ok(c) => c,
        Err(e) => {
            return Check {
                name: "config",
                status: Status::Fail,
                message: format!("{e:#}"),
            };
        }
    };
    let emb = config.embedding.unwrap_or_default();
    let reranker = emb
        .reranker
        .as_deref()
        .map(|r| format!(" reranker={r}"))
        .unwrap_or_default();
    Check {
        name: "config",
        status: Status::Pass,
        message: format!("model={} device={}{reranker}", emb.model, emb.device),
    }
}

fn check_db(root: &Path) -> Check {
    let db_path = root.join(PROJECT_DIR).join(DB_FILE);
    if !db_path.exists() {
        return Check {
            name: "db",
            status: Status::Warn,
            message: format!("missing {} (run `codesage index`)", db_path.display()),
        };
    }
    match Database::open(&db_path) {
        Ok(db) => {
            let f = db.file_count().unwrap_or(0);
            let s = db.symbol_count().unwrap_or(0);
            let r = db.reference_count().unwrap_or(0);
            Check {
                name: "db",
                status: Status::Pass,
                message: format!("schema OK; files={f} symbols={s} refs={r}"),
            }
        }
        Err(e) => Check {
            name: "db",
            status: Status::Fail,
            message: format!("failed to open: {e}"),
        },
    }
}

fn check_disk(root: &Path) -> Check {
    let db_path = root.join(PROJECT_DIR).join(DB_FILE);
    let size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    Check {
        name: "disk",
        status: Status::Pass,
        message: format!("index.db {}", format_bytes(size)),
    }
}

fn check_cuda(project: Option<&Path>) -> Check {
    let want_gpu = project
        .and_then(|root| {
            let config = load_project_config(root).ok()?;
            config
                .embedding
                .map(|e| e.device == "gpu" || e.device == "cuda")
        })
        .unwrap_or(false);
    let built_with_cuda = cfg!(feature = "cuda");

    if !want_gpu {
        return Check {
            name: "cuda",
            status: Status::Pass,
            message: "config requests CPU; CUDA not required".to_string(),
        };
    }
    if !built_with_cuda {
        return Check {
            name: "cuda",
            status: Status::Fail,
            message: "config wants device=gpu but binary built WITHOUT cuda feature; rebuild with `cargo build --release --features cuda`".to_string(),
        };
    }
    match codesage_embed::nvidia_lib_dirs() {
        dirs if dirs.is_empty() => Check {
            name: "cuda",
            status: Status::Warn,
            message:
                "nvidia libs not found; set CODESAGE_NVIDIA_LIBS, or install the `nvidia-*-cu12` pip \
                 packages (cudnn, cublas, cuda-runtime, cufft, curand, cuda-nvrtc). First \
                 GPU session will likely fail to register the CUDA provider."
                    .to_string(),
        },
        dirs => Check {
            name: "cuda",
            status: Status::Pass,
            message: format!("cuda feature compiled; {} nvidia lib dir(s) discovered", dirs.len()),
        },
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
    Check {
        name: "models",
        status,
        message: parts.join(" | "),
    }
}

fn check_hooks(root: &Path) -> Check {
    let configured = std::process::Command::new("git")
        .arg("config")
        .arg("--get")
        .arg("core.hooksPath")
        .current_dir(root)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            } else {
                None
            }
        });

    let (hooks_dir, kind) = match configured {
        None => match git_common_dir(root) {
            Some(c) => (c.join("hooks"), "git"),
            None => {
                return Check {
                    name: "hooks",
                    status: Status::Skip,
                    message: "not a git repository".to_string(),
                };
            }
        },
        Some(p) => {
            let path = if std::path::Path::new(&p).is_absolute() {
                PathBuf::from(p)
            } else {
                root.join(&p)
            };
            if path.join("h").is_file() || path.join("husky.sh").is_file() {
                let user = path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or(path.clone());
                (user, "husky")
            } else {
                return Check {
                    name: "hooks",
                    status: Status::Warn,
                    message: format!(
                        "core.hooksPath = {} unrecognized; cannot install hooks here",
                        path.display()
                    ),
                };
            }
        }
    };

    let mut installed = Vec::new();
    let mut missing = Vec::new();
    for name in &["post-commit", "post-merge", "post-checkout"] {
        let p = hooks_dir.join(name);
        let codesage = p.exists()
            && std::fs::read_to_string(&p)
                .map(|c| c.contains("codesage install-hooks"))
                .unwrap_or(false);
        if codesage {
            installed.push(*name);
        } else {
            missing.push(*name);
        }
    }

    if installed.len() == 3 {
        Check {
            name: "hooks",
            status: Status::Pass,
            message: format!("{kind}: all three installed at {}", hooks_dir.display()),
        }
    } else if installed.is_empty() {
        Check {
            name: "hooks",
            status: Status::Warn,
            message: format!(
                "{kind}: no codesage hooks at {} (run `codesage install-hooks`)",
                hooks_dir.display()
            ),
        }
    } else {
        Check {
            name: "hooks",
            status: Status::Warn,
            message: format!(
                "{kind}: installed=[{}] missing=[{}]",
                installed.join(","),
                missing.join(",")
            ),
        }
    }
}

/// Drift telemetry: compares the HEAD SHA stamped at the last successful
/// `codesage index` against the current `git rev-parse HEAD`. Warning
/// classification (Pass/Warn/Skip) matches what the drift module classifies —
/// we surface it here so `codesage doctor` is a single stop for "is my index
/// trustworthy right now?".
fn check_index_drift(root: &Path) -> Check {
    let db_path = root.join(PROJECT_DIR).join(DB_FILE);
    if !db_path.exists() {
        return Check {
            name: "index-drift",
            status: Status::Skip,
            message: "no index.db yet (run `codesage index`)".to_string(),
        };
    }
    let db = match Database::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            return Check {
                name: "index-drift",
                status: Status::Fail,
                message: format!("failed to open index: {e}"),
            };
        }
    };
    let report = check_drift(root, &db);
    let status = match report.kind {
        DriftKind::Fresh => Status::Pass,
        DriftKind::NotGit | DriftKind::NeverIndexed => Status::Skip,
        DriftKind::BehindHead | DriftKind::UnrelatedAncestor => Status::Warn,
        DriftKind::Unknown => Status::Warn,
    };
    Check {
        name: "index-drift",
        status,
        message: report.summary(),
    }
}

fn check_mcp() -> Check {
    let out = std::process::Command::new("claude")
        .arg("mcp")
        .arg("list")
        .output();
    let Ok(out) = out else {
        return Check {
            name: "mcp",
            status: Status::Skip,
            message: "claude CLI not in PATH".to_string(),
        };
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let registered = stdout.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("codesage:") || l.starts_with("codesage ")
    });
    if registered {
        Check {
            name: "mcp",
            status: Status::Pass,
            message: "codesage registered with Claude Code".to_string(),
        }
    } else {
        Check {
            name: "mcp",
            status: Status::Warn,
            message: "codesage NOT registered; run `claude mcp add --scope user codesage -- codesage mcp` (or use the codesage-tools plugin)".to_string(),
        }
    }
}

use crate::util::git_common_dir;

fn hf_cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("HF_HOME") {
        return PathBuf::from(p).join("hub");
    }
    if let Ok(p) = std::env::var("HUGGINGFACE_HUB_CACHE") {
        return PathBuf::from(p);
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
