//! Rust mapper: `src/main.rs`, `src/bin/*.rs`, `src/lib.rs`, Cargo workspace
//! members from `[workspace] members = [...]`, and integration tests in
//! `tests/*.rs`. Translates clawpatch's Rust mapper (src/mappers/rust.ts)
//! to native Rust with the same shape.

use std::fs;
use std::path::Path;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;

use crate::mappers::shared::{is_safe_dir, is_safe_file, rel_path, strip_line_comments};
use crate::mappers::types::{FeatureMapper, FeatureSeed, SeedFile};

pub struct RustMapper;

impl FeatureMapper for RustMapper {
    fn name(&self) -> &'static str {
        "rust"
    }
    fn map(&self, root: &Path) -> Result<Vec<FeatureSeed>> {
        let manifest = root.join("Cargo.toml");
        if !is_safe_file(root, &manifest) {
            return Ok(Vec::new());
        }
        let mut seeds = Vec::new();
        seed_for_package(root, root, &mut seeds, "cargo-root")?;
        // Workspace members.
        for member_dir in cargo_workspace_members(root, &manifest)? {
            let full = root.join(&member_dir);
            if !is_safe_dir(root, &full) {
                continue;
            }
            seed_for_package(root, &full, &mut seeds, "cargo-workspace-member")?;
        }
        // Conventional `crates/*` even when not declared in workspace.
        let crates_dir = root.join("crates");
        if is_safe_dir(root, &crates_dir) {
            for entry in fs::read_dir(&crates_dir)?.flatten() {
                let p = entry.path();
                if !is_safe_dir(root, &p) {
                    continue;
                }
                if !is_safe_file(root, &p.join("Cargo.toml")) {
                    continue;
                }
                seed_for_package(root, &p, &mut seeds, "cargo-workspace-member")?;
            }
        }
        Ok(seeds)
    }
}

fn seed_for_package(
    root: &Path,
    pkg_dir: &Path,
    seeds: &mut Vec<FeatureSeed>,
    source: &'static str,
) -> Result<()> {
    let manifest = pkg_dir.join("Cargo.toml");
    if !is_safe_file(root, &manifest) {
        return Ok(());
    }
    let pkg_name = read_package_name(&manifest).unwrap_or_else(|| {
        pkg_dir
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "crate".to_string())
    });
    let test_command = format!("cargo test --package {pkg_name}");
    // Binary entrypoint: src/main.rs.
    let main_rs = pkg_dir.join("src/main.rs");
    if is_safe_file(root, &main_rs) {
        let entry = rel_path(root, &main_rs);
        seeds.push(FeatureSeed {
            title: format!("Rust binary `{pkg_name}`"),
            summary: format!("Cargo binary entrypoint at {entry}"),
            kind: FeatureKind::CliCommand,
            source,
            confidence: FeatureConfidence::High,
            entry_path: entry.clone(),
            entry_symbol: Some("main".to_string()),
            entry_route: None,
            entry_command: Some(pkg_name.clone()),
            language: Language::Rust,
            tags: vec!["rust".to_string(), "cli".to_string()],
            owned_files: lib_rs_as_owned(root, pkg_dir),
            context_files: cargo_toml_context(root, pkg_dir),
            tests: Vec::new(),
            test_prefixes: vec![rel_path(root, &pkg_dir.join("tests"))],
        });
    }
    // Library entrypoint: src/lib.rs (separate feature even if main.rs exists).
    let lib_rs = pkg_dir.join("src/lib.rs");
    if is_safe_file(root, &lib_rs) {
        let entry = rel_path(root, &lib_rs);
        seeds.push(FeatureSeed {
            title: format!("Rust library `{pkg_name}`"),
            summary: format!("Cargo library crate at {entry}"),
            kind: FeatureKind::Library,
            source,
            confidence: FeatureConfidence::High,
            entry_path: entry.clone(),
            entry_symbol: None,
            entry_route: None,
            entry_command: None,
            language: Language::Rust,
            tags: vec!["rust".to_string(), "library".to_string()],
            owned_files: Vec::new(),
            context_files: cargo_toml_context(root, pkg_dir),
            tests: Vec::new(),
            test_prefixes: vec![rel_path(root, &pkg_dir.join("tests"))],
        });
    }
    // Additional bins under src/bin/*.rs (one feature each).
    let bin_dir = pkg_dir.join("src/bin");
    if is_safe_dir(root, &bin_dir) {
        for entry in fs::read_dir(&bin_dir)?.flatten() {
            let p = entry.path();
            if let Some(file_name) = p.file_name().and_then(|s| s.to_str())
                && file_name.ends_with(".rs")
                && is_safe_file(root, &p)
            {
                let bin_name = file_name.trim_end_matches(".rs").to_string();
                let entry_rel = rel_path(root, &p);
                seeds.push(FeatureSeed {
                    title: format!("Rust binary `{bin_name}` ({pkg_name})"),
                    summary: format!("Cargo bin target at {entry_rel}"),
                    kind: FeatureKind::CliCommand,
                    source: "cargo-bin",
                    confidence: FeatureConfidence::High,
                    entry_path: entry_rel,
                    entry_symbol: Some("main".to_string()),
                    entry_route: None,
                    entry_command: Some(bin_name),
                    language: Language::Rust,
                    tags: vec!["rust".to_string(), "cli".to_string()],
                    owned_files: Vec::new(),
                    context_files: cargo_toml_context(root, pkg_dir),
                    tests: Vec::new(),
                    test_prefixes: vec![rel_path(root, &pkg_dir.join("tests"))],
                });
            }
        }
    }
    // Integration tests under tests/*.rs.
    let tests_dir = pkg_dir.join("tests");
    if is_safe_dir(root, &tests_dir) {
        for entry in fs::read_dir(&tests_dir)?.flatten() {
            let p = entry.path();
            if let Some(file_name) = p.file_name().and_then(|s| s.to_str())
                && file_name.ends_with(".rs")
                && is_safe_file(root, &p)
            {
                let test_name = file_name.trim_end_matches(".rs").to_string();
                let entry_rel = rel_path(root, &p);
                seeds.push(FeatureSeed {
                    title: format!("Rust integration test `{test_name}` ({pkg_name})"),
                    summary: format!("Integration test file at {entry_rel}"),
                    kind: FeatureKind::TestSuite,
                    source: "rust-integration-test",
                    confidence: FeatureConfidence::Medium,
                    entry_path: entry_rel.clone(),
                    entry_symbol: None,
                    entry_route: None,
                    entry_command: Some(test_command.clone()),
                    language: Language::Rust,
                    tags: vec!["rust".to_string(), "test".to_string()],
                    owned_files: Vec::new(),
                    context_files: Vec::new(),
                    tests: Vec::new(),
                    test_prefixes: Vec::new(),
                });
            }
        }
    }
    Ok(())
}

fn lib_rs_as_owned(root: &Path, pkg_dir: &Path) -> Vec<SeedFile> {
    let lib = pkg_dir.join("src/lib.rs");
    if is_safe_file(root, &lib) {
        vec![SeedFile {
            path: rel_path(root, &lib),
            reason: "package library entry adjacent to binary".to_string(),
        }]
    } else {
        Vec::new()
    }
}

fn cargo_toml_context(root: &Path, pkg_dir: &Path) -> Vec<SeedFile> {
    let m = pkg_dir.join("Cargo.toml");
    if is_safe_file(root, &m) {
        vec![SeedFile {
            path: rel_path(root, &m),
            reason: "package manifest".to_string(),
        }]
    } else {
        Vec::new()
    }
}

fn read_package_name(manifest_path: &Path) -> Option<String> {
    let raw = fs::read_to_string(manifest_path).ok()?;
    let stripped = strip_line_comments(&raw, '#');
    // Find `[package]` section then `name = "..."` within it.
    let pkg_start = Regex::new(r"(?m)^\s*\[package\]\s*$")
        .ok()?
        .find(&stripped)?;
    let rest = &stripped[pkg_start.end()..];
    let next_section = Regex::new(r"(?m)^\s*\[[^\]]+\]\s*$")
        .ok()?
        .find(rest)
        .map(|m| m.start())
        .unwrap_or(rest.len());
    let body = &rest[..next_section];
    let name_re = Regex::new(r#"(?m)^\s*name\s*=\s*"([^"]+)""#).ok()?;
    name_re
        .captures(body)?
        .get(1)
        .map(|m| m.as_str().to_string())
}

fn cargo_workspace_members(root: &Path, manifest: &Path) -> Result<Vec<String>> {
    let raw = match fs::read_to_string(manifest) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let stripped = strip_line_comments(&raw, '#');
    let Some(ws_start) = Regex::new(r"(?m)^\s*\[workspace\]\s*$")?.find(&stripped) else {
        return Ok(Vec::new());
    };
    let rest = &stripped[ws_start.end()..];
    let next_section = Regex::new(r"(?m)^\s*\[[^\]]+\]\s*$")?
        .find(rest)
        .map(|m| m.start())
        .unwrap_or(rest.len());
    let body = &rest[..next_section];
    let Some(members_match) = Regex::new(r"(?ms)^\s*members\s*=\s*\[(.*?)\]")?.captures(body)
    else {
        return Ok(Vec::new());
    };
    let inside = members_match.get(1).map(|m| m.as_str()).unwrap_or("");
    let string_re = Regex::new(r#""([^"]+)""#)?;
    let mut out = Vec::new();
    for cap in string_re.captures_iter(inside) {
        if let Some(v) = cap.get(1) {
            let s = v.as_str().trim_end_matches('/').to_string();
            // Reject glob members for v1 — `crates/*` style; we already
            // pick those up via the `crates_dir` walk above.
            if s.contains('*') || s.contains('?') {
                continue;
            }
            if s.is_empty() || s.contains("..") {
                continue;
            }
            // Skip the root self-reference (`"."`).
            if s == "." {
                continue;
            }
            // Discard outside-of-root entries.
            let full = root.join(&s);
            if !is_safe_dir(root, &full) {
                continue;
            }
            out.push(s);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn picks_up_root_binary_and_library() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"acme\"\nversion = \"0.1.0\"\n",
        );
        write(dir.path(), "src/main.rs", "fn main() {}");
        write(dir.path(), "src/lib.rs", "pub fn hi() {}");
        let seeds = RustMapper.map(dir.path()).unwrap();
        let titles: Vec<&str> = seeds.iter().map(|s| s.title.as_str()).collect();
        assert!(titles.iter().any(|t| t.contains("Rust binary `acme`")));
        assert!(titles.iter().any(|t| t.contains("Rust library `acme`")));
    }

    #[test]
    fn picks_up_src_bin_targets() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"toolkit\"\nversion = \"0.1.0\"\n",
        );
        write(dir.path(), "src/main.rs", "fn main() {}");
        write(dir.path(), "src/bin/aux.rs", "fn main() {}");
        let seeds = RustMapper.map(dir.path()).unwrap();
        let aux = seeds
            .iter()
            .find(|s| s.entry_command.as_deref() == Some("aux"))
            .expect("aux bin seeded");
        assert_eq!(aux.kind, FeatureKind::CliCommand);
        assert_eq!(aux.source, "cargo-bin");
    }

    #[test]
    fn enumerates_workspace_members() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\n",
        );
        write(
            dir.path(),
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
        );
        write(dir.path(), "crates/a/src/lib.rs", "pub fn x() {}");
        write(
            dir.path(),
            "crates/b/Cargo.toml",
            "[package]\nname = \"b\"\nversion = \"0.1.0\"\n",
        );
        write(dir.path(), "crates/b/src/main.rs", "fn main() {}");
        let seeds = RustMapper.map(dir.path()).unwrap();
        let library_a = seeds.iter().any(|s| s.title == "Rust library `a`");
        let binary_b = seeds.iter().any(|s| s.title == "Rust binary `b`");
        assert!(library_a, "member `a` library seed missing: {seeds:?}");
        assert!(binary_b, "member `b` binary seed missing: {seeds:?}");
    }

    #[test]
    fn enumerates_integration_tests() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"k\"\nversion = \"0.1.0\"\n",
        );
        write(dir.path(), "src/lib.rs", "");
        write(dir.path(), "tests/integration.rs", "#[test] fn x() {}");
        let seeds = RustMapper.map(dir.path()).unwrap();
        let test = seeds
            .iter()
            .find(|s| s.source == "rust-integration-test")
            .expect("integration test seeded");
        assert_eq!(test.entry_path, "tests/integration.rs");
        assert_eq!(test.kind, FeatureKind::TestSuite);
    }
}
