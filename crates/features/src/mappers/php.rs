//! PHP mapper: composer.json bins + scripts, PSR-4 autoload roots,
//! `**/*.phpt` test fixtures (php-src convention), `ext/<name>/config.{m4,w32}`
//! extension slices (php-src), and Laravel `routes/{web,api,console,channels}.php`
//! route extraction. Covers framework-agnostic Composer, php-src internals,
//! and Laravel without per-app config.

use std::fs;
use std::path::Path;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use regex::Regex;
use serde_json::Value;

use crate::mappers::shared::{is_safe_dir, is_safe_file, rel_path};
use crate::mappers::types::{FeatureMapper, FeatureSeed, SeedFile, SeedTest};

pub struct PhpMapper;

impl FeatureMapper for PhpMapper {
    fn name(&self) -> &'static str {
        "php"
    }
    fn map(&self, root: &Path) -> Result<Vec<FeatureSeed>> {
        let mut seeds: Vec<FeatureSeed> = Vec::new();
        // Composer.
        if let Some(composer) = read_composer(root) {
            seeds.extend(composer_seeds(root, &composer));
        }
        // php-src style ext/<name>/config.m4.
        seeds.extend(php_src_extensions(root)?);
        // Laravel route registrations.
        seeds.extend(laravel_routes(root)?);
        Ok(seeds)
    }
}

fn read_composer(root: &Path) -> Option<Value> {
    let path = root.join("composer.json");
    if !is_safe_file(root, &path) {
        return None;
    }
    let raw = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn composer_seeds(root: &Path, composer: &Value) -> Vec<FeatureSeed> {
    let mut out = Vec::new();
    // bin entries (array of paths to bin scripts).
    if let Some(bins) = composer.get("bin").and_then(|v| v.as_array()) {
        for b in bins {
            let Some(rel) = b.as_str() else { continue };
            let entry = rel.trim_start_matches("./").to_string();
            let abs = root.join(&entry);
            if !is_safe_file(root, &abs) {
                continue;
            }
            let command = entry
                .rsplit('/')
                .next()
                .unwrap_or(entry.as_str())
                .trim_end_matches(".php")
                .to_string();
            out.push(FeatureSeed {
                title: format!("Composer bin `{command}`"),
                summary: format!("composer.json bin entry at {entry}"),
                kind: FeatureKind::CliCommand,
                source: "composer-bin",
                confidence: FeatureConfidence::High,
                entry_path: entry,
                entry_symbol: None,
                entry_route: None,
                entry_command: Some(command),
                language: Language::Php,
                tags: vec!["php".to_string(), "cli".to_string()],
                owned_files: Vec::new(),
                context_files: vec![SeedFile {
                    path: "composer.json".to_string(),
                    reason: "package manifest".to_string(),
                }],
                tests: Vec::new(),
                test_prefixes: Vec::new(),
            });
        }
    }
    // PSR-4 autoload roots.
    let psr4 = composer
        .get("autoload")
        .and_then(|v| v.get("psr-4"))
        .and_then(|v| v.as_object());
    if let Some(psr4) = psr4 {
        for (ns, path_val) in psr4 {
            let path = match path_val {
                Value::String(s) => s.clone(),
                Value::Array(arr) => arr
                    .iter()
                    .find_map(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default(),
                _ => continue,
            };
            if path.is_empty() {
                continue;
            }
            let entry = path.trim_end_matches('/').to_string();
            let abs = root.join(&entry);
            if !is_safe_dir(root, &abs) {
                continue;
            }
            let ns_clean = ns.trim_end_matches('\\');
            out.push(FeatureSeed {
                title: format!("PHP namespace `{ns_clean}`"),
                summary: format!("PSR-4 autoload root at {entry}/ (namespace {ns_clean})"),
                kind: FeatureKind::Library,
                source: "composer-psr4",
                confidence: FeatureConfidence::High,
                entry_path: entry.clone(),
                entry_symbol: Some(ns_clean.to_string()),
                entry_route: None,
                entry_command: None,
                language: Language::Php,
                tags: vec!["php".to_string(), "library".to_string()],
                owned_files: Vec::new(),
                context_files: vec![SeedFile {
                    path: "composer.json".to_string(),
                    reason: "PSR-4 autoload manifest".to_string(),
                }],
                tests: Vec::new(),
                test_prefixes: vec!["tests".to_string(), "test".to_string()],
            });
        }
    }
    out
}

/// php-src convention: each PHP internals extension lives under `ext/<name>/`
/// and is signaled by the presence of `config.m4` (POSIX build) or
/// `config.w32` (Windows). We emit one feature per detected extension dir
/// and walk its `.phpt` tests as the linked test suite.
fn php_src_extensions(root: &Path) -> Result<Vec<FeatureSeed>> {
    let mut out = Vec::new();
    let ext_dir = root.join("ext");
    if !is_safe_dir(root, &ext_dir) {
        return Ok(out);
    }
    for entry in fs::read_dir(&ext_dir)?.flatten() {
        let p = entry.path();
        if !is_safe_dir(root, &p) {
            continue;
        }
        let has_m4 = is_safe_file(root, &p.join("config.m4"));
        let has_w32 = is_safe_file(root, &p.join("config.w32"));
        if !has_m4 && !has_w32 {
            continue;
        }
        // Anchor entry_path on the config file that actually exists.
        // Windows-only extensions ship only `config.w32`; pin to that
        // when `config.m4` is absent so the feature's entry_path is
        // never a missing file. Prefer `.m4` when both are present for
        // ID stability on POSIX trees.
        let config_basename = if has_m4 { "config.m4" } else { "config.w32" };
        let name = match p.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let ext_rel = rel_path(root, &p);
        let tests_dir = p.join("tests");
        let tests: Vec<SeedTest> = if is_safe_dir(root, &tests_dir) {
            list_phpt_files(root, &tests_dir, 20)
                .into_iter()
                .map(|path| SeedTest {
                    path,
                    command: Some(format!("make test TESTS=ext/{name}/tests")),
                })
                .collect()
        } else {
            Vec::new()
        };
        // Attach the extension's top-level .c/.h sources as owned. Without
        // this, `find_feature ext/curl/interface.c` returns empty even when
        // the curl extension is mapped — the ext mapper only had config.m4
        // as the entry. Capped at 40 files so an extension with hundreds of
        // sources (mbstring, intl) doesn't blow up the bundle.
        let owned_files = list_c_sources(root, &p, 40);
        out.push(FeatureSeed {
            title: format!("PHP extension `{name}` (php-src)"),
            summary: format!("php-src internals extension at {ext_rel}/"),
            kind: FeatureKind::Library,
            source: "php-ext",
            confidence: FeatureConfidence::High,
            entry_path: format!("{ext_rel}/{config_basename}"),
            entry_symbol: None,
            entry_route: None,
            entry_command: None,
            language: Language::Php,
            tags: vec![
                "php".to_string(),
                "php-src".to_string(),
                "extension".to_string(),
            ],
            owned_files,
            context_files: vec![SeedFile {
                path: format!("{ext_rel}/{config_basename}"),
                reason: "extension build config".to_string(),
            }],
            tests,
            test_prefixes: vec![format!("{ext_rel}/tests")],
        });
    }
    Ok(out)
}

/// Walk `dir` (one level deep, no recursion into `tests/` or subdirs) and
/// return repo-relative paths of every `.c` / `.h` source. Skips files that
/// look generated (`*_arginfo.h` is autogenerated but agents do edit it; we
/// keep it). Used by the php-src ext mapper so `find_feature
/// ext/curl/interface.c` returns the curl extension rather than empty.
fn list_c_sources(root: &Path, dir: &Path, max: usize) -> Vec<SeedFile> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        if out.len() >= max {
            break;
        }
        let p = entry.path();
        if let Some(name) = p.file_name().and_then(|s| s.to_str())
            && (name.ends_with(".c") || name.ends_with(".h") || name.ends_with(".cpp"))
            && is_safe_file(root, &p)
        {
            out.push(SeedFile {
                path: rel_path(root, &p),
                reason: "extension source".to_string(),
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn list_phpt_files(root: &Path, dir: &Path, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        if out.len() >= max {
            break;
        }
        let p = entry.path();
        if let Some(name) = p.file_name().and_then(|s| s.to_str())
            && name.ends_with(".phpt")
            && is_safe_file(root, &p)
        {
            out.push(rel_path(root, &p));
        }
    }
    out.sort();
    out
}

/// Laravel's `routes/{web,api,console,channels}.php` files contain a flat
/// list of `Route::<verb>(...)` registrations. We extract each registration
/// as its own feature so an agent can ask "what handles POST /api/login?"
/// and get the right feature back.
fn laravel_routes(root: &Path) -> Result<Vec<FeatureSeed>> {
    let mut out = Vec::new();
    let routes_dir = root.join("routes");
    if !is_safe_dir(root, &routes_dir) {
        return Ok(out);
    }
    let verb_re = Regex::new(
        r#"(?m)Route::(get|post|put|patch|delete|options|any|match)\s*\(\s*(?:\[[^\]]*\]\s*,\s*)?['"]([^'"]+)['"]"#,
    )?;
    for file in ["web.php", "api.php", "console.php", "channels.php"] {
        let path = routes_dir.join(file);
        if !is_safe_file(root, &path) {
            continue;
        }
        let rel = rel_path(root, &path);
        let raw = fs::read_to_string(&path).unwrap_or_default();
        for cap in verb_re.captures_iter(&raw) {
            let verb = cap
                .get(1)
                .map(|m| m.as_str().to_uppercase())
                .unwrap_or_default();
            let pattern = cap
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if verb.is_empty() || pattern.is_empty() {
                continue;
            }
            let route = format!("{verb} {pattern}");
            out.push(FeatureSeed {
                title: format!("Laravel route `{route}`"),
                summary: format!("Route registered in {rel}"),
                kind: FeatureKind::Route,
                source: "laravel-route",
                confidence: FeatureConfidence::High,
                entry_path: rel.clone(),
                entry_symbol: None,
                entry_route: Some(route.clone()),
                entry_command: None,
                language: Language::Php,
                tags: vec![
                    "php".to_string(),
                    "framework:laravel".to_string(),
                    "route".to_string(),
                ],
                owned_files: Vec::new(),
                context_files: Vec::new(),
                tests: Vec::new(),
                test_prefixes: vec!["tests/Feature".to_string(), "tests".to_string()],
            });
        }
    }
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
    fn composer_bin_emits_cli_command_seed() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "composer.json",
            r#"{"name":"acme/cli","bin":["bin/acme"]}"#,
        );
        write(dir.path(), "bin/acme", "#!/usr/bin/env php\n<?php\n");
        let seeds = PhpMapper.map(dir.path()).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.entry_command.as_deref() == Some("acme"))
            .expect("composer-bin seed");
        assert_eq!(s.kind, FeatureKind::CliCommand);
        assert_eq!(s.source, "composer-bin");
    }

    #[test]
    fn psr4_autoload_emits_library_seed() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "composer.json",
            r#"{"autoload":{"psr-4":{"Acme\\":"src/"}}}"#,
        );
        write(
            dir.path(),
            "src/Foo.php",
            "<?php\nnamespace Acme;\nclass Foo {}\n",
        );
        let seeds = PhpMapper.map(dir.path()).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "composer-psr4")
            .expect("psr-4 seed");
        assert_eq!(s.kind, FeatureKind::Library);
        assert_eq!(s.entry_symbol.as_deref(), Some("Acme"));
    }

    #[test]
    fn php_src_extension_detected_by_config_m4() {
        let dir = tempdir().unwrap();
        // Simulate php-src ext/iconv/.
        write(dir.path(), "ext/iconv/config.m4", "PHP_ARG_WITH(iconv,,)\n");
        write(
            dir.path(),
            "ext/iconv/tests/bug001.phpt",
            "--TEST--\nbug\n--FILE--\n<?php\n?>",
        );
        let seeds = PhpMapper.map(dir.path()).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "php-ext")
            .expect("php-ext seed");
        assert!(s.title.contains("iconv"));
        assert!(s.tests.iter().any(|t| t.path.contains("bug001.phpt")));
        // entry_path must point at the file that actually exists.
        assert_eq!(s.entry_path, "ext/iconv/config.m4");
    }

    #[test]
    fn windows_only_php_ext_uses_config_w32_entry() {
        // Windows-only extensions ship only `config.w32`; the feature's
        // entry_path must anchor on the file that actually exists.
        let dir = tempdir().unwrap();
        write(dir.path(), "ext/wincache/config.w32", "// MSBuild config\n");
        let seeds = PhpMapper.map(dir.path()).unwrap();
        let s = seeds
            .iter()
            .find(|s| s.source == "php-ext")
            .expect("php-ext seed");
        assert_eq!(
            s.entry_path, "ext/wincache/config.w32",
            "windows-only ext should anchor on config.w32, got {:?}",
            s.entry_path
        );
    }

    #[test]
    fn laravel_route_extracts_verb_and_path() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "routes/web.php",
            r#"<?php
Route::get('/', fn() => 'home');
Route::post('/api/login', [LoginController::class, 'store']);
"#,
        );
        let seeds = PhpMapper.map(dir.path()).unwrap();
        let routes: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "laravel-route")
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert!(routes.iter().any(|r| r.starts_with("GET ")));
        assert!(routes.contains(&"POST /api/login"));
    }
}
