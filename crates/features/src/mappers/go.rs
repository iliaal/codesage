//! Go mapper: filesystem-only package discovery + per-package slices.
//!
//! Each directory containing `.go` files maps to one feature seed:
//! - `package main` in `cmd/<name>/` → `go-cmd` cli-command (entry = `main.go`)
//! - `package main` at the repo root → `go-root-package` cli-command
//! - any other package → `go-package` library (`go-internal-package` if under
//!   `internal/`)
//!
//! Per-package tests (`*_test.go`) attach to the feature with a scoped command
//! (`go test ./<dir>/...`); generated files (`*.pb.go`, `*_gen.go`,
//! `*_generated.go`, `*.sql.go`, `*_sqlc.go`, plus `Code generated ... DO NOT
//! EDIT` headers) get segregated into context with a `"generated go file"`
//! reason so an agent skips editing them; same-repo imports up to 24 refs
//! attach as context with reason `"imported package <import_path>"`.
//!
//! Ported from clawpatch's `go.ts`. Three anti-patterns explicitly skipped:
//! `go list` toolchain dependency (we stay hermetic — filesystem only),
//! hardcoded `["user-input","filesystem","process-exec","network"]` boundaries
//! on every binary (would override codesage's per-file derivation), and the
//! unscoped `"go test ./..."` literal (we scope per package).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;

use crate::mappers::shared::{is_safe_dir, is_safe_file, should_skip, walk_files};
use crate::mappers::types::{FeatureMapper, FeatureSeed, MapperContext, SeedFile, SeedTest};

pub struct GoMapper;

/// One Go package discovered on the filesystem. `dir_rel` is the repo-
/// relative directory (`""` for the repo root); `import_path` is the module
/// path concatenated with `dir_rel`; `name` is the `package <name>` declaration.
#[derive(Debug, Clone)]
struct GoPackage {
    dir_rel: String,
    import_path: String,
    name: String,
}

#[derive(Debug, Default)]
struct GoPackageFiles {
    owned: Vec<String>,
    tests: Vec<String>,
    generated: Vec<String>,
}

impl FeatureMapper for GoMapper {
    fn name(&self) -> &'static str {
        "go"
    }

    fn map(&self, ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
        let root = ctx.root;
        let go_mod = root.join("go.mod");
        if !is_safe_file(root, &go_mod) {
            return Ok(Vec::new());
        }
        let module_path = read_module_path(&go_mod);
        let packages = discover_packages(ctx, module_path.as_deref())?;
        let by_import: HashMap<String, GoPackage> = packages
            .iter()
            .map(|p| (p.import_path.clone(), p.clone()))
            .collect();

        let mut seeds = Vec::new();
        for pkg in &packages {
            let files = collect_package_files(ctx, &pkg.dir_rel)?;
            if files.owned.is_empty() {
                continue;
            }
            let imported_context =
                collect_import_context(ctx, module_path.as_deref(), &by_import, &files.owned)?;
            seeds.push(make_seed(pkg, files, imported_context));
        }
        seeds.retain(|s| !ctx.excluded(&s.entry_path));
        Ok(seeds)
    }
}

// ---- Discovery ----------------------------------------------------------

fn read_module_path(go_mod: &Path) -> Option<String> {
    let raw = fs::read_to_string(go_mod).ok()?;
    let re = Regex::new(r"(?m)^\s*module\s+(\S+)").ok()?;
    re.captures(&raw)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

fn discover_packages(ctx: &MapperContext, module_path: Option<&str>) -> Result<Vec<GoPackage>> {
    let root = ctx.root;
    // Walk every .go file in the repo, gated by gitignore + project excludes,
    // and bucket by parent directory. 20k file cap is enough for very large
    // Go repos (e.g. kubernetes is ~15k); cuts off worst-case walks.
    let mut by_dir: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for rel in walk_files(root, root, 20_000, ctx.excludes) {
        if !rel.ends_with(".go") {
            continue;
        }
        let (dir, _) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
        if is_skipped_go_dir(root, dir) {
            continue;
        }
        by_dir.entry(dir.to_string()).or_default().push(rel.clone());
    }

    let mut packages: Vec<GoPackage> = Vec::new();
    for dir_rel in by_dir.keys() {
        let pkg_name = match read_go_package_name(root, dir_rel) {
            Some(name) => name,
            None => continue,
        };
        let import_path = match module_path {
            Some(mp) if !dir_rel.is_empty() => format!("{mp}/{dir_rel}"),
            Some(mp) => mp.to_string(),
            None => String::new(),
        };
        packages.push(GoPackage {
            dir_rel: dir_rel.clone(),
            import_path,
            name: pkg_name,
        });
    }
    Ok(packages)
}

/// Skip dirs that are not part of the module: any `vendor/`, `testdata/`,
/// dot-prefixed, or underscore-prefixed segment, plus any descendant of a
/// nested `go.mod` (which is its own module, not ours).
fn is_skipped_go_dir(root: &Path, dir_rel: &str) -> bool {
    if should_skip(dir_rel) {
        return true;
    }
    for part in dir_rel.split('/').filter(|p| !p.is_empty()) {
        if part == "vendor" || part == "testdata" {
            return true;
        }
        if part.starts_with('.') || part.starts_with('_') {
            return true;
        }
    }
    // Nested `go.mod` walks up from `dir_rel` toward (but not including) the
    // root. Any ancestor with a `go.mod` is a sub-module.
    let parts: Vec<&str> = dir_rel.split('/').filter(|p| !p.is_empty()).collect();
    for i in 1..=parts.len() {
        let candidate: String = parts[..i].join("/");
        if is_safe_file(root, &root.join(&candidate).join("go.mod")) {
            return true;
        }
    }
    false
}

fn read_go_package_name(root: &Path, dir_rel: &str) -> Option<String> {
    let dir = if dir_rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(dir_rel)
    };
    if !is_safe_dir(root, &dir) {
        return None;
    }
    let pkg_re = Regex::new(r"(?m)^\s*package\s+([A-Za-z_][A-Za-z0-9_]*)").ok()?;
    let entries = fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.ends_with(".go") || name.ends_with("_test.go") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(cap) = pkg_re.captures(&raw)
            && let Some(m) = cap.get(1)
        {
            return Some(m.as_str().to_string());
        }
    }
    None
}

// ---- Per-package file classification -----------------------------------

fn collect_package_files(ctx: &MapperContext, dir_rel: &str) -> Result<GoPackageFiles> {
    let root = ctx.root;
    let dir = if dir_rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(dir_rel)
    };
    let mut files = GoPackageFiles::default();
    if !is_safe_dir(root, &dir) {
        return Ok(files);
    }
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(files);
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if !name.ends_with(".go") {
            continue;
        }
        let rel = if dir_rel.is_empty() {
            name.clone()
        } else {
            format!("{dir_rel}/{name}")
        };
        // Honor project excludes inside the per-package classifier too. If
        // an excluded file happened to sort first, it would have ended up
        // as entry_path and the post-map `seeds.retain(...excluded)` would
        // drop the whole package — losing every non-excluded sibling.
        if !ctx.allowed(&rel) {
            continue;
        }
        if name.ends_with("_test.go") {
            files.tests.push(rel);
            continue;
        }
        if is_generated_go_file(&entry.path(), &name) {
            files.generated.push(rel);
            continue;
        }
        files.owned.push(rel);
    }
    files.owned.sort();
    files.tests.sort();
    files.generated.sort();
    Ok(files)
}

fn is_generated_go_file(abs: &Path, file_name: &str) -> bool {
    // Filename suffix patterns. The clawpatch list plus codesage-specific
    // ones (sqlc, gen). Ordered most-common-first so the regex short-
    // circuits earlier on the typical case.
    let suffix_re = Regex::new(r"\.pb\.go$|_gen\.go$|_generated\.go$|\.sql\.go$|_sqlc\.go$")
        .expect("static regex");
    if suffix_re.is_match(file_name) {
        return true;
    }
    // Bounded header sniff: take the first ~2 KB of the file rather than
    // reading it whole. go-bindata can emit MB-scale generated files
    // without a recognized suffix; reading the entire thing just to check
    // the comment header allocates the full buffer. Drop bytes past the
    // last valid UTF-8 char boundary so a multibyte sequence split at the
    // cap doesn't reject a real marker. ASCII files (the common case) are
    // unaffected.
    let mut head_bytes = Vec::with_capacity(2_000);
    {
        use std::io::Read;
        let Ok(file) = std::fs::File::open(abs) else {
            return false;
        };
        let mut take = file.take(2_000);
        if take.read_to_end(&mut head_bytes).is_err() {
            return false;
        }
    }
    let mut end = head_bytes.len();
    let head = loop {
        match std::str::from_utf8(&head_bytes[..end]) {
            Ok(s) => break s,
            Err(e) => {
                end = e.valid_up_to();
                if end == 0 {
                    return false;
                }
            }
        }
    };
    let header_re = Regex::new(r"(?i)Code generated .* DO NOT EDIT\.|DO NOT EDIT: generated")
        .expect("static regex");
    header_re.is_match(head)
}

// ---- Same-repo import context (capped) ---------------------------------

const IMPORT_CONTEXT_CAP_DEFAULT: usize = 24;

// Env-overridable for per-project tuning (`CODESAGE_GO_IMPORT_CONTEXT_CAP`).
// Cached on first read; values < 1 fall back to the default.
fn import_context_cap() -> usize {
    use std::sync::OnceLock;
    static CACHE: OnceLock<usize> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("CODESAGE_GO_IMPORT_CONTEXT_CAP")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(IMPORT_CONTEXT_CAP_DEFAULT)
    })
}

fn collect_import_context(
    ctx: &MapperContext,
    module_path: Option<&str>,
    by_import: &HashMap<String, GoPackage>,
    owned_files: &[String],
) -> Result<Vec<SeedFile>> {
    let Some(module) = module_path else {
        return Ok(Vec::new());
    };
    let root = ctx.root;
    let module_prefix = format!("{module}/");
    let mut refs: Vec<SeedFile> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for file in owned_files {
        let Ok(raw) = fs::read_to_string(root.join(file)) else {
            continue;
        };
        for imported in extract_go_imports(&raw) {
            if imported != module && !imported.starts_with(&module_prefix) {
                continue;
            }
            let Some(pkg) = by_import.get(&imported) else {
                continue;
            };
            let pkg_files = collect_package_files(ctx, &pkg.dir_rel)?;
            for ctx_file in pkg_files.owned {
                if seen.contains(&ctx_file) {
                    continue;
                }
                seen.insert(ctx_file.clone());
                refs.push(SeedFile {
                    path: ctx_file,
                    reason: format!("imported package {imported}"),
                });
                if refs.len() >= import_context_cap() {
                    return Ok(refs);
                }
            }
        }
    }
    Ok(refs)
}

fn extract_go_imports(source: &str) -> Vec<String> {
    let mut imports: BTreeSet<String> = BTreeSet::new();
    // Single-line: `import "<path>"` and `import _ "<path>"`.
    let single = Regex::new(r#"(?m)^\s*import\s+(?:[._A-Za-z][A-Za-z0-9_.]*\s+)?"([^"]+)""#)
        .expect("static regex");
    for cap in single.captures_iter(source) {
        if let Some(m) = cap.get(1) {
            imports.insert(m.as_str().to_string());
        }
    }
    // Block form: `import ( "<path>"; "<other>" )`. Capture the block body
    // then extract every quoted path inside.
    let block = Regex::new(r"(?s)import\s*\(([^)]*)\)").expect("static regex");
    let inner = Regex::new(r#""([^"]+)""#).expect("static regex");
    for cap in block.captures_iter(source) {
        let Some(body) = cap.get(1) else {
            continue;
        };
        for inner_cap in inner.captures_iter(body.as_str()) {
            if let Some(m) = inner_cap.get(1) {
                imports.insert(m.as_str().to_string());
            }
        }
    }
    imports.into_iter().collect()
}

// ---- Seed construction --------------------------------------------------

fn make_seed(
    pkg: &GoPackage,
    files: GoPackageFiles,
    imported_context: Vec<SeedFile>,
) -> FeatureSeed {
    let is_main = pkg.name == "main";
    let command_name = if is_main {
        command_name_for_dir(&pkg.dir_rel)
    } else {
        None
    };
    let display_name = command_name
        .clone()
        .unwrap_or_else(|| display_name_for_package(pkg));

    let entry_path = files
        .owned
        .iter()
        .find(|f| *f == "main.go" || f.ends_with("/main.go"))
        .cloned()
        .or_else(|| files.owned.first().cloned())
        .unwrap_or_else(|| pkg.dir_rel.clone());

    let test_command = scoped_test_command(&pkg.dir_rel);
    let tests: Vec<SeedTest> = files
        .tests
        .iter()
        .map(|path| SeedTest {
            path: path.clone(),
            command: Some(test_command.clone()),
        })
        .collect();

    let mut context_files: Vec<SeedFile> = files
        .tests
        .iter()
        .map(|path| SeedFile {
            path: path.clone(),
            reason: "go package test".to_string(),
        })
        .collect();
    for f in files.generated {
        context_files.push(SeedFile {
            path: f,
            reason: "generated go file".to_string(),
        });
    }
    context_files.extend(imported_context);

    let owned_files: Vec<SeedFile> = files
        .owned
        .iter()
        .filter(|f| **f != entry_path)
        .map(|f| SeedFile {
            path: f.clone(),
            reason: "go package source".to_string(),
        })
        .collect();

    let source = if command_name.is_some() {
        "go-cmd"
    } else if pkg.dir_rel.is_empty() {
        "go-root-package"
    } else if pkg.dir_rel.starts_with("internal/") {
        "go-internal-package"
    } else {
        "go-package"
    };

    let kind = if is_main {
        FeatureKind::CliCommand
    } else {
        FeatureKind::Library
    };

    let mut tags = vec!["go".to_string()];
    tags.push(if is_main { "cli" } else { "package" }.to_string());

    let title = if is_main {
        format!("Go binary `{display_name}`")
    } else {
        format!("Go package `{display_name}`")
    };
    let summary = if is_main {
        format!(
            "Go {} package at {} with {} source file(s)",
            display_name,
            if pkg.dir_rel.is_empty() {
                ".".to_string()
            } else {
                pkg.dir_rel.clone()
            },
            files.owned.len()
        )
    } else {
        format!(
            "Go package {} ({}) with {} source file(s)",
            pkg.import_path,
            pkg.name,
            files.owned.len()
        )
    };

    FeatureSeed {
        title,
        summary,
        kind,
        source,
        confidence: FeatureConfidence::Medium,
        entry_path,
        entry_symbol: if is_main {
            Some("main".to_string())
        } else {
            None
        },
        entry_route: None,
        entry_command: command_name,
        // Scoped `go test ./<dir>/...` is the runnable test invocation for
        // every Go package feature. Lives on test_command so feature IDs
        // stay stable when test config evolves; lives separately from any
        // per-test SeedTest.command since the FeatureFileRef table doesn't
        // carry per-file command columns.
        test_command: if files.tests.is_empty() {
            None
        } else {
            Some(test_command.clone())
        },
        language: Language::Go,
        tags,
        owned_files,
        context_files,
        tests,
        test_prefixes: if pkg.dir_rel.is_empty() {
            Vec::new()
        } else {
            vec![pkg.dir_rel.clone()]
        },
    }
}

/// Scoped test command for a package: `go test ./<dir>/...` (or `./...` for
/// the repo root). The unscoped clawpatch literal (`go test ./...` for every
/// feature) was rejected during validation — it tells the agent nothing the
/// default doesn't already convey.
fn scoped_test_command(dir_rel: &str) -> String {
    if dir_rel.is_empty() {
        "go test ./...".to_string()
    } else {
        format!("go test ./{dir_rel}/...")
    }
}

fn command_name_for_dir(dir_rel: &str) -> Option<String> {
    let parts: Vec<&str> = dir_rel.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() == 2 && parts[0] == "cmd" {
        return Some(parts[1].to_string());
    }
    None
}

fn display_name_for_package(pkg: &GoPackage) -> String {
    if pkg.dir_rel.is_empty() {
        return pkg.name.clone();
    }
    pkg.dir_rel
        .rsplit_once('/')
        .map(|(_, tail)| tail.to_string())
        .unwrap_or_else(|| pkg.dir_rel.clone())
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
    fn cmd_subdir_main_yields_go_cmd_seed() {
        let dir = tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/acme\n");
        write(
            dir.path(),
            "cmd/server/main.go",
            "package main\nfunc main() {}\n",
        );
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "go-cmd")
            .expect("go-cmd seed");
        assert_eq!(s.entry_command.as_deref(), Some("server"));
        assert_eq!(s.kind, FeatureKind::CliCommand);
    }

    #[test]
    fn root_main_go_only_when_package_main() {
        let dir = tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/app\n");
        write(dir.path(), "main.go", "package main\nfunc main() {}\n");
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(seeds.iter().any(|s| s.source == "go-root-package"));
    }

    #[test]
    fn root_main_go_skipped_when_not_package_main() {
        let dir = tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/lib\n");
        // A non-main package at the repo root is still a package — it just
        // shouldn't be a cli-command. The mapper should produce a library
        // seed, not a cli-command seed.
        write(dir.path(), "foo.go", "package lib\n");
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(!seeds.iter().any(|s| s.kind == FeatureKind::CliCommand));
        assert!(seeds.iter().any(|s| s.kind == FeatureKind::Library));
    }

    #[test]
    fn no_seeds_without_go_mod() {
        let dir = tempdir().unwrap();
        // No go.mod — not a Go module, skip entirely.
        write(dir.path(), "stray.go", "package stray\n");
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(seeds.is_empty());
    }

    #[test]
    fn library_package_emits_library_seed() {
        let dir = tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/acme\n");
        write(
            dir.path(),
            "pkg/util/util.go",
            "package util\nfunc Helper() {}\n",
        );
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "go-package")
            .expect("go-package seed");
        assert_eq!(s.kind, FeatureKind::Library);
        assert_eq!(s.entry_path, "pkg/util/util.go");
    }

    #[test]
    fn internal_package_tagged_separately() {
        let dir = tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/acme\n");
        write(
            dir.path(),
            "internal/auth/auth.go",
            "package auth\nfunc Login() {}\n",
        );
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(seeds.iter().any(|s| s.source == "go-internal-package"));
    }

    #[test]
    fn tests_attach_with_scoped_command() {
        let dir = tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/acme\n");
        write(
            dir.path(),
            "pkg/util/util.go",
            "package util\nfunc Helper() {}\n",
        );
        write(
            dir.path(),
            "pkg/util/util_test.go",
            "package util\nimport \"testing\"\nfunc TestHelper(t *testing.T) {}\n",
        );
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.entry_path == "pkg/util/util.go")
            .expect("util package seed");
        assert_eq!(s.tests.len(), 1);
        assert_eq!(s.tests[0].path, "pkg/util/util_test.go");
        assert_eq!(
            s.tests[0].command.as_deref(),
            Some("go test ./pkg/util/..."),
            "test command must be package-scoped, not unscoped ./..."
        );
    }

    #[test]
    fn generated_files_segregated_into_context() {
        let dir = tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/acme\n");
        write(
            dir.path(),
            "pkg/api/api.go",
            "package api\nfunc Real() {}\n",
        );
        write(
            dir.path(),
            "pkg/api/messages.pb.go",
            "package api\n// generated grpc\n",
        );
        write(
            dir.path(),
            "pkg/api/header_marker.go",
            "// Code generated by hand-edit-me-not. DO NOT EDIT.\npackage api\nfunc Marker() {}\n",
        );
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.entry_path == "pkg/api/api.go")
            .expect("api package seed");
        // The .pb.go suffix-matched file and the DO-NOT-EDIT-header file are
        // both in context, not owned. Owned only has the hand-written file.
        let context_paths: Vec<&str> = s.context_files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            context_paths.contains(&"pkg/api/messages.pb.go"),
            "messages.pb.go should be in context, got {context_paths:?}"
        );
        assert!(
            context_paths.contains(&"pkg/api/header_marker.go"),
            "header-marked generated file should be in context, got {context_paths:?}"
        );
        let owned_paths: Vec<&str> = s.owned_files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            !owned_paths.iter().any(|p| p.ends_with(".pb.go")),
            ".pb.go should not be owned, got {owned_paths:?}"
        );
    }

    #[test]
    fn vendor_and_testdata_dirs_skipped() {
        let dir = tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/acme\n");
        write(
            dir.path(),
            "vendor/dep/thing.go",
            "package dep\nfunc Vendored() {}\n",
        );
        write(
            dir.path(),
            "pkg/util/testdata/fixture.go",
            "package util\nfunc Fixture() {}\n",
        );
        write(
            dir.path(),
            "pkg/util/util.go",
            "package util\nfunc Real() {}\n",
        );
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let entry_paths: Vec<&str> = seeds.iter().map(|s| s.entry_path.as_str()).collect();
        assert!(
            !entry_paths
                .iter()
                .any(|p| p.starts_with("vendor/") || p.contains("/testdata/")),
            "vendor/ or testdata/ leaked, got {entry_paths:?}"
        );
        assert!(
            entry_paths.contains(&"pkg/util/util.go"),
            "expected pkg/util/util.go seed, got {entry_paths:?}"
        );
    }

    #[test]
    fn nested_go_mod_subtree_skipped() {
        // A sub-directory with its own go.mod is its own module; don't index
        // it as part of the parent. This is the common case for repos that
        // vendor a tools/ sub-module.
        let dir = tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/acme\n");
        write(
            dir.path(),
            "pkg/util/util.go",
            "package util\nfunc Real() {}\n",
        );
        write(
            dir.path(),
            "tools/go.mod",
            "module example.com/acme/tools\n",
        );
        write(
            dir.path(),
            "tools/gen/main.go",
            "package main\nfunc main() {}\n",
        );
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let entry_paths: Vec<&str> = seeds.iter().map(|s| s.entry_path.as_str()).collect();
        assert!(
            !entry_paths.iter().any(|p| p.starts_with("tools/")),
            "nested-module tools/ leaked, got {entry_paths:?}"
        );
        assert!(entry_paths.contains(&"pkg/util/util.go"));
    }

    #[test]
    fn same_repo_import_attaches_context_files() {
        let dir = tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/acme\n");
        write(
            dir.path(),
            "internal/auth/auth.go",
            "package auth\n\nfunc Login() {}\n",
        );
        write(
            dir.path(),
            "cmd/server/main.go",
            "package main\n\nimport (\n  \"example.com/acme/internal/auth\"\n)\n\nfunc main() { auth.Login() }\n",
        );
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let server = seeds
            .iter()
            .find(|s| s.source == "go-cmd")
            .expect("server seed");
        let imported: Vec<&str> = server
            .context_files
            .iter()
            .filter(|f| f.reason.starts_with("imported package "))
            .map(|f| f.path.as_str())
            .collect();
        assert!(
            imported.contains(&"internal/auth/auth.go"),
            "expected auth.go in import context, got {imported:?}"
        );
    }

    #[test]
    fn import_context_bounded_at_24() {
        let dir = tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/acme\n");
        // Create 30 internal packages, each with 1 file, all imported by main.
        let mut imports = String::new();
        for i in 0..30 {
            write(
                dir.path(),
                &format!("internal/pkg{i}/pkg{i}.go"),
                &format!("package pkg{i}\n\nfunc F() {{}}\n"),
            );
            imports.push_str(&format!("  \"example.com/acme/internal/pkg{i}\"\n"));
        }
        let main_src = format!("package main\n\nimport (\n{imports})\n\nfunc main() {{}}\n");
        write(dir.path(), "cmd/server/main.go", &main_src);
        let seeds = GoMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let server = seeds
            .iter()
            .find(|s| s.source == "go-cmd")
            .expect("server seed");
        let imported_count = server
            .context_files
            .iter()
            .filter(|f| f.reason.starts_with("imported package "))
            .count();
        assert_eq!(
            imported_count, 24,
            "import context must be capped at 24, got {imported_count}"
        );
    }

    #[test]
    fn excluded_file_does_not_drop_whole_package() {
        // Regression: prior to the fix, `collect_package_files` ignored
        // ctx.excludes. If the alphabetically-first .go file in a dir was
        // excluded, it became entry_path; then the post-map
        // `seeds.retain(|s| !ctx.excluded(&s.entry_path))` dropped the
        // entire package, losing every non-excluded sibling.
        use globset::{Glob, GlobSetBuilder};
        let dir = tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/acme\n");
        write(
            dir.path(),
            "pkg/util/a_excluded.go",
            "package util\nfunc Excluded() {}\n",
        );
        write(
            dir.path(),
            "pkg/util/z_kept.go",
            "package util\nfunc Kept() {}\n",
        );
        let mut builder = GlobSetBuilder::new();
        builder.add(Glob::new("**/a_excluded.go").unwrap());
        let excludes = builder.build().unwrap();
        let ctx = MapperContext {
            root: dir.path(),
            excludes: Some(&excludes),
        };
        let seeds = GoMapper.map(&ctx).unwrap();
        let entry_paths: Vec<&str> = seeds.iter().map(|s| s.entry_path.as_str()).collect();
        assert!(
            entry_paths.contains(&"pkg/util/z_kept.go"),
            "package was dropped when excluded file sorted first: got {entry_paths:?}"
        );
        assert!(
            !entry_paths.contains(&"pkg/util/a_excluded.go"),
            "excluded file leaked into seed: {entry_paths:?}"
        );
    }
}
