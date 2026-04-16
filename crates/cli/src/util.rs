//! Shared helpers for CLI + doctor.

use std::path::{Path, PathBuf};

use tracing_subscriber::EnvFilter;

/// Initialize the global `tracing` subscriber for the CLI. Writes to **stderr**
/// (stdout is reserved for MCP stdio transport and the CLI's structured JSON
/// output). Honors `RUST_LOG`; falls back to `info`. Uses `try_init` so repeated
/// initialization (tests, nested binaries) is a no-op rather than a panic.
pub(crate) fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

/// Resolve the canonical git common directory (the actual `.git`, even from
/// inside a worktree) for `cwd`. Returns `None` when not a git repo or git is
/// unavailable. Result paths are absolute.
pub(crate) fn git_common_dir(cwd: &Path) -> Option<PathBuf> {
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
    let path = Path::new(dir);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

/// Format a byte count using binary (GiB/MiB/KiB) units. Consistent on both
/// the CLI reports and doctor output.
pub(crate) fn format_bytes(n: u64) -> String {
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
