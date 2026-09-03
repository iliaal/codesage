//! Rust mapper: `src/main.rs`, `src/bin/*.rs`, `src/lib.rs`, Cargo workspace
//! members from `[workspace] members = [...]`, and integration tests in
//! `tests/*.rs`. Translates clawpatch's Rust mapper (src/mappers/rust.ts)
//! to native Rust with the same shape.

use std::path::Path;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;

use crate::mappers::shared::{
    is_safe_dir, is_safe_file, read_to_string_bounded, rel_path, sorted_read_dir,
    strip_line_comments,
};
use crate::mappers::types::{FeatureMapper, FeatureSeed, MapperContext, SeedFile};

pub struct RustMapper;

impl FeatureMapper for RustMapper {
    fn name(&self) -> &'static str {
        "rust"
    }
    fn map(&self, ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
        let root = ctx.root;
        let manifest = root.join("Cargo.toml");
        if !is_safe_file(root, &manifest) {
            return Ok(Vec::new());
        }
        let mut seeds = Vec::new();
        seed_for_package(ctx, root, &mut seeds, "cargo-root")?;
        // Workspace members.
        for member_dir in cargo_workspace_members(root, &manifest)? {
            let full = root.join(&member_dir);
            if !is_safe_dir(root, &full) {
                continue;
            }
            seed_for_package(ctx, &full, &mut seeds, "cargo-workspace-member")?;
        }
        // Conventional `crates/*` even when not declared in workspace.
        let crates_dir = root.join("crates");
        if is_safe_dir(root, &crates_dir) {
            for p in sorted_read_dir(&crates_dir) {
                if !is_safe_dir(root, &p) {
                    continue;
                }
                if !is_safe_file(root, &p.join("Cargo.toml")) {
                    continue;
                }
                seed_for_package(ctx, &p, &mut seeds, "cargo-workspace-member")?;
            }
        }
        Ok(seeds)
    }
}

fn seed_for_package(
    ctx: &MapperContext,
    pkg_dir: &Path,
    seeds: &mut Vec<FeatureSeed>,
    source: &'static str,
) -> Result<()> {
    let root = ctx.root;
    let manifest = pkg_dir.join("Cargo.toml");
    if !is_safe_file(root, &manifest) {
        return Ok(());
    }
    if ctx.excluded(&rel_path(root, &manifest)) {
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
        if ctx.allowed(&entry) {
            seeds.push(FeatureSeed {
                summary: format!("Cargo binary entrypoint at {entry}"),
                source,
                confidence: FeatureConfidence::High,
                entry_symbol: Some("main".to_string()),
                entry_command: Some(pkg_name.clone()),
                tags: vec!["rust".to_string(), "cli".to_string()],
                owned_files: lib_rs_as_owned(ctx, pkg_dir),
                context_files: cargo_toml_context(ctx, pkg_dir),
                test_prefixes: vec![rel_path(root, &pkg_dir.join("tests"))],
                ..FeatureSeed::new(
                    FeatureKind::CliCommand,
                    Language::Rust,
                    format!("Rust binary `{pkg_name}`"),
                    entry.clone(),
                )
            });
        }
    }
    // Library entrypoint: src/lib.rs (separate feature even if main.rs exists).
    let lib_rs = pkg_dir.join("src/lib.rs");
    if is_safe_file(root, &lib_rs) {
        let entry = rel_path(root, &lib_rs);
        if ctx.allowed(&entry) {
            seeds.push(FeatureSeed {
                summary: format!("Cargo library crate at {entry}"),
                source,
                confidence: FeatureConfidence::High,
                tags: vec!["rust".to_string(), "library".to_string()],
                owned_files: library_owned_files(ctx, pkg_dir, &entry),
                context_files: cargo_toml_context(ctx, pkg_dir),
                test_prefixes: vec![rel_path(root, &pkg_dir.join("tests"))],
                ..FeatureSeed::new(
                    FeatureKind::Library,
                    Language::Rust,
                    format!("Rust library `{pkg_name}`"),
                    entry.clone(),
                )
            });
        }
    }
    // Additional bins under src/bin/*.rs (one feature each).
    let bin_dir = pkg_dir.join("src/bin");
    if is_safe_dir(root, &bin_dir) {
        for p in sorted_read_dir(&bin_dir) {
            if let Some(file_name) = p.file_name().and_then(|s| s.to_str())
                && file_name.ends_with(".rs")
                && is_safe_file(root, &p)
            {
                let bin_name = file_name.trim_end_matches(".rs").to_string();
                let entry_rel = rel_path(root, &p);
                if !ctx.allowed(&entry_rel) {
                    continue;
                }
                seeds.push(FeatureSeed {
                    summary: format!("Cargo bin target at {entry_rel}"),
                    source: "cargo-bin",
                    confidence: FeatureConfidence::High,
                    entry_symbol: Some("main".to_string()),
                    entry_command: Some(bin_name.clone()),
                    tags: vec!["rust".to_string(), "cli".to_string()],
                    context_files: cargo_toml_context(ctx, pkg_dir),
                    test_prefixes: vec![rel_path(root, &pkg_dir.join("tests"))],
                    ..FeatureSeed::new(
                        FeatureKind::CliCommand,
                        Language::Rust,
                        format!("Rust binary `{bin_name}` ({pkg_name})"),
                        entry_rel,
                    )
                });
            }
        }
    }
    // Integration tests under tests/*.rs.
    let tests_dir = pkg_dir.join("tests");
    if is_safe_dir(root, &tests_dir) {
        for p in sorted_read_dir(&tests_dir) {
            if let Some(file_name) = p.file_name().and_then(|s| s.to_str())
                && file_name.ends_with(".rs")
                && is_safe_file(root, &p)
            {
                let test_name = file_name.trim_end_matches(".rs").to_string();
                let entry_rel = rel_path(root, &p);
                if !ctx.allowed(&entry_rel) {
                    continue;
                }
                seeds.push(FeatureSeed {
                    summary: format!("Integration test file at {entry_rel}"),
                    source: "rust-integration-test",
                    entry_command: Some(test_command.clone()),
                    tags: vec!["rust".to_string(), "test".to_string()],
                    ..FeatureSeed::new(
                        FeatureKind::TestSuite,
                        Language::Rust,
                        format!("Rust integration test `{test_name}` ({pkg_name})"),
                        entry_rel.clone(),
                    )
                });
            }
        }
    }
    Ok(())
}

fn library_owned_files(ctx: &MapperContext, pkg_dir: &Path, entry_path: &str) -> Vec<SeedFile> {
    use crate::mappers::shared::walk_files;
    let src_dir = pkg_dir.join("src");
    if !is_safe_dir(ctx.root, &src_dir) {
        return Vec::new();
    }
    // walk_files yields ROOT-relative paths, so the bin-exclusion prefix
    // must be root-relative too. For a workspace member at crates/foo the
    // bin dir is crates/foo/src/bin — a bare "src/bin/" would never match.
    let bin_prefix = format!("{}/", rel_path(ctx.root, &src_dir.join("bin")));
    walk_files(ctx.root, &src_dir, 10_000, ctx.excludes)
        .into_iter()
        .filter(|rel| rel.ends_with(".rs") && rel != entry_path && !rel.starts_with(&bin_prefix))
        .filter(|rel| ctx.allowed(rel))
        .map(|rel| SeedFile {
            path: rel,
            reason: "package source".to_string(),
        })
        .collect()
}

fn lib_rs_as_owned(ctx: &MapperContext, pkg_dir: &Path) -> Vec<SeedFile> {
    let root = ctx.root;
    let lib = pkg_dir.join("src/lib.rs");
    if !is_safe_file(root, &lib) {
        return Vec::new();
    }
    let rel = rel_path(root, &lib);
    if !ctx.allowed(&rel) {
        return Vec::new();
    }
    vec![SeedFile {
        path: rel,
        reason: "package library entry adjacent to binary".to_string(),
    }]
}

fn cargo_toml_context(ctx: &MapperContext, pkg_dir: &Path) -> Vec<SeedFile> {
    let root = ctx.root;
    let m = pkg_dir.join("Cargo.toml");
    if !is_safe_file(root, &m) {
        return Vec::new();
    }
    let rel = rel_path(root, &m);
    if !ctx.allowed(&rel) {
        return Vec::new();
    }
    vec![SeedFile {
        path: rel,
        reason: "package manifest".to_string(),
    }]
}

fn read_package_name(manifest_path: &Path) -> Option<String> {
    let raw = read_to_string_bounded(manifest_path).ok().flatten()?;
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
    let Ok(Some(raw)) = read_to_string_bounded(manifest) else {
        return Ok(Vec::new());
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
            if s.contains('*') || s.contains('?') {
                // Expand a simple `prefix/*` glob against the filesystem.
                // `crates/*` is already covered by the dedicated crates_dir
                // walk in `map`, so skip it here to avoid double-seeding;
                // other prefixes (`libs/*`, `packages/*`) were previously
                // dropped silently.
                if let Some(prefix) = s.strip_suffix("/*")
                    && prefix != "crates"
                    && !prefix.is_empty()
                    && !prefix.contains('*')
                    && !prefix.contains('?')
                    && !prefix.contains("..")
                {
                    let base = root.join(prefix);
                    if is_safe_dir(root, &base) {
                        for p in sorted_read_dir(&base) {
                            if p.is_dir() && p.join("Cargo.toml").is_file() && is_safe_dir(root, &p)
                            {
                                let rel = rel_path(root, &p);
                                if !rel.is_empty() {
                                    out.push(rel);
                                }
                            }
                        }
                    }
                }
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
        let seeds = RustMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
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
        let seeds = RustMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
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
        let seeds = RustMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let library_a = seeds.iter().any(|s| s.title == "Rust library `a`");
        let binary_b = seeds.iter().any(|s| s.title == "Rust binary `b`");
        assert!(library_a, "member `a` library seed missing: {seeds:?}");
        assert!(binary_b, "member `b` binary seed missing: {seeds:?}");
    }

    #[test]
    fn workspace_member_bin_excluded_from_library_owned_files() {
        // A workspace member's src/bin/*.rs must not leak into the
        // library seed's owned_files. The bin-exclusion prefix is
        // root-relative, so it has to account for the member subdir.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/foo\"]\n",
        );
        write(
            dir.path(),
            "crates/foo/Cargo.toml",
            "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n",
        );
        write(dir.path(), "crates/foo/src/lib.rs", "pub fn x() {}");
        write(dir.path(), "crates/foo/src/inner.rs", "pub fn y() {}");
        write(dir.path(), "crates/foo/src/bin/tool.rs", "fn main() {}");
        let seeds = RustMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let lib = seeds
            .iter()
            .find(|s| s.title == "Rust library `foo`")
            .expect("member library seed missing");
        let owned: Vec<&str> = lib.owned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            !owned.contains(&"crates/foo/src/bin/tool.rs"),
            "bin source leaked into library owned_files: {owned:?}"
        );
        assert!(
            owned.contains(&"crates/foo/src/inner.rs"),
            "non-bin member source missing from owned_files: {owned:?}"
        );
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
        let seeds = RustMapper
            .map(&MapperContext::for_root(dir.path()))
            .unwrap();
        let test = seeds
            .iter()
            .find(|s| s.source == "rust-integration-test")
            .expect("integration test seeded");
        assert_eq!(test.entry_path, "tests/integration.rs");
        assert_eq!(test.kind, FeatureKind::TestSuite);
    }

    #[test]
    fn map_is_deterministic_across_runs() {
        // Bin/test seed order follows sorted directory scans, never
        // readdir order: mapping the same tree twice must agree exactly.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"k\"\nversion = \"0.1.0\"\n",
        );
        write(dir.path(), "src/lib.rs", "");
        write(dir.path(), "src/bin/zeta.rs", "fn main() {}");
        write(dir.path(), "src/bin/alpha.rs", "fn main() {}");
        write(dir.path(), "tests/zeta.rs", "#[test] fn z() {}");
        write(dir.path(), "tests/alpha.rs", "#[test] fn a() {}");
        let ctx = MapperContext::for_root(dir.path());
        let first = RustMapper.map(&ctx).unwrap();
        let second = RustMapper.map(&ctx).unwrap();
        assert_eq!(format!("{first:?}"), format!("{second:?}"));
        let bins: Vec<&str> = first
            .iter()
            .filter(|s| s.source == "cargo-bin")
            .map(|s| s.entry_path.as_str())
            .collect();
        assert_eq!(bins, vec!["src/bin/alpha.rs", "src/bin/zeta.rs"]);
    }
}
