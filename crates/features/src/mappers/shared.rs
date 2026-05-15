//! Shared helpers for per-language mappers: safe directory walks (realpath
//! + escape detection), normalized path strings, and glob-style listing.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// Returns true if `candidate` is inside `root` after symlink resolution.
/// Used to defend mappers against repos that contain symlinks pointing
/// outside the tree (rare but real; clawpatch's mapper guard inspired this).
pub fn is_inside_root(root: &Path, candidate: &Path) -> bool {
    let real_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let real_candidate = fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());
    real_candidate.starts_with(&real_root)
}

/// File exists, is a regular file, is inside `root` after symlink check, and
/// is not itself a symlink. Returns false on any I/O error.
pub fn is_safe_file(root: &Path, candidate: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(candidate) else {
        return false;
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    is_inside_root(root, candidate)
}

/// Directory exists, is a real directory (not a symlink), and is inside
/// `root` after canonicalization.
pub fn is_safe_dir(root: &Path, candidate: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(candidate) else {
        return false;
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
    is_inside_root(root, candidate)
}

/// Convert an absolute path inside `root` to a repo-relative POSIX string.
/// Falls back to the absolute path on canonicalization failure.
pub fn rel_path(root: &Path, abs: &Path) -> String {
    let stripped = abs.strip_prefix(root).unwrap_or(abs);
    let s = stripped.to_string_lossy().into_owned();
    s.replace('\\', "/")
}

/// Walk a directory subtree, yielding all regular-file repo-relative paths
/// inside `root` that aren't symlinks and don't sit under common ignore
/// directories. Bounded by `max_files` to keep mapper scans cheap on big
/// repos; returns the partial set when exceeded.
pub fn walk_files(root: &Path, start: &Path, max_files: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    walk_inner(root, start, max_files, &mut out, &mut seen);
    out.sort();
    out.dedup();
    out
}

fn walk_inner(
    root: &Path,
    dir: &Path,
    max_files: usize,
    out: &mut Vec<String>,
    seen: &mut BTreeSet<PathBuf>,
) {
    if out.len() >= max_files {
        return;
    }
    if !is_safe_dir(root, dir) {
        return;
    }
    if should_skip(&rel_path(root, dir)) {
        return;
    }
    let canonical = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !seen.insert(canonical) {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk_inner(root, &path, max_files, out, seen);
            if out.len() >= max_files {
                return;
            }
        } else if meta.is_file() {
            let rel = rel_path(root, &path);
            if !should_skip(&rel) {
                out.push(rel);
                if out.len() >= max_files {
                    return;
                }
            }
        }
    }
}

/// Directory- or file-relative ignore predicate. Mirrors the
/// `DEFAULT_EXCLUDE_PATTERNS` policy at the parser layer plus mapper-specific
/// additions (target/, .build/, .codesage/, vendor/).
pub fn should_skip(rel: &str) -> bool {
    if rel.is_empty() || rel == "." {
        return false;
    }
    let parts = rel.split('/');
    for part in parts {
        match part {
            "node_modules" | "dist" | "build" | "coverage" | ".git" | ".codesage" | "vendor"
            | "target" | ".build" | ".next" | "__pycache__" | ".venv" | "venv" => return true,
            _ => {}
        }
    }
    false
}

/// Read a file's contents to a UTF-8 string, returning None on any error
/// (missing file, binary, permission). Bounded by an internal 1 MB cap so
/// a stray large generated file doesn't blow up a mapper that only wants
/// to grep a header.
pub fn read_to_string_bounded(path: &Path) -> Result<Option<String>> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if meta.len() > 1_000_000 {
        return Ok(None);
    }
    Ok(fs::read_to_string(path).ok())
}

/// Strip TOML `#`-style line comments while leaving the strings inside
/// quoted values intact. Cheap helper used by mappers that parse
/// `Cargo.toml` or similar without pulling in a full toml crate. Matches
/// clawpatch's `stripLineComment` behavior.
pub fn strip_line_comments(source: &str, marker: char) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.lines() {
        out.push_str(&strip_line_comment(line, marker));
        out.push('\n');
    }
    out
}

fn strip_line_comment(line: &str, marker: char) -> String {
    let mut in_string = false;
    let mut escaped = false;
    let bytes = line.as_bytes();
    let mut cut: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        let c = b as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        } else if c == marker {
            cut = Some(i);
            break;
        }
    }
    match cut {
        Some(i) => line[..i].to_string(),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn should_skip_catches_known_dirs() {
        assert!(should_skip("node_modules/foo.js"));
        assert!(should_skip("crates/foo/target/debug/x"));
        assert!(should_skip(".git/HEAD"));
        assert!(should_skip("vendor/lib/file.php"));
        assert!(!should_skip("src/main.rs"));
        assert!(!should_skip("crates/foo/src/main.rs"));
    }

    #[test]
    fn walk_skips_symlinks_and_escape_paths() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let outside = tempdir().unwrap();
        fs::write(root.join("real.rs"), b"fn main() {}").unwrap();
        // symlink pointing outside the root should not be walked.
        let _ = symlink(outside.path(), root.join("escape"));
        let walked = walk_files(root, root, 100);
        assert!(walked.iter().any(|p| p == "real.rs"));
        assert!(
            walked.iter().all(|p| !p.starts_with("escape")),
            "got {:?}",
            walked
        );
    }

    #[test]
    fn rel_path_is_posix() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let sub = root.join("a").join("b.txt");
        assert_eq!(rel_path(root, &sub), "a/b.txt");
    }

    #[test]
    fn strip_line_comment_preserves_strings() {
        let src = "name = \"foo # not a comment\" # real comment\n";
        let out = strip_line_comments(src, '#');
        assert_eq!(out, "name = \"foo # not a comment\" \n");
    }
}
