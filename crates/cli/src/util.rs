//! Shared helpers for CLI + doctor.

use std::path::Path;

use tracing_subscriber::EnvFilter;

/// Keep tracing on stderr; stdout carries MCP and JSON. Repeated initialization is harmless.
pub(crate) fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

pub(crate) use codesage_graph::drift::git_common_dir;

/// Compare canonical paths, falling back to lexical equality if either cannot resolve.
pub(crate) fn paths_resolve_same(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

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
