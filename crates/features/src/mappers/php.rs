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
use crate::mappers::types::{FeatureMapper, FeatureSeed, MapperContext, SeedFile, SeedTest};

pub struct PhpMapper;

impl FeatureMapper for PhpMapper {
    fn name(&self) -> &'static str {
        "php"
    }
    fn map(&self, ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
        let root = ctx.root;
        let mut seeds: Vec<FeatureSeed> = Vec::new();
        if let Some(composer) = read_composer(root) {
            seeds.extend(composer_seeds(root, &composer));
        }
        seeds.extend(php_src_extensions(root)?);
        seeds.extend(php_extension_at_root(root)?);
        let routes = parse_laravel_routes(root)?;
        seeds.extend(laravel_route_seeds(&routes));
        // Laravel application-layer slices: controllers (resolved back to
        // their routes via the parsed route table), form requests, Artisan
        // commands. These add coarser feature_ids than the
        // route-per-registration shape so agents can ask "what slice owns
        // app/Http/Controllers/UserController.php?" without paging through
        // every route registration.
        if is_laravel_project(root) {
            seeds.extend(laravel_controllers(root, &routes)?);
            seeds.extend(laravel_form_requests(root)?);
            seeds.extend(laravel_artisan_commands(root)?);
        }
        // Apply project excludes uniformly on the way out so framework
        // detectors don't need to thread `ctx` through every helper.
        seeds.retain(|s| ctx.allowed(&s.entry_path));
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
                test_command: None,
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
                test_command: None,
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
            test_command: None,
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

/// PHP extensions distributed via Composer/PIE live at the repo root
/// rather than under `ext/<name>/` (the php-src convention). Detect them
/// by the combination of `config.m4` / `config.w32` AND a `php_<name>.h`
/// header at the project root, then emit:
///
/// 1. **One umbrella** `php-ext-root` seed for the extension as a whole.
///    Entry = the build config file, owned = the public header + main
///    `<name>.c|.cpp` if either exists.
///
/// 2. **One per-module** `php-ext-module` seed for each
///    `<name>_<part>.{c,cpp,cc}` at the root, with the sibling header
///    (same stem, `.h`) attached as owned. On extensions that organize
///    themselves by component file (fastchart's per-chart-type files,
///    mdparser's per-renderer files), this is the granularity that
///    matches how agents reason about the code.
///
/// Skips repos that have `config.m4` but no `php_<name>.h` (generic
/// autotools projects, the PHP interpreter itself) so the heuristic
/// doesn't false-positive on non-extension trees.
fn php_extension_at_root(root: &Path) -> Result<Vec<FeatureSeed>> {
    let has_config_m4 = is_safe_file(root, &root.join("config.m4"));
    let has_config_w32 = is_safe_file(root, &root.join("config.w32"));
    if !has_config_m4 && !has_config_w32 {
        return Ok(Vec::new());
    }
    let Some(ext_name) = detect_root_extension_name(root) else {
        return Ok(Vec::new());
    };
    let entry_config = if has_config_m4 {
        "config.m4"
    } else {
        "config.w32"
    };
    let header_path = format!("php_{ext_name}.h");
    let mut seeds = Vec::new();

    // Umbrella feature: the extension as a whole.
    let mut umbrella_owned: Vec<SeedFile> = Vec::new();
    for cand in [
        format!("{ext_name}.c"),
        format!("{ext_name}.cpp"),
        format!("{ext_name}.cc"),
    ] {
        if is_safe_file(root, &root.join(&cand)) {
            umbrella_owned.push(SeedFile {
                path: cand,
                reason: "module entry source".to_string(),
            });
            break;
        }
    }
    if is_safe_file(root, &root.join(&header_path)) {
        umbrella_owned.push(SeedFile {
            path: header_path.clone(),
            reason: "extension public header".to_string(),
        });
    }
    let mut umbrella_ctx: Vec<SeedFile> = Vec::new();
    for cand in [
        "composer.json".to_string(),
        format!("{ext_name}.stub.php"),
        "config.w32".to_string(),
        "config.m4".to_string(),
    ] {
        if cand == entry_config {
            continue;
        }
        if is_safe_file(root, &root.join(&cand)) {
            umbrella_ctx.push(SeedFile {
                path: cand,
                reason: "extension metadata".to_string(),
            });
        }
    }
    seeds.push(FeatureSeed {
        title: format!("PHP extension `{ext_name}`"),
        summary: format!("PHP extension `{ext_name}` declared by {entry_config} (root-level)"),
        kind: FeatureKind::Library,
        source: "php-ext-root",
        confidence: FeatureConfidence::High,
        entry_path: entry_config.to_string(),
        entry_symbol: Some(ext_name.clone()),
        entry_route: None,
        entry_command: None,
        test_command: None,
        language: Language::Php,
        tags: vec![
            "php".to_string(),
            "php-extension".to_string(),
            "library".to_string(),
        ],
        owned_files: umbrella_owned,
        context_files: umbrella_ctx,
        tests: Vec::new(),
        test_prefixes: vec!["tests".to_string()],
    });

    // Per-module seeds: `<name>_<part>.{c,cpp,cc}` files at root.
    let prefix = format!("{ext_name}_");
    let Ok(rd) = fs::read_dir(root) else {
        return Ok(seeds);
    };
    let mut module_files: Vec<(String, String)> = Vec::new(); // (filename, stem)
    for entry in rd.flatten() {
        let p = entry.path();
        let Some(fname) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !fname.starts_with(&prefix) {
            continue;
        }
        let Some(stem) = fname
            .strip_suffix(".cpp")
            .or_else(|| fname.strip_suffix(".cc"))
            .or_else(|| fname.strip_suffix(".c"))
        else {
            continue;
        };
        // Skip generated arginfo files.
        if stem.ends_with("_arginfo") {
            continue;
        }
        if !is_safe_file(root, &p) {
            continue;
        }
        module_files.push((fname.to_string(), stem.to_string()));
    }
    module_files.sort();
    for (fname, stem) in module_files {
        let part = stem.strip_prefix(&prefix).unwrap_or(&stem).to_string();
        if part.is_empty() {
            continue;
        }
        let language = if fname.ends_with(".c") {
            Language::C
        } else {
            Language::Cpp
        };
        let mut owned = vec![SeedFile {
            path: fname.clone(),
            reason: "module source".to_string(),
        }];
        let sibling_header = format!("{stem}.h");
        if is_safe_file(root, &root.join(&sibling_header)) {
            owned.push(SeedFile {
                path: sibling_header,
                reason: "module header".to_string(),
            });
        }
        seeds.push(FeatureSeed {
            title: format!("`{ext_name}` extension module `{part}`"),
            summary: format!("PHP extension `{ext_name}` module file {fname}"),
            kind: FeatureKind::Library,
            source: "php-ext-module",
            confidence: FeatureConfidence::High,
            entry_path: fname,
            entry_symbol: Some(stem),
            entry_route: None,
            entry_command: None,
            test_command: None,
            language,
            tags: vec![
                "php".to_string(),
                "php-extension".to_string(),
                "module".to_string(),
            ],
            owned_files: owned,
            context_files: vec![SeedFile {
                path: header_path.clone(),
                reason: "extension public header".to_string(),
            }],
            tests: Vec::new(),
            test_prefixes: vec!["tests".to_string()],
        });
    }
    Ok(seeds)
}

/// Find the extension's canonical short name from its `php_<name>.h`
/// header. Preference order: a header matching the directory name (the
/// dominant convention), then the first `php_*.h` in alphabetical order.
/// Skips generated `_arginfo.h` / `_internal.h` headers.
fn detect_root_extension_name(root: &Path) -> Option<String> {
    let dir_name = root
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.trim_start_matches("php_").to_string())
        .unwrap_or_default();
    if !dir_name.is_empty() {
        let cand = root.join(format!("php_{dir_name}.h"));
        if is_safe_file(root, &cand) {
            return Some(dir_name);
        }
    }
    let rd = fs::read_dir(root).ok()?;
    let mut candidates: Vec<String> = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        let Some(fname) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !fname.starts_with("php_") || !fname.ends_with(".h") {
            continue;
        }
        if fname.contains("_arginfo") || fname.contains("_internal") {
            continue;
        }
        if !is_safe_file(root, &p) {
            continue;
        }
        if let Some(stem) = fname
            .strip_prefix("php_")
            .and_then(|s| s.strip_suffix(".h"))
        {
            candidates.push(stem.to_string());
        }
    }
    candidates.sort();
    candidates.into_iter().next()
}

/// One Laravel route registration extracted from `routes/*.php`.
/// `controller_class` is `None` when the route registers a closure or a
/// non-class-resolved target — those still produce a per-registration
/// seed but can't be bridged back to a controller file.
#[derive(Debug, Clone)]
struct LaravelRoute {
    file: String,
    verb: String,
    pattern: String,
    /// Class name as written at the call site, normalized for FQCN
    /// comparison: leading `\` stripped, namespace separators preserved
    /// (`App\Http\Controllers\UserController`). When the route source
    /// only references a short name (`UserController::class` with a
    /// `use App\Http\Controllers\UserController` import), the short
    /// name is recorded here and `laravel_controllers` falls back to
    /// matching by `class basename`.
    controller_class: Option<String>,
    action: Option<String>,
}

fn parse_laravel_routes(root: &Path) -> Result<Vec<LaravelRoute>> {
    let mut out = Vec::new();
    let routes_dir = root.join("routes");
    if !is_safe_dir(root, &routes_dir) {
        return Ok(out);
    }
    // The verb_re alternative captures verb + URI without controller
    // info — kept for closure-style and `Route::match([...])` calls. The
    // controller_re adds the `, ControllerClass::class` arm separately
    // so the existing seed shape stays intact when the controller half
    // is missing.
    let verb_re = Regex::new(
        r#"(?m)Route::(get|post|put|patch|delete|options|any|match)\s*\(\s*(?:\[[^\]]*\]\s*,\s*)?['"]([^'"]+)['"]"#,
    )?;
    let controller_re = Regex::new(
        r#"(?m)Route::(get|post|put|patch|delete|options|any|match|resource|apiResource)\s*\(\s*(?:\[[^\]]*\]\s*,\s*)?['"]([^'"]+)['"]\s*,\s*(?:\[\s*)?(\\?[A-Za-z_][A-Za-z0-9_\\]*)::class(?:\s*,\s*['"]([^'"]+)['"])?"#,
    )?;
    for file in ["web.php", "api.php", "console.php", "channels.php"] {
        let path = routes_dir.join(file);
        if !is_safe_file(root, &path) {
            continue;
        }
        let rel = rel_path(root, &path);
        let raw = fs::read_to_string(&path).unwrap_or_default();
        // Collect (verb, pattern) → (controller_class, action) from the
        // richer regex so we can attach controller info to base matches.
        let mut by_key: std::collections::HashMap<(String, String), (String, Option<String>)> =
            std::collections::HashMap::new();
        for cap in controller_re.captures_iter(&raw) {
            let verb = cap
                .get(1)
                .map(|m| m.as_str().to_uppercase())
                .unwrap_or_default();
            let pattern = cap
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let class = cap
                .get(3)
                .map(|m| m.as_str().trim_start_matches('\\').to_string())
                .unwrap_or_default();
            let action = cap.get(4).map(|m| m.as_str().to_string());
            if verb.is_empty() || pattern.is_empty() || class.is_empty() {
                continue;
            }
            by_key.insert((verb, pattern), (class, action));
        }
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
            let (controller_class, action) = match by_key.remove(&(verb.clone(), pattern.clone())) {
                Some((c, a)) => (Some(c), a),
                None => (None, None),
            };
            out.push(LaravelRoute {
                file: rel.clone(),
                verb,
                pattern,
                controller_class,
                action,
            });
        }
        // Any controller-only matches left in `by_key` had no plain
        // verb_re hit (e.g. `Route::resource` + `apiResource`); emit
        // them so resources surface as features too.
        for ((verb, pattern), (class, action)) in by_key.drain() {
            out.push(LaravelRoute {
                file: rel.clone(),
                verb,
                pattern,
                controller_class: Some(class),
                action,
            });
        }
    }
    Ok(out)
}

fn laravel_route_seeds(routes: &[LaravelRoute]) -> Vec<FeatureSeed> {
    routes
        .iter()
        .map(|r| {
            let route = format!("{} {}", r.verb, r.pattern);
            FeatureSeed {
                title: format!("Laravel route `{route}`"),
                summary: format!("Route registered in {}", r.file),
                kind: FeatureKind::Route,
                source: "laravel-route",
                confidence: FeatureConfidence::High,
                entry_path: r.file.clone(),
                entry_symbol: None,
                entry_route: Some(route),
                entry_command: None,
                test_command: None,
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
            }
        })
        .collect()
}

/// Heuristic: a project is "Laravel" if it has a `composer.json` listing
/// `laravel/framework` or it has an `artisan` script at the root. Kept
/// loose to catch real Laravel apps that don't pin the framework dep
/// directly (workspace setups, modular monoliths).
fn is_laravel_project(root: &Path) -> bool {
    if is_safe_file(root, &root.join("artisan")) {
        return true;
    }
    let Some(composer) = read_composer(root) else {
        return false;
    };
    for field in ["require", "require-dev"] {
        if let Some(map) = composer.get(field).and_then(|v| v.as_object())
            && map.contains_key("laravel/framework")
        {
            return true;
        }
    }
    false
}

/// One feature per `app/Http/Controllers/**/*.php`, bridging back to
/// the routes that hit it through `parse_laravel_routes`'s class info.
/// Trust boundaries are seeded coarsely (auth+user-input+database+
/// serialization) since every HTTP controller crosses those by default;
/// the file-level `file_trust_boundaries` table refines per-file.
fn laravel_controllers(root: &Path, routes: &[LaravelRoute]) -> Result<Vec<FeatureSeed>> {
    let controllers_dir = root.join("app/Http/Controllers");
    if !is_safe_dir(root, &controllers_dir) {
        return Ok(Vec::new());
    }
    let files = walk_php_files(root, &controllers_dir, 500);
    let mut out = Vec::with_capacity(files.len());
    for rel in files {
        let abs = root.join(&rel);
        let raw = fs::read_to_string(&abs).unwrap_or_default();
        let class_short = rel
            .rsplit('/')
            .next()
            .and_then(|f| f.strip_suffix(".php"))
            .unwrap_or("Controller")
            .to_string();
        let class_fqcn = php_declared_class_fqcn(&raw).unwrap_or_else(|| class_short.clone());
        // Match by FQCN when the route call site used a fully-qualified
        // `\App\Http\Controllers\Foo::class`; fall back to short name
        // when the route used a `use`-imported `Foo::class`.
        let owned_routes: Vec<&LaravelRoute> = routes
            .iter()
            .filter(|r| match &r.controller_class {
                Some(c) if c.contains('\\') => c == &class_fqcn,
                Some(c) => c == &class_short,
                None => false,
            })
            .collect();
        let route_summary = if owned_routes.is_empty() {
            format!("Laravel HTTP controller {class_short}.")
        } else {
            let rendered: Vec<String> = owned_routes
                .iter()
                .take(6)
                .map(|r| {
                    if let Some(a) = &r.action {
                        format!("{} {}#{}", r.verb, r.pattern, a)
                    } else {
                        format!("{} {}", r.verb, r.pattern)
                    }
                })
                .collect();
            format!(
                "Laravel HTTP controller for {} ({} routes).",
                rendered.join(", "),
                owned_routes.len()
            )
        };
        let context_files: Vec<SeedFile> = owned_routes
            .iter()
            .map(|r| SeedFile {
                path: r.file.clone(),
                reason: "route definition".to_string(),
            })
            .collect();
        let entry_route = owned_routes
            .first()
            .map(|r| format!("{} {}", r.verb, r.pattern));
        out.push(FeatureSeed {
            title: format!("Laravel controller `{class_short}`"),
            summary: route_summary,
            kind: FeatureKind::Route,
            source: "laravel-controller",
            confidence: FeatureConfidence::High,
            entry_path: rel.clone(),
            entry_symbol: Some(class_short),
            entry_route,
            entry_command: None,
            test_command: None,
            language: Language::Php,
            tags: vec![
                "php".to_string(),
                "framework:laravel".to_string(),
                "controller".to_string(),
                "http".to_string(),
            ],
            owned_files: vec![SeedFile {
                path: rel,
                reason: "controller".to_string(),
            }],
            context_files: dedup_seed_files(context_files),
            tests: Vec::new(),
            test_prefixes: vec!["tests/Feature".to_string(), "tests/Unit".to_string()],
        });
    }
    Ok(out)
}

/// `app/Http/Requests/**/*.php` — Laravel FormRequest classes carrying
/// validation rules. One feature per request class. The request *is*
/// the user-input gate, so we tag the user-input + auth boundaries
/// coarsely at the seed level (refined by file_trust_boundaries).
fn laravel_form_requests(root: &Path) -> Result<Vec<FeatureSeed>> {
    let requests_dir = root.join("app/Http/Requests");
    if !is_safe_dir(root, &requests_dir) {
        return Ok(Vec::new());
    }
    let files = walk_php_files(root, &requests_dir, 500);
    let mut out = Vec::with_capacity(files.len());
    for rel in files {
        let class_short = rel
            .rsplit('/')
            .next()
            .and_then(|f| f.strip_suffix(".php"))
            .unwrap_or("Request")
            .to_string();
        out.push(FeatureSeed {
            title: format!("Laravel request `{class_short}`"),
            summary: format!("Laravel FormRequest {class_short} in {rel}"),
            kind: FeatureKind::Route,
            source: "laravel-request",
            confidence: FeatureConfidence::Medium,
            entry_path: rel.clone(),
            entry_symbol: Some(class_short),
            entry_route: None,
            entry_command: None,
            test_command: None,
            language: Language::Php,
            tags: vec![
                "php".to_string(),
                "framework:laravel".to_string(),
                "request".to_string(),
                "validation".to_string(),
            ],
            owned_files: vec![SeedFile {
                path: rel,
                reason: "form request".to_string(),
            }],
            context_files: Vec::new(),
            tests: Vec::new(),
            test_prefixes: vec!["tests/Feature".to_string(), "tests/Unit".to_string()],
        });
    }
    Ok(out)
}

/// `app/Console/Commands/**/*.php` — Artisan command classes. We pull
/// `$signature = 'cmd:name {arg}'` out of the source so the seed's
/// `entry_command` resolves to the Artisan invocation name, not the
/// PHP class name. Falls back to class name when the signature is
/// dynamic or absent.
fn laravel_artisan_commands(root: &Path) -> Result<Vec<FeatureSeed>> {
    let commands_dir = root.join("app/Console/Commands");
    if !is_safe_dir(root, &commands_dir) {
        return Ok(Vec::new());
    }
    let signature_re =
        Regex::new(r#"\$signature\s*=\s*['"]([^'"\s{]+)"#).expect("signature regex must compile");
    let files = walk_php_files(root, &commands_dir, 500);
    let mut out = Vec::with_capacity(files.len());
    for rel in files {
        let abs = root.join(&rel);
        let raw = fs::read_to_string(&abs).unwrap_or_default();
        let class_short = rel
            .rsplit('/')
            .next()
            .and_then(|f| f.strip_suffix(".php"))
            .unwrap_or("Command")
            .to_string();
        let signature = signature_re
            .captures(&raw)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let cmd_name = signature.clone().unwrap_or_else(|| class_short.clone());
        out.push(FeatureSeed {
            title: format!("Laravel command `{cmd_name}`"),
            summary: match &signature {
                Some(s) => format!("Laravel Artisan command '{s}' in {rel}"),
                None => format!("Laravel Artisan command {class_short}."),
            },
            kind: FeatureKind::CliCommand,
            source: "laravel-artisan-command",
            confidence: FeatureConfidence::High,
            entry_path: rel.clone(),
            entry_symbol: Some(class_short),
            entry_route: None,
            entry_command: Some(cmd_name),
            test_command: None,
            language: Language::Php,
            tags: vec![
                "php".to_string(),
                "framework:laravel".to_string(),
                "artisan".to_string(),
                "cli".to_string(),
            ],
            owned_files: vec![SeedFile {
                path: rel,
                reason: "Artisan command".to_string(),
            }],
            context_files: Vec::new(),
            tests: Vec::new(),
            test_prefixes: vec!["tests/Feature".to_string(), "tests/Unit".to_string()],
        });
    }
    Ok(out)
}

fn php_declared_class_fqcn(source: &str) -> Option<String> {
    let namespace_re = Regex::new(r"(?m)^\s*namespace\s+([A-Za-z_\\][A-Za-z0-9_\\]*)\s*;").ok()?;
    let class_re =
        Regex::new(r"(?m)^\s*(?:abstract\s+|final\s+)?class\s+([A-Za-z_][A-Za-z0-9_]*)").ok()?;
    let ns = namespace_re.captures(source)?.get(1)?.as_str().to_string();
    let class = class_re.captures(source)?.get(1)?.as_str().to_string();
    Some(format!("{ns}\\{class}"))
}

fn walk_php_files(root: &Path, dir: &Path, max: usize) -> Vec<String> {
    fn recurse(root: &Path, dir: &Path, max: usize, out: &mut Vec<String>) {
        if out.len() >= max {
            return;
        }
        let Ok(rd) = fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            if out.len() >= max {
                return;
            }
            let p = entry.path();
            if let Ok(meta) = fs::symlink_metadata(&p) {
                if meta.file_type().is_symlink() {
                    continue;
                }
                if meta.is_dir() {
                    recurse(root, &p, max, out);
                } else if meta.is_file()
                    && p.extension().and_then(|s| s.to_str()) == Some("php")
                    && is_safe_file(root, &p)
                {
                    out.push(rel_path(root, &p));
                }
            }
        }
    }
    let mut out = Vec::new();
    recurse(root, dir, max, &mut out);
    out.sort();
    out
}

fn dedup_seed_files(mut v: Vec<SeedFile>) -> Vec<SeedFile> {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();
    v.retain(|f| seen.insert(f.path.clone()));
    v
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
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
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
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
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
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
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
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
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
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let routes: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "laravel-route")
            .filter_map(|s| s.entry_route.as_deref())
            .collect();
        assert!(routes.iter().any(|r| r.starts_with("GET ")));
        assert!(routes.contains(&"POST /api/login"));
    }

    #[test]
    fn laravel_controller_seed_resolves_back_to_routes() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "composer.json",
            r#"{"name":"acme/app","require":{"laravel/framework":"^11.0"}}"#,
        );
        write(
            dir.path(),
            "routes/web.php",
            r#"<?php
use App\Http\Controllers\UserController;
Route::get('/users', [UserController::class, 'index']);
Route::post('/users', [UserController::class, 'store']);
"#,
        );
        write(
            dir.path(),
            "app/Http/Controllers/UserController.php",
            r#"<?php
namespace App\Http\Controllers;
class UserController { public function index() {} public function store() {} }
"#,
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let c = seeds
            .iter()
            .find(|s| s.source == "laravel-controller")
            .expect("expected laravel-controller seed");
        assert_eq!(c.entry_path, "app/Http/Controllers/UserController.php");
        assert_eq!(c.entry_symbol.as_deref(), Some("UserController"));
        assert!(
            c.summary.contains("GET /users") || c.summary.contains("POST /users"),
            "controller summary should mention bridged routes, got {}",
            c.summary
        );
        let ctx_paths: Vec<&str> = c.context_files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            ctx_paths.contains(&"routes/web.php"),
            "expected routes/web.php in context, got {ctx_paths:?}"
        );
    }

    #[test]
    fn laravel_form_request_emitted() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "composer.json",
            r#"{"name":"acme/app","require":{"laravel/framework":"^11.0"}}"#,
        );
        write(
            dir.path(),
            "app/Http/Requests/StoreUserRequest.php",
            r#"<?php
namespace App\Http\Requests;
class StoreUserRequest { public function rules() { return []; } }
"#,
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let r = seeds
            .iter()
            .find(|s| s.source == "laravel-request")
            .expect("expected laravel-request seed");
        assert_eq!(r.entry_path, "app/Http/Requests/StoreUserRequest.php");
        assert_eq!(r.entry_symbol.as_deref(), Some("StoreUserRequest"));
    }

    #[test]
    fn laravel_artisan_command_extracts_signature() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "composer.json",
            r#"{"name":"acme/app","require":{"laravel/framework":"^11.0"}}"#,
        );
        write(dir.path(), "artisan", "#!/usr/bin/env php\n");
        write(
            dir.path(),
            "app/Console/Commands/SyncUsers.php",
            r#"<?php
namespace App\Console\Commands;
use Illuminate\Console\Command;
class SyncUsers extends Command {
    protected $signature = 'users:sync {--force}';
    public function handle() {}
}
"#,
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let c = seeds
            .iter()
            .find(|s| s.source == "laravel-artisan-command")
            .expect("expected laravel-artisan-command seed");
        assert_eq!(c.entry_command.as_deref(), Some("users:sync"));
        assert_eq!(c.entry_path, "app/Console/Commands/SyncUsers.php");
    }

    #[test]
    fn laravel_application_layer_skipped_for_non_laravel_projects() {
        // A repo with `app/Http/Controllers/*.php` but no Laravel
        // dependency must not produce laravel-controller seeds.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "composer.json",
            r#"{"name":"acme/app","require":{"symfony/console":"^7.0"}}"#,
        );
        write(
            dir.path(),
            "app/Http/Controllers/Whatever.php",
            "<?php namespace App\\Http\\Controllers; class Whatever {}",
        );
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(
            !seeds.iter().any(|s| s.source == "laravel-controller"),
            "non-Laravel project must not emit laravel-controller seeds"
        );
    }

    #[test]
    fn php_ext_root_emits_umbrella_and_per_module_seeds() {
        // Mirrors the fastchart layout: config.m4 + php_<name>.h + main
        // <name>.c + several <name>_<part>.c files (one per module).
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "config.m4",
            "PHP_NEW_EXTENSION(acme, acme.c, $ext_shared)\n",
        );
        write(
            dir.path(),
            "php_acme.h",
            "#ifndef PHP_ACME_H\n#define PHP_ACME_H\n#endif\n",
        );
        write(dir.path(), "acme.c", "/* main module */\n");
        write(dir.path(), "acme_pie.c", "/* pie chart */\n");
        write(dir.path(), "acme_pie.h", "/* pie header */\n");
        write(dir.path(), "acme_bar.c", "/* bar chart */\n");
        write(dir.path(), "composer.json", r#"{"name":"acme/ext"}"#);
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();

        // Umbrella seed.
        let umbrella = seeds
            .iter()
            .find(|s| s.source == "php-ext-root")
            .expect("expected php-ext-root umbrella seed");
        assert_eq!(umbrella.entry_path, "config.m4");
        assert_eq!(umbrella.entry_symbol.as_deref(), Some("acme"));
        let owned_paths: Vec<&str> = umbrella
            .owned_files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert!(
            owned_paths.contains(&"acme.c"),
            "umbrella should own main module .c"
        );
        assert!(
            owned_paths.contains(&"php_acme.h"),
            "umbrella should own public header"
        );

        // Per-module seeds.
        let modules: Vec<&FeatureSeed> = seeds
            .iter()
            .filter(|s| s.source == "php-ext-module")
            .collect();
        let module_entries: Vec<&str> = modules.iter().map(|s| s.entry_path.as_str()).collect();
        assert!(
            module_entries.contains(&"acme_pie.c"),
            "expected acme_pie.c module seed"
        );
        assert!(
            module_entries.contains(&"acme_bar.c"),
            "expected acme_bar.c module seed"
        );

        // Sibling header attached when present.
        let pie_seed = modules
            .iter()
            .find(|s| s.entry_path == "acme_pie.c")
            .unwrap();
        let pie_owned: Vec<&str> = pie_seed
            .owned_files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert!(
            pie_owned.contains(&"acme_pie.h"),
            "expected sibling header acme_pie.h owned by acme_pie.c seed, got {pie_owned:?}"
        );

        // Module without a sibling header (acme_bar.h doesn't exist).
        let bar_seed = modules
            .iter()
            .find(|s| s.entry_path == "acme_bar.c")
            .unwrap();
        assert_eq!(
            bar_seed.owned_files.len(),
            1,
            "no sibling header for acme_bar"
        );
    }

    #[test]
    fn php_ext_root_handles_cpp_extension() {
        // Mirrors the php_clickhouse layout: config.m4 + php_<name>.h + a
        // main .cpp, no per-module split. Only the umbrella seed fires.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "config.m4",
            "PHP_NEW_EXTENSION(clickhouse, clickhouse.cpp)\n",
        );
        write(
            dir.path(),
            "php_clickhouse.h",
            "#ifndef PHP_CLICKHOUSE_H\n#endif\n",
        );
        write(dir.path(), "clickhouse.cpp", "/* main */\n");
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let umbrella = seeds
            .iter()
            .find(|s| s.source == "php-ext-root")
            .expect("expected umbrella");
        let owned: Vec<&str> = umbrella
            .owned_files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert!(
            owned.contains(&"clickhouse.cpp"),
            "expected clickhouse.cpp owned"
        );
        assert!(
            !seeds.iter().any(|s| s.source == "php-ext-module"),
            "no per-module seeds when no <name>_*.{{c,cpp}} files exist"
        );
    }

    #[test]
    fn php_ext_root_skips_when_no_extension_header() {
        // `config.m4` exists but `php_<name>.h` does not — generic
        // autotools project, must not fire as a PHP extension.
        let dir = tempdir().unwrap();
        write(dir.path(), "config.m4", "AC_INIT([foo], [1.0])\n");
        write(dir.path(), "foo.c", "int main(){return 0;}\n");
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        assert!(
            !seeds
                .iter()
                .any(|s| matches!(s.source, "php-ext-root" | "php-ext-module")),
            "must not fire on a non-extension autotools repo"
        );
    }

    #[test]
    fn php_ext_root_skips_generated_arginfo_modules() {
        // Generated `<name>_arginfo.h` (and an unlikely `<name>_arginfo.c`)
        // should not turn into per-module features — they're machine-
        // generated and not what an agent reasons about.
        let dir = tempdir().unwrap();
        write(dir.path(), "config.m4", "PHP_NEW_EXTENSION(acme, acme.c)\n");
        write(dir.path(), "php_acme.h", "#endif\n");
        write(dir.path(), "acme.c", "/* main */\n");
        write(dir.path(), "acme_arginfo.h", "/* generated */\n");
        write(dir.path(), "acme_real.c", "/* real module */\n");
        let seeds = PhpMapper.map(&MapperContext::for_root(dir.path())).unwrap();
        let module_entries: Vec<&str> = seeds
            .iter()
            .filter(|s| s.source == "php-ext-module")
            .map(|s| s.entry_path.as_str())
            .collect();
        assert_eq!(module_entries, vec!["acme_real.c"]);
    }
}
