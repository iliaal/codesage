//! Shared helpers for CLI + doctor.

use std::path::Path;

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

/// Canonical git-common-dir resolution lives in the graph crate beside the
/// drift instrumentation; re-exported here so CLI modules keep one import path.
pub(crate) use codesage_graph::drift::git_common_dir;

/// True when `a` and `b` resolve to the same filesystem location. Tries
/// `canonicalize` first (which follows symlinks and normalizes `..`); if
/// either path can't be canonicalized — typically because it doesn't exist
/// yet — falls back to lexical equality.
pub(crate) fn paths_resolve_same(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn paths_resolve_same_handles_redundant_dot_segments() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("hooks");
        std::fs::create_dir(&real).unwrap();
        let with_dot = dir.path().join(".").join("hooks");
        assert!(paths_resolve_same(&real, &with_dot));
    }

    #[test]
    fn paths_resolve_same_distinguishes_different_dirs() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        assert!(!paths_resolve_same(&a, &b));
    }

    #[test]
    fn paths_resolve_same_falls_back_lexically_when_paths_missing() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(paths_resolve_same(&missing, &missing));
        let other = dir.path().join("also-missing");
        assert!(!paths_resolve_same(&missing, &other));
    }
}
